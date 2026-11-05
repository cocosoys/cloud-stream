//! HTTP routing: static bundle, signaling upgrade endpoint and health status
//! (design 2.2, 5.1).
//!
//! Stage1 routes:
//! * `GET /api/<app>/signaling/` — WS upgrade. `app` may be `webrtc` (client
//!   hardcodes it) or any other segment accepted as a fallback; the app segment
//!   is not validated against credentials (no auth gate in stage1).
//! * `GET /api/status` — liveness probe used by the deploy wrapper before the
//!   browser opens the signaling socket. JSON (`current_mode`,
//!   `enable_dual_mode`): the dashboard parses it to choose a transport (#106).
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
// #93: `GET /api/v1/health` + the two `POST /api/v1/control/*` actions.
pub mod health;
// #99: `dist/.asset-manifest.json` — the bundle's hash table, read for ETags and
// served whole on `GET /api/v1/manifest`.
pub mod manifest;
// #106: `GET /api/websockets` — the WebSocket + WebCodecs transport. Same
// frames as WebRTC, different wire: 12-byte header, raw bitstream, no RTP.
pub mod ws_media;

pub use server::serve;
pub use health::HealthProbe;

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
    write_http_with(
        stream,
        status,
        status_text,
        mime,
        body,
        &[("Cache-Control", "no-cache")],
    )
    .await
}

/// Same, with extra response headers. `Cache-Control` is the caller's to set
/// here — static files pick between `no-cache` and `immutable`, everything else
/// goes through `write_http` above.
pub(crate) async fn write_http_with<S: tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    status_text: &str,
    mime: &str,
    body: &[u8],
    extra: &[(&str, &str)],
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut head = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {mime}\r\nContent-Length: {len}\r\n\
         Connection: close\r\n",
        status = status,
        status_text = status_text,
        mime = mime,
        len = body.len(),
    );
    for (name, value) in extra {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    let _ = stream.shutdown().await;
    Ok(())
}

/// `304 Not Modified` for a static file the client already holds. Carries no
/// body and no `Content-Length`: the point is to skip the transfer, so writing
/// even `0` invites clients to treat the response as a fresh entity.
pub(crate) async fn write_not_modified<S: tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    cache_control: &str,
    etag: Option<&str>,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut head = String::from("HTTP/1.1 304 Not Modified\r\nConnection: close\r\n");
    head.push_str("Cache-Control: ");
    head.push_str(cache_control);
    head.push_str("\r\n");
    if let Some(etag) = etag {
        head.push_str("ETag: ");
        head.push_str(etag);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    let _ = stream.shutdown().await;
    Ok(())
}

