use oxidl_core::{DownloadError, DownloadState, DownloadTask, ProgressEvent};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ranges,
    NoRanges,
    NotFound,
    /// Advertises the full length of a range but closes the socket halfway.
    Truncate,
}

#[derive(Clone)]
struct Config {
    mode: Mode,
    /// Pause after every 16 KiB slice of a response body.
    slice_delay: Duration,
}

fn test_data(len: usize) -> Arc<Vec<u8>> {
    Arc::new((0..len).map(|i| ((i * 31 + i / 251) % 251) as u8).collect())
}

async fn spawn_server(data: Arc<Vec<u8>>, config: Config) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let data = data.clone();
            let config = config.clone();
            tokio::spawn(async move {
                let _ = handle(stream, data, config).await;
            });
        }
    });
    addr
}

async fn handle(mut stream: TcpStream, data: Arc<Vec<u8>>, config: Config) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let range = head.lines().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        let value = lower.strip_prefix("range: bytes=")?.to_string();
        let (a, b) = value.split_once('-')?;
        Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
    });

    let len = data.len();
    if config.mode == Mode::NotFound {
        stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope")
            .await?;
        return Ok(());
    }

    let (status_line, extra, body_start, body_end) = match (config.mode, range) {
        (Mode::NoRanges, _) | (_, None) => ("HTTP/1.1 200 OK", String::new(), 0, len),
        (_, Some((a, b))) => {
            let b = b.min(len - 1);
            (
                "HTTP/1.1 206 Partial Content",
                format!("Content-Range: bytes {a}-{b}/{len}\r\nAccept-Ranges: bytes\r\n"),
                a,
                b + 1,
            )
        }
    };

    let body = &data[body_start..body_end];
    let header = format!(
        "{status_line}\r\n{extra}ETag: \"v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;

    let send_len = if config.mode == Mode::Truncate && body.len() > 1 {
        body.len() / 2
    } else {
        body.len()
    };
    for slice in body[..send_len].chunks(16 * 1024) {
        stream.write_all(slice).await?;
        if !config.slice_delay.is_zero() {
            tokio::time::sleep(config.slice_delay).await;
        }
    }
    stream.shutdown().await
}

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxidl-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(DownloadState::state_file_path(path));
}

/// Runs a task to completion while draining progress events.
fn drain() -> (mpsc::Sender<ProgressEvent>, tokio::task::JoinHandle<Vec<ProgressEvent>>) {
    let (tx, mut rx) = mpsc::channel::<ProgressEvent>(100);
    let handle = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        events
    });
    (tx, handle)
}

fn fast() -> Config {
    Config {
        mode: Mode::Ranges,
        slice_delay: Duration::ZERO,
    }
}

#[tokio::test]
async fn multipart_download_is_byte_identical() {
    let data = test_data(6 * 1024 * 1024 + 123);
    let addr = spawn_server(data.clone(), fast()).await;
    let out = temp_path("multipart.bin");
    cleanup(&out);

    let task = DownloadTask::new(format!("http://{addr}/file.bin"), &out, 4);
    let (tx, events) = drain();
    task.start_with_progress(tx).await.unwrap();
    let events = events.await.unwrap();

    assert_eq!(std::fs::read(&out).unwrap(), *data);
    assert!(!DownloadState::state_file_path(&out).exists());
    let last = events.last().unwrap();
    assert_eq!(last.downloaded_bytes, data.len() as u64);
    assert_eq!(last.total_bytes, data.len() as u64);
    cleanup(&out);
}

#[tokio::test]
async fn pause_then_resume_produces_identical_file() {
    let data = test_data(8 * 1024 * 1024);
    let addr = spawn_server(
        data.clone(),
        Config {
            mode: Mode::Ranges,
            slice_delay: Duration::from_millis(15),
        },
    )
    .await;
    let out = temp_path("resume.bin");
    cleanup(&out);
    let url = format!("http://{addr}/file.bin");

    let task = DownloadTask::new(url.clone(), &out, 4);
    let token = task.cancel_handle();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        token.cancel();
    });
    let (tx, _events) = drain();
    let result = task.start_with_progress(tx).await;
    assert!(matches!(result, Err(DownloadError::Paused)), "got {result:?}");

    let saved = DownloadState::load_from_file(&out).await.expect("state file kept");
    let partial = saved.downloaded_total();
    assert!(partial > 0 && partial < data.len() as u64, "partial = {partial}");

    let task = DownloadTask::new(url, &out, 4);
    let (tx, events) = drain();
    task.start_with_progress(tx).await.unwrap();
    let events = events.await.unwrap();

    // The first event of the second session reports the bytes kept from the first.
    assert_eq!(events[0].downloaded_bytes, partial);
    assert_eq!(std::fs::read(&out).unwrap(), *data);
    assert!(!DownloadState::state_file_path(&out).exists());
    cleanup(&out);
}

