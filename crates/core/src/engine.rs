use crate::error::DownloadError;
use crate::util::validate_url;
use bytes::Bytes;
use reqwest::{header, Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// A part is never smaller than this, so small files use fewer connections.
const MIN_PART_SIZE: u64 = 1024 * 1024;
const STATE_SAVE_INTERVAL: Duration = Duration::from_millis(500);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
const FINAL_EVENT_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartState {
    pub part_index: usize,
    /// First byte of the part (inclusive).
    pub start_byte: u64,
    /// Last byte of the part (inclusive).
    pub end_byte: u64,
    pub downloaded_bytes: u64,
}

impl PartState {
    pub fn len(&self) -> u64 {
        self.end_byte - self.start_byte + 1
    }

    pub fn is_completed(&self) -> bool {
        self.downloaded_bytes >= self.len()
    }

    fn next_byte(&self) -> u64 {
        self.start_byte + self.downloaded_bytes
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadState {
    pub url: String,
    pub output_path: PathBuf,
    pub total_size: u64,
    /// ETag or Last-Modified of the remote file, used to detect changes between sessions.
    #[serde(default)]
    pub validator: Option<String>,
    pub parts: Vec<PartState>,
}

impl DownloadState {
    fn new(
        url: String,
        output_path: PathBuf,
        total_size: u64,
        validator: Option<String>,
        max_parts: usize,
    ) -> Self {
        Self {
            url,
            output_path,
            total_size,
            validator,
            parts: plan_parts(total_size, max_parts),
        }
    }

    pub fn state_file_path(output_path: &Path) -> PathBuf {
        let mut path = output_path.to_path_buf();
        let filename = path.file_name().unwrap_or_default().to_string_lossy();
        path.set_file_name(format!("{}.oxidl.json", filename));
        path
    }

    pub async fn load_from_file(output_path: &Path) -> Option<Self> {
        let data = tokio::fs::read_to_string(Self::state_file_path(output_path))
            .await
            .ok()?;
        serde_json::from_str(&data).ok()
    }

    /// Writes to a temporary file first, so a crash never leaves a half-written state file.
    pub async fn save_to_file(&self) -> Result<(), std::io::Error> {
        let state_path = Self::state_file_path(&self.output_path);
        let tmp_path = state_path.with_extension("tmp");
        let json = serde_json::to_string_pretty(self)?;
        tokio::fs::write(&tmp_path, json).await?;
        tokio::fs::rename(&tmp_path, &state_path).await
    }

    pub async fn remove_state_file(output_path: &Path) {
        let _ = tokio::fs::remove_file(Self::state_file_path(output_path)).await;
    }

    pub fn downloaded_total(&self) -> u64 {
        self.parts.iter().map(|p| p.downloaded_bytes).sum()
    }
}

/// Splits `total_size` (must be greater than zero) into contiguous inclusive byte ranges.
fn plan_parts(total_size: u64, max_parts: usize) -> Vec<PartState> {
    let count = ((total_size / MIN_PART_SIZE).max(1) as usize).min(max_parts.max(1));
    let base = total_size / count as u64;
    (0..count)
        .map(|i| {
            let start = i as u64 * base;
            let end = if i == count - 1 {
                total_size - 1
            } else {
                start + base - 1
            };
            PartState {
                part_index: i,
                start_byte: start,
                end_byte: end,
                downloaded_bytes: 0,
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct ProgressEvent {
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub percentage: f64,
    pub bytes_per_sec: u64,
}

fn make_event(downloaded: u64, total: u64, speed: u64) -> ProgressEvent {
    let percentage = if total > 0 {
        downloaded as f64 / total as f64 * 100.0
    } else {
        0.0
    };
    ProgressEvent {
        downloaded_bytes: downloaded,
        total_bytes: total,
        percentage,
        bytes_per_sec: speed,
    }
}

/// Smoothed transfer speed, measured over half-second windows of the current session.
struct SpeedMeter {
    window_start: Instant,
    window_bytes: u64,
    speed: f64,
}

impl SpeedMeter {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            window_bytes: 0,
            speed: 0.0,
        }
    }

    fn record(&mut self, bytes: u64) -> u64 {
        self.window_bytes += bytes;
        let elapsed = self.window_start.elapsed().as_secs_f64();
        if elapsed >= 0.5 {
            let instant = self.window_bytes as f64 / elapsed;
            self.speed = if self.speed == 0.0 {
                instant
            } else {
                0.7 * self.speed + 0.3 * instant
            };
            self.window_start = Instant::now();
            self.window_bytes = 0;
        }
        self.speed as u64
    }
}

struct Probe {
    total_size: Option<u64>,
    accepts_ranges: bool,
    validator: Option<String>,
}

fn parse_content_range_total(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse().ok()
}

enum Next {
    Chunk(Bytes),
    Eof,
    Cancelled,
}

/// Reads the next body chunk. Cancellation and stalls are both detected while waiting.
async fn next_chunk(res: &mut Response, token: &CancellationToken) -> Result<Next, DownloadError> {
    tokio::select! {
        _ = token.cancelled() => Ok(Next::Cancelled),
        read = tokio::time::timeout(STALL_TIMEOUT, res.chunk()) => match read {
            Err(_) => Err(DownloadError::Timeout),
            Ok(Ok(Some(bytes))) => Ok(Next::Chunk(bytes)),
            Ok(Ok(None)) => Ok(Next::Eof),
            Ok(Err(e)) => Err(e.into()),
        },
    }
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

    /// Replaces the task's cancellation token with one owned by the caller.
    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = token;
        self
    }

    pub fn cancel_handle(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Runs the download. The receiver of `tx` has to keep draining events.
    /// Returns `Err(DownloadError::Paused)` after cancellation; call again with the
    /// same output path to resume when the server supports byte ranges.
    pub async fn start_with_progress(
        &self,
        tx: mpsc::Sender<ProgressEvent>,
    ) -> Result<(), DownloadError> {
        validate_url(&self.url)?;
        let client = Client::builder().connect_timeout(CONNECT_TIMEOUT).build()?;

        let probe = self.probe(&client).await?;
        match probe {
            Probe {
                total_size: Some(total),
                accepts_ranges: true,
                validator,
            } if total > 0 => self.download_multipart(&client, tx, total, validator).await,
            _ => self.download_single(&client, tx).await,
        }
    }

    /// Asks for the first byte only. A 206 answer proves range support and reveals the total size.
    async fn probe(&self, client: &Client) -> Result<Probe, DownloadError> {
        let request = client.get(&self.url).header(header::RANGE, "bytes=0-0").send();
        let sent = tokio::select! {
            _ = self.cancel_token.cancelled() => return Err(DownloadError::Paused),
            r = tokio::time::timeout(STALL_TIMEOUT, request) => r,
        };
        let resp = sent.map_err(|_| DownloadError::Timeout)??;

        let status = resp.status();
        let headers = resp.headers();
        let validator = headers
            .get(header::ETAG)
            .or_else(|| headers.get(header::LAST_MODIFIED))
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        match status {
            StatusCode::PARTIAL_CONTENT => {
                let total = headers
                    .get(header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_content_range_total);
                Ok(Probe {
                    total_size: total,
                    accepts_ranges: total.is_some(),
                    validator,
                })
            }
            // An empty resource cannot satisfy "bytes=0-0".
            StatusCode::RANGE_NOT_SATISFIABLE => Ok(Probe {
                total_size: Some(0),
                accepts_ranges: false,
                validator,
            }),
            s if s.is_success() => Ok(Probe {
                total_size: resp.content_length(),
                accepts_ranges: false,
                validator,
            }),
            s => Err(DownloadError::HttpStatus(s.as_u16())),
        }
    }

    /// Reuses the saved state when it still describes the same remote file and the
    /// local file is intact. Otherwise starts over with a fresh, pre-sized file.
    async fn prepare_state(
        &self,
        total_size: u64,
        validator: Option<String>,
    ) -> Result<DownloadState, DownloadError> {
        if let Some(existing) = DownloadState::load_from_file(&self.output_path).await {
            let file_len = tokio::fs::metadata(&self.output_path)
                .await
                .ok()
                .map(|m| m.len());
            if existing.url == self.url
                && existing.total_size == total_size
                && existing.validator == validator
                && file_len == Some(total_size)
            {
                return Ok(existing);
            }
        }

        let file = File::create(&self.output_path).await?;
        file.set_len(total_size).await?;
        let state = DownloadState::new(
            self.url.clone(),
            self.output_path.clone(),
            total_size,
            validator,
            self.num_parts,
        );
        state.save_to_file().await?;
        Ok(state)
    }

    async fn download_multipart(
        &self,
        client: &Client,
        tx: mpsc::Sender<ProgressEvent>,
        total_size: u64,
        validator: Option<String>,
    ) -> Result<(), DownloadError> {
        let state = self.prepare_state(total_size, validator).await?;

        // Workers use a child token, so one failing worker can stop its siblings
        // while a user cancel still reaches every worker.
        let run_token = self.cancel_token.child_token();
        let (part_tx, mut part_rx) = mpsc::channel::<(usize, u64)>(256);

        let mut workers: JoinSet<Result<(), DownloadError>> = JoinSet::new();
        for part in state.parts.iter().filter(|p| !p.is_completed()).cloned() {
            workers.spawn(download_part(
                client.clone(),
                self.url.clone(),
                self.output_path.clone(),
                part,
                run_token.clone(),
                part_tx.clone(),
            ));
        }
        drop(part_tx);

        // The aggregator owns the state, so no lock is held while sending or saving.
        let aggregator = tokio::spawn(async move {
            let mut state = state;
            let total = state.total_size;
            let mut meter = SpeedMeter::new();
            let mut last_save = Instant::now();

            let _ = tx.try_send(make_event(state.downloaded_total(), total, 0));

            let mut speed = 0;
            while let Some((index, written)) = part_rx.recv().await {
                state.parts[index].downloaded_bytes += written;
                speed = meter.record(written);
                let _ = tx.try_send(make_event(state.downloaded_total(), total, speed));

                if last_save.elapsed() >= STATE_SAVE_INTERVAL {
                    let _ = state.save_to_file().await;
                    last_save = Instant::now();
                }
            }

            let _ = state.save_to_file().await;
            let _ = tokio::time::timeout(
                FINAL_EVENT_TIMEOUT,
                tx.send(make_event(state.downloaded_total(), total, speed)),
            )
            .await;
            state
        });

        let mut first_error: Option<DownloadError> = None;
        while let Some(joined) = workers.join_next().await {
            let result = match joined {
                Ok(result) => result,
                Err(join_err) => Err(DownloadError::Worker(join_err.to_string())),
            };
            if let Err(err) = result {
                first_error.get_or_insert(err);
                run_token.cancel();
            }
        }

        let state = aggregator
            .await
            .map_err(|e| DownloadError::Worker(e.to_string()))?;

        if let Some(err) = first_error {
            return Err(err);
        }
        if self.cancel_token.is_cancelled() {
            return Err(DownloadError::Paused);
        }
        if !state.parts.iter().all(|p| p.is_completed()) {
            return Err(DownloadError::Incomplete);
        }
        let on_disk = tokio::fs::metadata(&self.output_path).await?.len();
        if on_disk != state.total_size {
            return Err(DownloadError::Incomplete);
        }

        DownloadState::remove_state_file(&self.output_path).await;
        Ok(())
    }

    /// Fallback for servers without range support or without a known size.
    /// The transfer cannot resume, so a paused download starts again from zero.
    async fn download_single(
        &self,
        client: &Client,
        tx: mpsc::Sender<ProgressEvent>,
    ) -> Result<(), DownloadError> {
        DownloadState::remove_state_file(&self.output_path).await;

        let request = client.get(&self.url).send();
        let sent = tokio::select! {
            _ = self.cancel_token.cancelled() => return Err(DownloadError::Paused),
            r = tokio::time::timeout(STALL_TIMEOUT, request) => r,
        };
        let mut res = sent.map_err(|_| DownloadError::Timeout)??;
        if !res.status().is_success() {
            return Err(DownloadError::HttpStatus(res.status().as_u16()));
        }

        let total = res.content_length().unwrap_or(0);
        let mut file = File::create(&self.output_path).await?;
        let mut downloaded = 0u64;
        let mut meter = SpeedMeter::new();
        let mut speed = 0;

        loop {
            match next_chunk(&mut res, &self.cancel_token).await? {
                Next::Cancelled => {
                    file.flush().await?;
                    return Err(DownloadError::Paused);
                }
                Next::Eof => break,
                Next::Chunk(chunk) => {
                    file.write_all(&chunk).await?;
                    downloaded += chunk.len() as u64;
                    speed = meter.record(chunk.len() as u64);
                    let _ = tx.try_send(make_event(downloaded, total, speed));
                }
            }
        }

        file.flush().await?;
        let _ = tokio::time::timeout(
            FINAL_EVENT_TIMEOUT,
            tx.send(make_event(downloaded, total, speed)),
        )
        .await;

        if total > 0 && downloaded != total {
            return Err(DownloadError::Incomplete);
        }
        Ok(())
    }
}

/// Downloads one byte range into its slot of the pre-sized output file.
/// A cancelled token ends the worker with `Ok`; the caller inspects the token.
async fn download_part(
    client: Client,
    url: String,
    path: PathBuf,
    part: PartState,
    token: CancellationToken,
    tx: mpsc::Sender<(usize, u64)>,
) -> Result<(), DownloadError> {
    let start = part.next_byte();
    if start > part.end_byte {
        return Ok(());
    }

    let request = client
        .get(&url)
        .header(header::RANGE, format!("bytes={}-{}", start, part.end_byte))
        .send();
    let sent = tokio::select! {
        _ = token.cancelled() => return Ok(()),
        r = tokio::time::timeout(STALL_TIMEOUT, request) => r,
    };
    let mut res = sent.map_err(|_| DownloadError::Timeout)??;

    match res.status() {
        StatusCode::PARTIAL_CONTENT => {}
        StatusCode::OK => return Err(DownloadError::RangeNotSupported),
        other => return Err(DownloadError::HttpStatus(other.as_u16())),
    }

    let mut file = OpenOptions::new().write(true).open(&path).await?;
    file.seek(SeekFrom::Start(start)).await?;

    let mut remaining = part.end_byte - start + 1;
    loop {
        match next_chunk(&mut res, &token).await? {
            Next::Cancelled | Next::Eof => break,
            Next::Chunk(mut chunk) => {
                if chunk.len() as u64 > remaining {
                    chunk.truncate(remaining as usize);
                }
                file.write_all(&chunk).await?;
                remaining -= chunk.len() as u64;
                let _ = tx.send((part.part_index, chunk.len() as u64)).await;
                if remaining == 0 {
                    break;
                }
            }
        }
    }
    file.flush().await?;

    if remaining > 0 && !token.is_cancelled() {
        return Err(DownloadError::Incomplete);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_cover_the_file_without_gaps_or_overlap() {
        for (total, max) in [
            (1u64, 8usize),
            (10, 8),
            (5 * MIN_PART_SIZE + 7, 8),
            (100 * MIN_PART_SIZE, 8),
        ] {
            let parts = plan_parts(total, max);
            assert!(!parts.is_empty() && parts.len() <= max);
            assert_eq!(parts[0].start_byte, 0);
            assert_eq!(parts.last().unwrap().end_byte, total - 1);
            for pair in parts.windows(2) {
                assert_eq!(pair[0].end_byte + 1, pair[1].start_byte);
            }
            assert_eq!(parts.iter().map(|p| p.len()).sum::<u64>(), total);
        }
    }

    #[test]
    fn small_files_use_a_single_part() {
        assert_eq!(plan_parts(MIN_PART_SIZE - 1, 8).len(), 1);
    }

    #[test]
    fn completion_uses_inclusive_end_byte() {
        let mut part = PartState {
            part_index: 0,
            start_byte: 100,
            end_byte: 199,
            downloaded_bytes: 99,
        };
        assert!(!part.is_completed());
        part.downloaded_bytes = 100;
        assert!(part.is_completed());
    }

    #[test]
    fn content_range_total_is_parsed() {
        assert_eq!(parse_content_range_total("bytes 0-0/12345"), Some(12345));
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
    }
}
