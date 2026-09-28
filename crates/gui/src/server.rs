use axum::{
    extract::{Request, State},
    http::{header, HeaderName, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Header that carries the shared secret from the browser extension.
pub const TOKEN_HEADER: &str = "x-oxidm-token";

#[derive(Deserialize)]
pub struct DownloadPayload {
    pub url: String,
}

#[derive(Serialize)]
pub struct ApiResponse {
    pub status: String,
    pub message: String,
}

#[derive(Clone)]
struct AppState {
    url_tx: mpsc::Sender<String>,
    token: Arc<String>,
}

/// Loads the API token from `path`, or creates a new random one readable only by the owner.
pub fn load_or_create_token(path: &Path) -> std::io::Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let existing = existing.trim().to_string();
        if existing.len() >= 32 {
            return Ok(existing);
        }
    }

    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(token.as_bytes())?;
    Ok(token)
}

/// Only browser extensions may call the API from a browser context.
fn origin_allowed(origin: &HeaderValue) -> bool {
    origin
        .to_str()
        .map(|o| o.starts_with("chrome-extension://") || o.starts_with("moz-extension://"))
        .unwrap_or(false)
}

fn tokens_match(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn reject(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(ApiResponse {
            status: "error".to_string(),
            message: message.to_string(),
        }),
    )
        .into_response()
}

/// Runs before the JSON body is parsed, so unauthenticated callers learn nothing.
async fn require_auth(State(app): State<AppState>, req: Request, next: Next) -> Response {
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        if !origin_allowed(origin) {
            return reject(StatusCode::FORBIDDEN, "Origin not allowed");
        }
    }

    let supplied = req
        .headers()
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !tokens_match(supplied, &app.token) {
        return reject(StatusCode::UNAUTHORIZED, "Missing or invalid token");
    }

    next.run(req).await
}

fn build_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| origin_allowed(origin)))
        .allow_methods([Method::POST])
        .allow_headers([header::CONTENT_TYPE, HeaderName::from_static(TOKEN_HEADER)]);

    // The CORS layer sits outside the auth layer, so preflight requests never need the token.
    Router::new()
        .route("/add", post(handle_add_download))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .layer(cors)
        .with_state(state)
}

pub async fn start_server(url_tx: mpsc::Sender<String>, token: String) -> std::io::Result<()> {
    let app = build_router(AppState {
        url_tx,
        token: Arc::new(token),
    });

    let addr = SocketAddr::from(([127, 0, 0, 1], 3030));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await
}

async fn handle_add_download(
    State(app): State<AppState>,
    Json(payload): Json<DownloadPayload>,
) -> Response {
    if let Err(err) = oxidl_core::validate_url(&payload.url) {
        return reject(StatusCode::BAD_REQUEST, &err.to_string());
    }

    if app.url_tx.send(payload.url).await.is_err() {
        return reject(StatusCode::SERVICE_UNAVAILABLE, "Application is shutting down");
    }

    (
        StatusCode::OK,
        Json(ApiResponse {
            status: "success".to_string(),
            message: "Download queued successfully".to_string(),
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn app() -> (Router, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(8);
        let router = build_router(AppState {
            url_tx: tx,
            token: Arc::new(TOKEN.to_string()),
        });
        (router, rx)
    }

    fn post_add(url: &str, token: Option<&str>, origin: Option<&str>) -> axum::http::Request<Body> {
        let mut req = axum::http::Request::builder()
            .method("POST")
            .uri("/add")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            req = req.header(TOKEN_HEADER, token);
        }
        if let Some(origin) = origin {
            req = req.header(header::ORIGIN, origin);
        }
        req.body(Body::from(format!(r#"{{"url":"{url}"}}"#))).unwrap()
    }

    #[tokio::test]
    async fn missing_or_wrong_token_is_rejected() {
        let (router, mut rx) = app();
        let res = router
            .clone()
            .oneshot(post_add("https://example.com/a.zip", None, None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = router
            .oneshot(post_add("https://example.com/a.zip", Some("wrong"), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn web_page_origin_is_rejected_even_with_the_token() {
        let (router, mut rx) = app();
        let res = router
            .oneshot(post_add(
                "https://example.com/a.zip",
                Some(TOKEN),
                Some("https://evil.example"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn valid_token_queues_the_download() {
        let (router, mut rx) = app();
        let res = router
            .clone()
            .oneshot(post_add("https://example.com/a.zip", Some(TOKEN), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(rx.try_recv().unwrap(), "https://example.com/a.zip");

        let res = router
            .oneshot(post_add(
                "https://example.com/b.zip",
                Some(TOKEN),
                Some("moz-extension://abc"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(rx.try_recv().unwrap(), "https://example.com/b.zip");
    }

    #[tokio::test]
    async fn non_http_urls_are_rejected() {
        let (router, mut rx) = app();
        let res = router
            .oneshot(post_add("file:///etc/passwd", Some(TOKEN), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(rx.try_recv().is_err());
    }

    fn preflight(origin: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method("OPTIONS")
            .uri("/add")
            .header(header::ORIGIN, origin)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type,x-oxidm-token")
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn cors_preflight_only_approves_extensions() {
        let (router, _rx) = app();
        let res = router
            .clone()
            .oneshot(preflight("https://evil.example"))
            .await
            .unwrap();
        assert!(res.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());

        let res = router
            .oneshot(preflight("chrome-extension://abcdef"))
            .await
            .unwrap();
        assert_eq!(
            res.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            "chrome-extension://abcdef"
        );
    }
}