#[tokio::test]
async fn server_without_range_support_uses_single_stream() {
    let data = test_data(3 * 1024 * 1024 + 5);
    let addr = spawn_server(
        data.clone(),
        Config {
            mode: Mode::NoRanges,
            slice_delay: Duration::ZERO,
        },
    )
    .await;
    let out = temp_path("norange.bin");
    cleanup(&out);

    let task = DownloadTask::new(format!("http://{addr}/file.bin"), &out, 8);
    let (tx, _events) = drain();
    task.start_with_progress(tx).await.unwrap();

    assert_eq!(std::fs::read(&out).unwrap(), *data);
    cleanup(&out);
}

#[tokio::test]
async fn http_errors_do_not_produce_a_file() {
    let data = test_data(1024);
    let addr = spawn_server(
        data,
        Config {
            mode: Mode::NotFound,
            slice_delay: Duration::ZERO,
        },
    )
    .await;
    let out = temp_path("missing.bin");
    cleanup(&out);

    let task = DownloadTask::new(format!("http://{addr}/nope"), &out, 4);
    let (tx, _events) = drain();
    let result = task.start_with_progress(tx).await;

    assert!(matches!(result, Err(DownloadError::HttpStatus(404))), "got {result:?}");
    assert!(!out.exists());
}

#[tokio::test]
async fn truncated_stream_is_an_error_and_keeps_resume_state() {
    let data = test_data(4 * 1024 * 1024);
    let addr = spawn_server(
        data.clone(),
        Config {
            mode: Mode::Truncate,
            slice_delay: Duration::ZERO,
        },
    )
    .await;
    let out = temp_path("truncated.bin");
    cleanup(&out);

    let task = DownloadTask::new(format!("http://{addr}/file.bin"), &out, 4);
    let (tx, _events) = drain();
    let result = task.start_with_progress(tx).await;

    assert!(result.is_err(), "a cut-off transfer must not report success");
    assert!(DownloadState::state_file_path(&out).exists());
    cleanup(&out);
}

#[tokio::test]
async fn changed_remote_file_restarts_instead_of_mixing_versions() {
    let old = test_data(3 * 1024 * 1024);
    let new = Arc::new(old.iter().map(|b| b.wrapping_add(1)).collect::<Vec<u8>>());
    let out = temp_path("changed.bin");
    cleanup(&out);

    // Leave a stale state file that claims half of the old file is present.
    let stale = DownloadState {
        url: "http://127.0.0.1:1/file.bin".into(),
        output_path: out.clone(),
        total_size: old.len() as u64,
        validator: Some("\"old\"".into()),
        parts: vec![oxidl_core::PartState {
            part_index: 0,
            start_byte: 0,
            end_byte: old.len() as u64 - 1,
            downloaded_bytes: old.len() as u64 / 2,
        }],
    };
    std::fs::write(&out, &old[..]).unwrap();
    stale.save_to_file().await.unwrap();

    let addr = spawn_server(new.clone(), fast()).await;
    let mut stale_same_url = stale.clone();
    stale_same_url.url = format!("http://{addr}/file.bin");
    stale_same_url.save_to_file().await.unwrap();

    let task = DownloadTask::new(format!("http://{addr}/file.bin"), &out, 2);
    let (tx, _events) = drain();
    task.start_with_progress(tx).await.unwrap();

    assert_eq!(std::fs::read(&out).unwrap(), *new);
    cleanup(&out);
}

#[tokio::test]
async fn non_http_urls_are_rejected() {
    let out = temp_path("scheme.bin");
    let task = DownloadTask::new("file:///etc/passwd", &out, 4);
    let (tx, _events) = drain();
    let result = task.start_with_progress(tx).await;
    assert!(matches!(result, Err(DownloadError::InvalidUrl(_))), "got {result:?}");
}
