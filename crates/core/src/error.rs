use thiserror::Error;

#[derive(Error, Debug)]
pub enum DownloadError {
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Server ignored the Range header")]
    RangeNotSupported,
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),
    #[error("Server answered with HTTP status {0}")]
    HttpStatus(u16),
    #[error("Connection stalled (no data received)")]
    Timeout,
    #[error("Download ended before all bytes arrived")]
    Incomplete,
    #[error("Download paused by user")]
    Paused,
    #[error("Worker task failed: {0}")]
    Worker(String),
}
