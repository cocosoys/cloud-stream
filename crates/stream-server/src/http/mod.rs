//! HTTP routing: static bundle, signaling upgrade endpoint and health status
//! (design 2.2, 5.1).
//!
//! Stage1 routes:
//! * `GET /api/<app>/signaling/` — WS upgrade. `app` may be `webrtc` (client
//!   hardcodes it) or any other segment accepted as a fallback; the app segment
//!   is not validated against credentials (no auth gate in stage1).
//! * `GET /api/status` — plain-text health probe used by the deploy wrapper
//!   before the browser opens the signaling socket.
//! * Any other GET — served from the `--dist` root by `static`.
//!
//! The raw TCP accept loop, HTTP/Upgrade handshake, tungstenite upgrade and
//! TLS acceptor are assembled with task #35; this module fixes path classification
//! now so the loop can compile against a fixed route surface.

// `static` is a Rust keyword; the module is declared escaped and is still
// referenced as `crate::http::static` downstream.
pub mod r#static;
pub mod server;
pub mod file_transfer;

pub use server::serve;

use std::path::PathBuf;

/// Look up a request header value case-insensitively (shared by the HTTP
/// accept loop and the file-transfer handlers).
pub(crate) fn header_value<'h>(headers: &'h [httparse::Header<'_>], name: &str) -> Option<&'h [u8]> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value)
}

/// Write a plain `Connection: close` HTTP/1.1 response and shut the socket.
pub(crate) async fn write_http<S: tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    status_text: &str,
    mime: &str,
    body: &[u8],
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let head = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {mime}\r\nContent-Length: {len}\r\n\
         Connection: close\r\nCache-Control: no-cache\r\n\r\n",
        status = status,
        status_text = status_text,
        mime = mime,
        len = body.len(),
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    let _ = stream.shutdown().await;
    Ok(())
}

/// Classified request target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Signaling WS upgrade; carries the requested app segment.
    Signaling { app: String },
    /// Liveness probe.
    Status,
    /// #67: file-transfer tree: `GET /api/files/{rel}` lists a directory or
    /// streams a file attachment under the `--upload-dir` root.
    Files { rel: String },
    /// #67: `POST /api/upload` — raw octet-stream body with the destination
    /// relative path in the `X-Upload-Path` header.
    Upload,
    /// Static file path (relative to dist) to serve.
    Static(PathBuf),
    /// Unrecognized API segment or unsupported method; 404 / 400.
    NotFound,
}

/// Classify a request path (query stripped here as well). `app_name` is kept for
/// future strict matching; stage1 accepts the wildcard fallback per design 5.1.
pub fn classify(path: &str, _app_name: &str) -> Route {
    let clean = path.split(['?', '#']).next().unwrap_or("");

    let segments: Vec<&str> = clean
        .strip_prefix('/')
        .unwrap_or(clean)
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    match segments.as_slice() {
        ["api", "status"] => Route::Status,
        ["api", "upload"] => Route::Upload,
        ["api", "files", rest @ ..] => Route::Files {
            rel: rest.join("/"),
        },
        ["api", app, "signaling"] | ["api", app, "signaling", ""] => {
            Route::Signaling { app: (*app).to_owned() }
        }
        ["api", ..] => Route::NotFound,
        _ => {
            let rel = if segments.is_empty() {
                PathBuf::from("index.html")
            } else {
                PathBuf::from(segments.join("/"))
            };
            Route::Static(rel)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_signaling_wildcard() {
        assert!(matches!(
            classify("/api/webrtc/signaling/", "webrtc"),
            Route::Signaling { app } if app == "webrtc"
        ));
        assert!(matches!(
            classify("/api/webrtc/signaling", "webrtc"),
            Route::Signaling { .. }
        ));
    }

    #[test]
    fn routes_status_and_static() {
        assert!(matches!(classify("/api/status", "webrtc"), Route::Status));
        assert!(matches!(classify("/", "webrtc"), Route::Static(_)));
        assert!(matches!(
            classify("/assets/index.js", "webrtc"),
            Route::Static(_)
        ));
        assert!(matches!(classify("/api/other", "webrtc"), Route::NotFound));
    }

    #[test]
    fn routes_file_transfer() {
        assert!(matches!(classify("/api/upload", "webrtc"), Route::Upload));
        assert!(matches!(
            classify("/api/files/", "webrtc"),
            Route::Files { ref rel } if rel.is_empty()
        ));
        assert!(matches!(
            classify("/api/files", "webrtc"),
            Route::Files { ref rel } if rel.is_empty()
        ));
        assert!(matches!(
            classify("/api/files/saves/world.zip", "webrtc"),
            Route::Files { ref rel } if rel == "saves/world.zip"
        ));
        assert!(matches!(
            classify("/api/files/a/b/", "webrtc"),
            Route::Files { ref rel } if rel == "a/b"
        ));
    }
}
