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

pub use server::serve;

use std::path::PathBuf;

/// Classified request target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Signaling WS upgrade; carries the requested app segment.
    Signaling { app: String },
    /// Liveness probe.
    Status,
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
}
