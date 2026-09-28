use crate::error::DownloadError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartState {
    pub part_index: usize,
    pub start_byte: u64,
    pub end_byte: u64,
    pub downloaded_bytes: u64,
    pub is_completed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadState {
    pub url: String,
    pub output_path: PathBuf,
    pub total_size: u64,
    pub parts: Vec<PartState>,
}

impl DownloadState {
    pub fn state_file_path(output_path: &Path) -> PathBuf {
        let mut path = output_path.to_path_buf();
        let filename = path.file_name().unwrap_or_default().to_string_lossy();
        path.set_file_name(format!("{}.oxidl.json", filename));
        path
    }

    pub async fn load_from_file(output_path: &Path) -> Option<Self> {
        let state_path = Self::state_file_path(output_path);
        if !state_path.exists() {
            return None;
        }
        let data = tokio::fs::read_to_string(state_path).await.ok()?;
        serde_json::from_str(&data).ok()
    }

    pub async fn save_to_file(&self) -> Result<(), std::io::Error> {
        let state_path = Self::state_file_path(&self.output_path);
        let json = serde_json::to_string_pretty(self)?;
        tokio::fs::write(state_path, json).await
    }

    pub async fn remove_state_file(&self) {
        let state_path = Self::state_file_path(&self.output_path);
        let _ = tokio::fs::remove_file(state_path).await;
    }
}

#[derive(Debug, Clone)]
pub struct ProgressEvent {
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub percentage: f64,
    pub bytes_per_sec: u64,
}

pub struct DownloadTask {
    pub url: String,
    pub output_path: PathBuf,
    pub num_parts: usize,
    pub cancel_token: CancellationToken,
}

impl DownloadTask {
    pub fn new(url: impl Into<String>, output_path: impl Into<PathBuf>, num_parts: usize) -> Self {
        Self {
            url: url.into(),
            output_path: output_path.into(),
            num_parts,
            cancel_token: CancellationToken::new(),
        }
    }

    pub fn cancel_handle(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    pub async fn start_with_progress(
        &self,
        tx: mpsc::Sender<ProgressEvent>,
    ) -> Result<(), DownloadError> {
        let client = reqwest::Client::new();

        let state = match DownloadState::load_from_file(&self.output_path).await {
            Some(existing_state) if existing_state.url == self.url => existing_state,
            _ => {
                let resp = client.head(&self.url).send().await?;
                let total_size = resp
                    .headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|val| val.to_str().ok())
                    .and_then(|val| val.parse::<u64>().ok())
                    .unwrap_or(0);

                let part_size = if total_size > 0 {
                    total_size / self.num_parts as u64
                } else {
                    0
                };

                let mut parts = Vec::new();
                for i in 0..self.num_parts {
                    let start_byte = i as u64 * part_size;
                    let end_byte = if i == self.num_parts - 1 {
                        total_size.saturating_sub(1)
                    } else {
                        (i as u64 + 1) * part_size - 1
                    };

                    parts.push(PartState {
                        part_index: i,
                        start_byte,
                        end_byte,
                        downloaded_bytes: 0,
                        is_completed: false,
                    });
                }

                let new_state = DownloadState {
                    url: self.url.clone(),
                    output_path: self.output_path.clone(),
                    total_size,
                    parts,
                };
                new_state.save_to_file().await?;
                new_state
            }
        };

        if !self.output_path.exists() {
            let file = File::create(&self.output_path).await?;
            if state.total_size > 0 {
                file.set_len(state.total_size).await?;
            }
        }

        let state_arc = Arc::new(tokio::sync::Mutex::new(state));
        let (part_tx, mut part_rx) = mpsc::channel::<(usize, u64)>(100);

        let mut worker_handles: Vec<tokio::task::JoinHandle<Result<(), DownloadError>>> =
            Vec::new();

        for i in 0..self.num_parts {
            let part = {
                let s = state_arc.lock().await;
                s.parts[i].clone()
            };

            if part.is_completed {
                continue;
            }

            let client_clone = client.clone();
            let url_clone = self.url.clone();
            let file_path = self.output_path.clone();
            let token = self.cancel_token.clone();
            let p_tx = part_tx.clone();

            let handle = tokio::spawn(async move {
                let resume_start = part.start_byte + part.downloaded_bytes;
                if resume_start > part.end_byte && part.end_byte > 0 {
                    return Ok(());
                }

                let mut req = client_clone.get(&url_clone);
                if part.end_byte > 0 {
                    req = req.header(
                        reqwest::header::RANGE,
                        format!("bytes={}-{}", resume_start, part.end_byte),
                    );
                }

                let mut res = req.send().await?;
                let mut file = OpenOptions::new()
                    .write(true)
                    .open(&file_path)
                    .await?;

                file.seek(SeekFrom::Start(resume_start)).await?;

                while let Some(chunk) = tokio::select! {
                    _ = token.cancelled() => None,
                    res_chunk = res.chunk() => res_chunk?,
                } {
                    file.write_all(&chunk).await?;
                    let chunk_len = chunk.len() as u64;
                    let _ = p_tx.send((i, chunk_len)).await;
                }

                file.flush().await?;
                Ok(())
            });

            worker_handles.push(handle);
        }

        drop(part_tx);

        let state_tracker = state_arc.clone();

        let progress_handle = tokio::spawn(async move {
            let start_time = std::time::Instant::now();
            let mut last_save = std::time::Instant::now();

            while let Some((part_idx, bytes_written)) = part_rx.recv().await {
                let mut s = state_tracker.lock().await;
                s.parts[part_idx].downloaded_bytes += bytes_written;

                if s.parts[part_idx].end_byte > 0
                    && s.parts[part_idx].start_byte + s.parts[part_idx].downloaded_bytes
                        >= s.parts[part_idx].end_byte
                {
                    s.parts[part_idx].is_completed = true;
                }

                let total_downloaded: u64 = s.parts.iter().map(|p| p.downloaded_bytes).sum();
                let elapsed = start_time.elapsed().as_secs();
                let speed = if elapsed > 0 {
                    total_downloaded / elapsed
                } else {
                    0
                };
                let percentage = if s.total_size > 0 {
                    (total_downloaded as f64 / s.total_size as f64) * 100.0
                } else {
                    0.0
                };

                let _ = tx
                    .send(ProgressEvent {
                        downloaded_bytes: total_downloaded,
                        total_bytes: s.total_size,
                        percentage,
                        bytes_per_sec: speed,
                    })
                    .await;

                if last_save.elapsed().as_millis() > 500 {
                    let _ = s.save_to_file().await;
                    last_save = std::time::Instant::now();
                }
            }

            let s = state_tracker.lock().await;
            let _ = s.save_to_file().await;
        });

        for handle in worker_handles {
            let _ = handle.await;
        }

        let _ = progress_handle.await;

        if self.cancel_token.is_cancelled() {
            return Err(DownloadError::Paused);
        }

        let final_state = state_arc.lock().await;
        final_state.remove_state_file().await;

        Ok(())
    }
}