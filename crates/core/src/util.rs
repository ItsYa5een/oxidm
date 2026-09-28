use crate::error::DownloadError;
use percent_encoding::percent_decode_str;
use url::Url;

const FALLBACK_FILENAME: &str = "download.bin";
const MAX_FILENAME_CHARS: usize = 200;

/// Accepts only well-formed http and https URLs.
pub fn validate_url(input: &str) -> Result<(), DownloadError> {
    let url = Url::parse(input.trim()).map_err(|e| DownloadError::InvalidUrl(e.to_string()))?;
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(DownloadError::InvalidUrl(format!(
            "unsupported scheme: {other}"
        ))),
    }
}

/// Derives a safe local file name from the last path segment of a URL.
/// The query string is dropped, percent-escapes are decoded, path separators
/// and control characters become underscores, and leading dots are removed.
pub fn suggest_filename(input: &str) -> String {
    let last_segment = Url::parse(input.trim()).ok().and_then(|url| {
        url.path_segments()
            .and_then(|segments| segments.filter(|s| !s.is_empty()).last().map(str::to_owned))
    });

    let Some(segment) = last_segment else {
        return FALLBACK_FILENAME.to_string();
    };

    let decoded = percent_decode_str(&segment).decode_utf8_lossy();
    let cleaned: String = decoded
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned: String = cleaned
        .trim()
        .trim_start_matches('.')
        .chars()
        .take(MAX_FILENAME_CHARS)
        .collect();

    if cleaned.is_empty() {
        FALLBACK_FILENAME.to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_drops_query_and_decodes() {
        assert_eq!(
            suggest_filename("https://example.com/a/My%20File.zip?token=abc#frag"),
            "My File.zip"
        );
    }

    #[test]
    fn filename_falls_back_for_bare_hosts_and_dots() {
        assert_eq!(suggest_filename("https://example.com/"), "download.bin");
        assert_eq!(suggest_filename("https://example.com/.."), "download.bin");
        assert_eq!(suggest_filename("not a url"), "download.bin");
    }

    #[test]
    fn filename_neutralises_encoded_separators() {
        let name = suggest_filename("https://example.com/..%2F..%2Fetc%2Fpasswd");
        assert!(!name.contains('/'));
        assert!(!name.starts_with('.'));
    }

    #[test]
    fn only_http_schemes_pass() {
        assert!(validate_url("https://example.com/x").is_ok());
        assert!(validate_url("http://example.com/x").is_ok());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("").is_err());
    }
}
