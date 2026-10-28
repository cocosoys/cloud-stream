//! HTTP/HTTPS accept loop, request routing and the WebSocket upgrade
//! (design 2.2, 5.1, 5.2).
//!
//! Stage1 split:
//! * `GET /api/status` — plain-text liveness probe.
//! * `GET /api/<app>/signaling/` — tungstenite WebSocket upgrade; the session
//!   handshake and SDP/ICE exchange live in `signaling::websession`.
//! * Every other GET — served from `--dist` by `http::static`.
//!
//! TLS (#37): `Config::tls_enabled()` decides between a plain `TcpStream` and a
//! `tokio_rustls` wrapped stream. The routing code below is generic over
//! `AsyncRead + AsyncWrite + Unpin` so the same handlers serve both transports;
//! `PrefixedStream` replays the head bytes already consumed before tungstenite
//! parses the request a second time for the 101 upgrade. Static and probe
//! responses are written by hand with a fixed `Connection: close`.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{anyhow, Result};
use httparse::Status;
use log;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpListener;
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::handshake::server::{Request, Response},
    WebSocketStream,
};
use tokio_rustls::{rustls, TlsAcceptor};
use rustls_pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};

use crate::config::Config;
use crate::http::{classify, file_transfer, header_value, r#static, write_http};
use crate::signaling::websession;
use crate::signaling::SessionRegistry;

/// Maximum accepted size of a plain HTTP request head before the first body
/// byte. Static GETs and the signaling handshake carry none.
const MAX_HEAD: usize = 16 * 1024;

/// Read bytes until the empty line (`\r\n\r\n`) closes the request head. A
/// single `read` may deliver head and body together; the bytes past the head
/// terminator are returned as `extra` so a POST body is never lost (the 4 KiB
/// read chunk frequently contains the first body bytes).
async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return if buf.is_empty() {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed before request head"))
            } else {
                Err(io::Error::new(io::ErrorKind::InvalidData, "truncated request head"))
            };
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let end = idx + 4;
            return Ok((buf[..end].to_vec(), buf[end..].to_vec()));
        }
        if buf.len() > MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head exceeds 16 KiB",
            ));
        }
    }
}

/// A stream that first replays the already-read request head, then proxies the
/// raw inner stream. `accept_hdr_async` reads the HTTP request again; replaying
/// the head keeps those bytes from being lost. All fields are `Unpin`, so the
/// stream itself is `Unpin` as required by the async wrapper bounds.
struct PrefixedStream<S> {
    head: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S: Unpin> Unpin for PrefixedStream<S> {}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pos < self.head.len() {
            let remaining = self.head.len() - self.pos;
            let n = std::cmp::min(remaining, buf.remaining());
            buf.put_slice(&self.head[self.pos..self.pos + n]);
            self.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Build the rustls acceptor when both certificate and key paths are supplied;
/// `None` keeps the plain HTTP stage1 fallback. Self-signed PEM chains are
/// expected on the LAN deploy; client auth is disabled.
fn build_tls_acceptor(cfg: &Config) -> Result<Option<TlsAcceptor>> {
    let (Some(cert_path), Some(key_path)) = (&cfg.tls_cert, &cfg.tls_key) else {
        return Ok(None);
    };

    let cert_pem = std::fs::read(cert_path)
        .map_err(|err| anyhow!("read TLS certificate {} failed: {err}", cert_path.display()))?;
    let key_pem = std::fs::read(key_path)
        .map_err(|err| anyhow!("read TLS key {} failed: {err}", key_path.display()))?;

    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<_, _>>()
        .map_err(|err| anyhow!("parse TLS certificate PEM: {err}"))?;
    if certs.is_empty() {
        return Err(anyhow!("no certificate section in {}", cert_path.display()));
    }
    let cert_count = certs.len();
    let key = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|err| anyhow!("parse TLS private key PEM: {err}"))?;

    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|err| anyhow!("configure TLS server: {err}"))?;

    log::info!("TLS certificate {} loaded ({} certificate section(s))", cert_path.display(), cert_count);
    Ok(Some(TlsAcceptor::from(Arc::new(config))))
}

/// One accepted TCP connection: route the request, answer static/status, or
/// hand the upgraded signaling socket to the session task.
async fn handle_connection<S>(
    stream: S,
    cfg: Arc<Config>,
    registry: SessionRegistry,
    chunks: file_transfer::ChunkStore,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    if let Err(err) = handle_inner(stream, cfg, registry, chunks).await {
        log::debug!("connection closed: {err:#}");
    }
}

