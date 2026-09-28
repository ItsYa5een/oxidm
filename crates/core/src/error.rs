use thiserror::Error;

#[derive(Error, Debug)]
pub enum DownloadError {
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Server does not support range requests")]
    RangeNotSupported,
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),
    #[error("Failed to extract content length from server")]
    MissingContentLength,
    #[error("Download paused by user")]
    Paused,
}