/// Whether an `If-None-Match` value names `etag`.
///
/// Comparison is weak: `W/"x"` and `"x"` are the same entity for the purpose of
/// "did this change", and a client is free to send either form back. `*`
/// matches anything the server has, which for a single file means the file.
pub(crate) fn etag_matches(header: &str, etag: &str) -> bool {
    if header.is_empty() || etag.is_empty() {
        return false;
    }
    let target = etag.trim().trim_start_matches("W/");
    header.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate.trim_start_matches("W/") == target
    })
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
    /// #93: `GET /api/v1/health` — the liveness + telemetry report.
    Health,
    /// #99: `GET /api/v1/manifest` — the bundle's hash table, as built.
    ///
    /// Exposed so a client (or the deploy chain) can learn what the server is
    /// actually serving without downloading it: which files exist, their
    /// SHA-256, and which ones are safe to cache forever.
    Manifest,
    /// #93: `POST /api/v1/control/<action>` — the small write surface. The
    /// action is kept as a raw string here and matched against a whitelist in
    /// the handler, so a typo yields 404 instead of a silent no-op.
    Control { action: String },
    /// Static file path (relative to dist) to serve.
    Static(PathBuf),
    /// #106: `GET /api/websockets` — the WebSocket media transport.
    ///
    /// A separate route from `Signaling`, not a second app segment under it:
    /// the two answer with different protocols on the same socket, and a
    /// signaling path that silently accepted a media client would look like a
    /// dead page rather than a wrong one.
    WebSockets,
    /// #106: `POST /api/switch` — the transport menu's entry point.
    ///
    /// The frontend posts `{"mode":"webrtc"|"websockets"}` here before it
    /// reloads into that mode (`lib/mode-switch.js:43`). Without it the menu
    /// gets a 404 and the page never reloads — a control that silently does
    /// nothing, which is exactly the failure shape this project keeps chasing.
    Switch,
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
        // #93: versioned surface. The unversioned `/api/status` above stays as
        // the deploy wrapper's cheap probe; this one is for machines, not for
        // "did the port open".
        ["api", "v1", "health"] => Route::Health,
        ["api", "v1", "manifest"] => Route::Manifest,
        ["api", "v1", "control", rest @ ..] => Route::Control {
            action: rest.join("/"),
        },
        ["api", "files", rest @ ..] => Route::Files {
            rel: rest.join("/"),
        },
        ["api", app, "signaling"] | ["api", app, "signaling", ""] => {
            Route::Signaling { app: (*app).to_owned() }
        }
        // #106: the media socket. The frontend builds this as
        // `getRoutePrefix() + '/api/websockets'` (`selkies-ws-core.js:7722`), so
        // with no prefix it is exactly this path — a trailing slash included
        // because a proxy may normalize one in.
        ["api", "websockets"] | ["api", "websockets", ""] => Route::WebSockets,
        ["api", "switch"] => Route::Switch,
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
    fn routes_websockets() {
        // #106: the path the frontend builds (`getRoutePrefix() + '/api/websockets'`).
        assert!(matches!(classify("/api/websockets", "webrtc"), Route::WebSockets));
        assert!(matches!(classify("/api/websockets/", "webrtc"), Route::WebSockets));
        // A token in the query must not change the route: `?token=` is one of the
        // five carriers the media socket authenticates with.
        assert!(matches!(
            classify("/api/websockets?token=abc", "webrtc"),
            Route::WebSockets
        ));
        // It is not a signaling app segment — the two take different protocols.
        assert!(matches!(
            classify("/api/websockets/signaling", "webrtc"),
            Route::Signaling { ref app } if app == "websockets"
        ));
        // #106: the mode menu's POST.
        assert!(matches!(classify("/api/switch", "webrtc"), Route::Switch));
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

    #[test]
    fn routes_health_and_control() {
        assert!(matches!(classify("/api/v1/health", "webrtc"), Route::Health));
        // A query string must not change the route.
        assert!(matches!(classify("/api/v1/health?x=1", "webrtc"), Route::Health));
        assert!(matches!(classify("/api/v1/manifest", "webrtc"), Route::Manifest));
        assert!(matches!(classify("/api/v1/manifest?v=1", "webrtc"), Route::Manifest));
        assert!(matches!(
            classify("/api/v1/control/capture/restart", "webrtc"),
            Route::Control { ref action } if action == "capture/restart"
        ));
        assert!(matches!(
            classify("/api/v1/control/session/kick", "webrtc"),
            Route::Control { ref action } if action == "session/kick"
        ));
        // An empty action still lands in the handler, which answers 404 with the
        // whitelist — better than pretending the endpoint does not exist.
        assert!(matches!(
            classify("/api/v1/control/", "webrtc"),
            Route::Control { ref action } if action.is_empty()
        ));
        // Unknown version segments stay 404 rather than falling through to static.
        assert!(matches!(classify("/api/v2/health", "webrtc"), Route::NotFound));
    }

    #[test]
    fn etag_matching_is_weak_and_list_aware() {
        let tag = "\"sha256-abc\"";
        assert!(etag_matches(tag, tag));
        // A client may replay the tag as weak, or list several: both mean "mine".
        assert!(etag_matches("W/\"sha256-abc\"", tag));
        assert!(etag_matches("\"other\", \"sha256-abc\"", tag));
        assert!(etag_matches("*", tag));
        // Whitespace around list members is legal padding, not a different tag.
        assert!(etag_matches("  \"sha256-abc\"  ", tag));
        // A different entity, an empty header and an empty tag all miss.
        assert!(!etag_matches("\"sha256-def\"", tag));
        assert!(!etag_matches("", tag));
        assert!(!etag_matches(tag, ""));
    }
}