async fn handle_inner<S>(
    mut stream: S,
    cfg: Arc<Config>,
    registry: SessionRegistry,
    chunks: file_transfer::ChunkStore,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (head, mut extra) = read_head(&mut stream).await?;
    let mut headers: [httparse::Header<'_>; 64] =
        std::array::from_fn(|_| httparse::Header { name: "", value: &[] });
    let mut req = httparse::Request::new(&mut headers);
    let Status::Complete(_) = req.parse(&head)? else {
        return Err(anyhow!("incomplete HTTP request"));
    };

    let method = req.method.unwrap_or("GET");
    let path = req.path.unwrap_or("/");
    let route = classify(path, &cfg.app_name);

    // #67: POST /api/upload carries the raw body; every other method is 405.
    if method == "POST" {
        if matches!(route, crate::http::Route::Upload) {
            let clen = header_value(&headers, "Content-Length")
                .and_then(|v| std::str::from_utf8(v).ok())
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            if clen > file_transfer::MAX_UPLOAD_BODY {
                write_http(
                    &mut stream,
                    413,
                    "Payload Too Large",
                    "application/json; charset=utf-8",
                    br#"{"status":"error","message":"request body too large"}"#,
                )
                .await?;
                return Ok(());
            }
            // The body may already have arrived with the head; read only the
            // remaining bytes, never re-read what `read_head` consumed.
            let mut body = Vec::with_capacity(clen);
            body.extend_from_slice(&extra);
            extra.clear();
            let need = clen.saturating_sub(body.len());
            if need > 0 {
                let mut rest = vec![0u8; need];
                stream.read_exact(&mut rest).await?;
                body.extend_from_slice(&rest);
            }
            let _ = file_transfer::handle_upload(&mut stream, &headers, &body, &cfg, &chunks).await;
            return Ok(());
        }
        write_http(
            &mut stream,
            405,
            "Method Not Allowed",
            "text/plain; charset=utf-8",
            b"405 Method Not Allowed",
        )
        .await?;
        return Ok(());
    }

    if method != "GET" {
        write_http(
            &mut stream,
            405,
            "Method Not Allowed",
            "text/plain; charset=utf-8",
            b"405 Method Not Allowed",
        )
        .await?;
        return Ok(());
    }

    match route {
        crate::http::Route::Files { rel } => {
            let _ = file_transfer::handle_files(&mut stream, &rel, &cfg).await;
            return Ok(());
        }

        // A GET to the upload endpoint is a method mismatch.
        crate::http::Route::Upload => {
            write_http(
                &mut stream,
                405,
                "Method Not Allowed",
                "text/plain; charset=utf-8",
                b"405 Method Not Allowed",
            )
            .await?;
            return Ok(());
        }
        crate::http::Route::Status => {
            write_http(
                &mut stream,
                200,
                "OK",
                "text/plain; charset=utf-8",
                b"ok\n",
            )
            .await?;
        }

        crate::http::Route::Signaling { .. } => {
            let upgrade = header_value(&headers, "Upgrade")
                .and_then(|v| std::str::from_utf8(v).ok())
                .unwrap_or("");
            let connection = header_value(&headers, "Connection")
                .and_then(|v| std::str::from_utf8(v).ok())
                .unwrap_or("");
            let is_ws = upgrade.eq_ignore_ascii_case("websocket")
                && connection
                    .split([',', ' '])
                    .any(|p| p.eq_ignore_ascii_case("upgrade"));
            if !is_ws {
                write_http(
                    &mut stream,
                    400,
                    "Bad Request",
                    "text/plain; charset=utf-8",
                    b"400 Expected WebSocket upgrade",
                )
                .await?;
                return Ok(());
            }

            let prefixed = PrefixedStream { head, pos: 0, inner: stream };
            // The route was already classified as signaling; the callback only
            // confirms the request method/token with tungstenite and accepts.
            let ws: WebSocketStream<PrefixedStream<S>> = accept_hdr_async(prefixed, |_req: &Request, resp: Response| {
                Ok(resp)
            })
            .await
            .map_err(|err| anyhow!("signaling upgrade failed: {err}"))?;

            websession::run(ws, cfg, registry).await?;
        }

        crate::http::Route::Static(rel) => {
            let url_path = rel.to_string_lossy();
            match r#static::serve(&cfg.dist, &url_path)? {
                Some(file) => {
                    write_http(&mut stream, 200, "OK", &file.mime, file.bytes.as_slice()).await?;
                }
                None => {
                    let missing = r#static::not_found();
                    write_http(
                        &mut stream,
                        404,
                        "Not Found",
                        &missing.mime,
                        missing.bytes.as_slice(),
                    )
                    .await?;
                }
            }
        }

        crate::http::Route::NotFound => {
            write_http(
                &mut stream,
                404,
                "Not Found",
                "text/plain; charset=utf-8",
                b"404 Not Found",
            )
            .await?;
        }
    }

    Ok(())
}

/// Bind the listener and accept connections until the process stops. When a
/// TLS certificate chain is supplied, every accepted stream is handed through
/// the rustls acceptor before routing.
pub async fn serve(cfg: Arc<Config>) -> Result<()> {
    let acceptor = build_tls_acceptor(&cfg)?;
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|err| anyhow!("bind {addr} failed: {err}"))?;

    if acceptor.is_some() {
        log::info!("listening on https://{addr} (TLS rustls 1.2/1.3)");
    } else {
        log::warn!("listening on http://{addr} (plain HTTP stage1 fallback; no TLS certificate)");
    }

    let registry = SessionRegistry::default();
    // #67: one chunked-upload registry per process; each upload slice is an
    // independent TCP connection, so the registry is shared across them.
    let chunks = file_transfer::new_chunk_store();
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let cfg = cfg.clone();
                let registry = registry.clone();
                let chunks = chunks.clone();
                match &acceptor {
                    Some(_) => {
                        // TlsAcceptor wraps an Arc<ServerConfig>; clone it into
                        // the spawned task so the borrowed reference does not
                        // need a 'static lifetime on the local acceptor binding.
                        let acc = acceptor.clone().unwrap();
                        tokio::spawn(async move {
                            match acc.accept(stream).await {
                                Ok(tls) => handle_connection(tls, cfg, registry, chunks).await,
                                Err(err) => {
                                    log::debug!("TLS handshake failed from {peer}: {err:#}")
                                }
                            }
                        });
                    }
                    None => {
                        tokio::spawn(async move {
                            handle_connection(stream, cfg, registry, chunks).await;
                        });
                    }
                }
                log::debug!("accepted connection from {peer}");
            }
            Err(err) => {
                log::warn!("accept failed: {err}");
            }
        }
    }
}
