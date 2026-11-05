//! HTTP/HTTPS accept loop, request routing and the WebSocket upgrade
//! (design 2.2, 5.1, 5.2).
//!
//! Stage1 split:
//! * `GET /api/status` — liveness probe. JSON since #106: the dashboard reads
//!   `current_mode` / `enable_dual_mode` from it to pick a transport; the
//!   deploy wrapper and the watchdog only check the status code.
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
use crate::http::{classify, file_transfer, header_value, r#static, write_http, Route};
use crate::pipeline::PipelineRegistry;
use crate::security::{self, Credential, Endpoint, EventKind, TokenGate};
use crate::signaling::websession::{self, Admission};
use crate::signaling::SessionRegistry;

/// 把路由映射成鉴权骨架认的端点（#88）。
///
/// 映射写成独立函数而不是内联在 `handle_inner` 里，是因为它是**唯一**一处
/// 需要跟着 `Route` 变化的地方：以后加端点时，编译器不会提醒你补策略，
/// 但补在这里至少只有一处会漏。
fn surface_of(route: &Route) -> EventKind {
    EventKind::Http(match route {
        Route::Status => Endpoint::Status,
        Route::Upload => Endpoint::Upload,
        Route::Files { .. } => Endpoint::Files,
        Route::Signaling { .. } => Endpoint::Signaling,
        // #93: 报告与控制分开映射 —— 前者是观测（谁连着、推流健不健康），
        // 后者会重开采集器或踢人，将来分级时必须是两道门。
        Route::Health => Endpoint::Health,
        Route::Control { .. } => Endpoint::Control,
        // #99: 清单把全站的文件名和哈希一次性摊开，属于侦察面，单独一类而不是
        // 并进 `Static` —— 将来"能拉页面"和"能点出整棵文件树"可以分权。
        Route::Manifest => Endpoint::Manifest,
        Route::Static(_) => Endpoint::Static,
        // #106: 媒体 socket 与信令同一个端点面 —— 两者都是"给我一路画面"的升级
        // 路径，区别只在线上格式。给它单开一类，就得到策略表里再加一处"忘了配"
        // 的地方，而默认值会比现在更宽松还是更严，取决于表的写法：不值得赌。
        Route::WebSockets => Endpoint::Signaling,
        // #106: 切传输是一次写操作（前端会据此重载页面），归到控制面。
        Route::Switch => Endpoint::Control,
        Route::NotFound => Endpoint::NotFound,
    })
}

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
///
/// #88: `peer` is carried all the way down so the policy sees who is asking.
/// Before this it only appeared in a debug log line, which meant a future
/// allow-list had nothing to match on at the one point every request passes.
async fn handle_connection<S>(
    stream: S,
    cfg: Arc<Config>,
    registry: SessionRegistry,
    admission: Admission,
    pipelines: PipelineRegistry,
    chunks: file_transfer::ChunkStore,
    health: crate::http::HealthProbe,
    peer: std::net::IpAddr,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    if let Err(err) = handle_inner(
        stream, cfg, registry, admission, pipelines, chunks, health, peer,
    )
    .await
    {
        log::debug!("connection closed: {err:#}");
    }
}

async fn handle_inner<S>(
    mut stream: S,
    cfg: Arc<Config>,
    registry: SessionRegistry,
    admission: Admission,
    pipelines: PipelineRegistry,
    chunks: file_transfer::ChunkStore,
    health: crate::http::HealthProbe,
    peer: std::net::IpAddr,
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

    // #88: 每个请求的第一个副作用点。裁决放在方法检查之前 —— 一个被判死的
    // 客户端不该还能从 405/413 这些响应里读到任何东西。
    //
    // 注意 `Route::Files` 同时承载"列目录"和"下载"，两者都是读，所以统一映射
    // 到 `Endpoint::Files`；真正的写入口只有 `Endpoint::Upload`。
    // #101: 口令先于策略。顺序是有意的 —— 没出示凭据的人不该先收到
    // "403 Forbidden"，那等于告诉他"这里有东西，只是你没被允许"。
    //
    // 两类豁免：
    //   * `Static` —— 前端 bundle 本身。Selkies 前端是 MPL 开源的，代码里没有
    //     秘密；而且页面必须能打开，否则用户连"这机器要口令"都看不到，只能
    //     看到一片空白 —— 那正是本项目花了几轮才消掉的"启动了但没效果"。
    //   * `Status` —— 部署脚本的探活口径，正文只有 `ok` 两个字。
    let credential = match cfg.auth_token.as_deref().and_then(TokenGate::new) {
        Some(gate) if !matches!(route, Route::Static(_) | Route::Status) => {
            match gate.check_request(&headers, path) {
                Some(cred) => cred,
                None => {
                    // `WWW-Authenticate` 必须带 `Bearer realm="Selkies"`：前端的
                    // `auth-guard.js` 按这个前缀区分"令牌不匹配"（重载也没用，
                    // 所以不重载）和"Basic 墙"（重载才能重新登录）。少了它，
                    // 输错口令的页面会每 30 秒自刷一次且永远好不了。
                    let challenge =
                        format!("Bearer realm=\"{}\"", crate::security::token::REALM);
                    crate::http::write_http_with(
                        &mut stream,
                        401,
                        "Unauthorized",
                        "text/plain; charset=utf-8",
                        b"401 Unauthorized: this server requires a token; append ?token=<token> to the page URL\n",
                        &[("WWW-Authenticate", challenge.as_str())],
                    )
                    .await?;
                    return Ok(());
                }
            }
        }
        _ => Credential::None,
    };

    let principal = crate::security::token::principal(peer, credential);
    if !crate::security::allow(surface_of(&route), &principal) {
        write_http(
            &mut stream,
            403,
            "Forbidden",
            "text/plain; charset=utf-8",
            b"403 Forbidden\n",
        )
        .await?;
        return Ok(());
    }

    // #67: POST /api/upload carries the raw body; every other method is 405.
    if method == "POST" {
        // #106: 模式菜单的入口。放在 upload 之前，因为它的 body 很小且是要解析的
        // JSON，而 upload 那条路会把整段字节写进磁盘目录。
        if matches!(route, crate::http::Route::Switch) {
            let clen = header_value(&headers, "Content-Length")
                .and_then(|v| std::str::from_utf8(v).ok())
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            // 一个 `{"mode":"websockets"}` 是 24 字节；四位数的上限够宽松，也够
            // 挡住"往这个端点灌一百兆"这种最省事的滥用。
            if clen > 4096 {
                write_http(
                    &mut stream,
                    413,
                    "Payload Too Large",
                    "application/json; charset=utf-8",
                    br#"{"status":"error","message":"switch body too large"}"#,
                )
                .await?;
                return Ok(());
            }
            let mut body = Vec::with_capacity(clen);
            body.extend_from_slice(&extra);
            extra.clear();
            let need = clen.saturating_sub(body.len());
            if need > 0 {
                let mut rest = vec![0u8; need];
                stream.read_exact(&mut rest).await?;
                body.extend_from_slice(&rest);
            }
            let _ = crate::http::ws_media::handle_switch(&mut stream, &body).await;
            return Ok(());
        }
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
        // #93: the write surface is two POSTs; everything else on POST stays 405
        // so a stray form post cannot look like it did something.
        if let crate::http::Route::Control { action } = &route {
            let _ = crate::http::health::handle_control(
                &mut stream,
                action,
                &registry,
                &pipelines,
                &cfg,
            )
            .await;
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
        // #106: JSON, not the old plain-text `ok`. The dashboard's
        // `detectInitialMode()` (`main.jsx`) parses this body and uses it to
        // decide which transport to start and whether to draw the panel's
        // mode switch; against `ok\n` it threw, swallowed the error, and the
        // page fell through to the core's hard default — which used to be a
        // WebSocket connection to an endpoint this server did not have.
        //
        // The probe's *status code* is what the deploy wrapper and the
        // watchdog check, so the body change costs them nothing.
        crate::http::Route::Status => {
            let body = serde_json::json!({
                "status": "ok",
                "current_mode": cfg.default_mode,
                "enable_dual_mode": true,
                "transports": ["webrtc", "websockets"],
            })
            .to_string();
            write_http(
                &mut stream,
                200,
                "OK",
                "application/json; charset=utf-8",
                body.as_bytes(),
            )
            .await?;
        }

        // #93: the probing endpoint. Answers 200 when the pipeline is healthy or
        // simply idle, 503 when the process is up but frames stopped going out —
        // that distinction is the whole point, `Route::Status` cannot make it.
        crate::http::Route::Health => {
            let _ = crate::http::health::handle_health(
                &mut stream,
                &cfg,
                &registry,
                &admission,
                &pipelines,
                &health,
            )
            .await;
            return Ok(());
        }

        // #99: the bundle's hash table. Same bytes the build wrote, so a client
        // can diff its cache against the server instead of re-downloading.
        crate::http::Route::Manifest => {
            let inm = header_value(&headers, "If-None-Match")
                .and_then(|v| std::str::from_utf8(v).ok())
                .unwrap_or("");
            match crate::http::manifest::body(&cfg.dist) {
                Some((_, etag)) if crate::http::etag_matches(inm, &etag) => {
                    crate::http::write_not_modified(&mut stream, "no-cache", Some(&etag))
                        .await?;
                }
                Some((bytes, etag)) => {
                    crate::http::write_http_with(
                        &mut stream,
                        200,
                        "OK",
                        "application/json; charset=utf-8",
                        bytes.as_slice(),
                        &[("Cache-Control", "no-cache"), ("ETag", etag.as_str())],
                    )
                    .await?;
                }
                None => {
                    crate::http::write_http(
                        &mut stream,
                        404,
                        "Not Found",
                        "application/json; charset=utf-8",
                        br#"{"status":"error","message":"this dist has no .asset-manifest.json"}"#,
                    )
                    .await?;
                }
            }
            return Ok(());
        }

        // The control surface is POST-only; a GET is a method mismatch.
        crate::http::Route::Control { .. } => {
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

        // #106: same for the transport switch. A GET here would look like a probe
        // that always "succeeds" while changing nothing.
        crate::http::Route::Switch => {
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

        crate::http::Route::Signaling { .. } => {
            // #88: 路径匹配放行不等于允许开会话。"这个 URL 存不存在"和"你能
            // 不能拿到一路画面"是两个问题，前者在 `Endpoint::Signaling` 判过，
            // 后者在这里判 —— 未来的观众/操作员分级就落在这一道上。
            if !security::allow(EventKind::Signaling, &principal) {
                write_http(
                    &mut stream,
                    403,
                    "Forbidden",
                    "text/plain; charset=utf-8",
                    b"403 Forbidden\n",
                )
                .await?;
                return Ok(());
            }
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

            // #102: 准入放在升级**之前**。拒绝一个还在握手的客户端，比让它建
            // PeerConnection、走完 SDP、再被一条关闭码踹掉便宜得多；而且 101
            // 之后的拒绝只能靠关闭码表达，前端读到的是"连上又断"，会当成网络
            // 抖动进入重连循环 —— 那正是限流想避免的负载。
            //
            // `Retry-After` 是有用的：前端的重连是带退避的，没有这个头它只能按
            // 自己的节奏试；有了它，一个被拒的页面至少知道"别人占满了"，而不是
            // "服务端坏了"。
            let Some(permit) = admission.try_enter() else {
                let body = format!(
                    "{{\"status\":\"error\",\"message\":\"session limit reached ({}); retry after a holder disconnects\"}}",
                    admission.max()
                );
                crate::http::write_http_with(
                    &mut stream,
                    503,
                    "Service Unavailable",
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    &[("Retry-After", "5")],
                )
                .await?;
                log::warn!(
                    "signaling refused from {peer}: {} of {} slots in use",
                    admission.live(),
                    admission.max()
                );
                return Ok(());
            };

            // #93: hand the identity down so `/api/v1/health` can name the
            // client. `User-Agent` is the only fingerprint a browser volunteers;
            // the rest (`client_device_id`) comes in the hello.
            //
            // Built **before** `head` is moved into `PrefixedStream` — `headers`
            // borrows those bytes, so reading a header afterwards does not
            // compile.
            let client = crate::signaling::ClientMeta {
                ip: peer.to_string(),
                user_agent: header_value(&headers, "User-Agent")
                    .and_then(|v| std::str::from_utf8(v).ok())
                    .unwrap_or("")
                    .to_owned(),
            };

            let prefixed = PrefixedStream { head, pos: 0, inner: stream };
            // The route was already classified as signaling; the callback only
            // confirms the request method/token with tungstenite and accepts.
            let ws: WebSocketStream<PrefixedStream<S>> = accept_hdr_async(prefixed, |_req: &Request, resp: Response| {
                Ok(resp)
            })
            .await
            .map_err(|err| anyhow!("signaling upgrade failed: {err}"))?;

            websession::run(ws, cfg, registry, pipelines, client, permit).await?;
        }

        // #106: 画面走 WebSocket 的那条线。和信令是同一个升级动作，但**下游没有
        // 一处共享**：这里发的是 12 字节头 + 裸码流，不开 PeerConnection，也不进
        // 信令状态机。混在一个分支里，媒体客户端会停在"等 HELLO"上，页面一片空白
        // 而日志一切正常 —— 这类"接上了但没画面"正是这一版要消灭的形态。
        //
        // 裁决复用 `Endpoint::Signaling` + `EventKind::Signaling`：能开一路画面的
        // 人就能用任一传输看这一路画面，传输方式不是权限维度。等真的出现
        // "只许看、不许开 PeerConnection" 的需求，再单独分一个端点。
        crate::http::Route::WebSockets => {
            if !security::allow(EventKind::Signaling, &principal) {
                write_http(
                    &mut stream,
                    403,
                    "Forbidden",
                    "text/plain; charset=utf-8",
                    b"403 Forbidden\n",
                )
                .await?;
                return Ok(());
            }
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
                // 前端的重连探针会 GET 这个路径而不升级（`scheduleReconnect`
                // 用一次普通请求问"服务还在不在"），所以这里必须给一个明确的
                // 4xx 而不是挂住 —— 探针读到 400 就知道该重开 socket 了。
                write_http(
                    &mut stream,
                    400,
                    "Bad Request",
                    "text/plain; charset=utf-8",
                    b"400 Expected WebSocket upgrade\n",
                )
                .await?;
                return Ok(());
            }
            // 与信令同一道闸：拒绝放在 101 之前，前端能读到 503 + Retry-After
            // 并退避；101 之后再拒只能靠关闭码，页面会当成网络抖动。
            let Some(permit) = admission.try_enter() else {
                let body = format!(
                    "{{\"status\":\"error\",\"message\":\"session limit reached ({}); retry after a holder disconnects\"}}",
                    admission.max()
                );
                crate::http::write_http_with(
                    &mut stream,
                    503,
                    "Service Unavailable",
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    &[("Retry-After", "5")],
                )
                .await?;
                log::warn!(
                    "websockets refused from {peer}: {} of {} slots in use",
                    admission.live(),
                    admission.max()
                );
                return Ok(());
            };

            let prefixed = PrefixedStream { head, pos: 0, inner: stream };
            let ws: WebSocketStream<PrefixedStream<S>> =
                accept_hdr_async(prefixed, |_req: &Request, resp: Response| Ok(resp))
                    .await
                    .map_err(|err| anyhow!("websockets upgrade failed: {err}"))?;
            crate::http::ws_media::run(ws, cfg, registry.clone(), pipelines, permit, peer, credential).await?;
        }

        crate::http::Route::Static(rel) => {
            let url_path = rel.to_string_lossy();
            match r#static::serve(&cfg.dist, &url_path)? {
                Some(file) => {
                    // #99: revalidate instead of re-send. The tag is the file's
                    // SHA-256 from the build's hash table, so a hit here proves
                    // the client's copy is byte-identical — 304 carries no body.
                    let inm = header_value(&headers, "If-None-Match")
                        .and_then(|v| std::str::from_utf8(v).ok())
                        .unwrap_or("");
                    let etag = file.etag.as_deref();
                    if etag.map(|tag| crate::http::etag_matches(inm, tag)).unwrap_or(false) {
                        crate::http::write_not_modified(&mut stream, file.cache_control, etag)
                            .await?;
                    } else {
                        let mut extra: Vec<(&str, &str)> =
                            vec![("Cache-Control", file.cache_control)];
                        if let Some(tag) = etag {
                            extra.push(("ETag", tag));
                        }
                        crate::http::write_http_with(
                            &mut stream,
                            200,
                            "OK",
                            &file.mime,
                            file.bytes.as_slice(),
                            &extra,
                        )
                        .await?;
                    }
                }
                None => {
                    let missing = r#static::not_found();
                    crate::http::write_http(
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
    // #102: how many sessions may hold the stream at once. One shared encoder
    // means "one more viewer" is not free — it costs a packetizer pass and a
    // full copy of the bitrate — so the bound exists to protect the people
    // already watching, not to ration a license. Default 8, `--max-sessions`.
    let admission = Admission::new(cfg.max_sessions);
    log::info!("session admission: at most {} live signaling session(s)", admission.max());
    // #101: one program per display, shared by every session attached to it.
    // Held here rather than per connection because a capture outlives the
    // session that opened it whenever a second viewer is still watching.
    let pipelines = PipelineRegistry::default();
    // #67: one chunked-upload registry per process; each upload slice is an
    // independent TCP connection, so the registry is shared across them.
    let chunks = file_transfer::new_chunk_store();
    // #93: one probe per process, shared by every connection. Per-connection
    // probes would each own a sysinfo `System` and re-scan the process table on
    // every request — the observer would become the load.
    let health = crate::http::HealthProbe::new();
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let cfg = cfg.clone();
                let registry = registry.clone();
                let admission = admission.clone();
                let pipelines = pipelines.clone();
                let chunks = chunks.clone();
                let health = health.clone();
                match &acceptor {
                    Some(_) => {
                        // TlsAcceptor wraps an Arc<ServerConfig>; clone it into
                        // the spawned task so the borrowed reference does not
                        // need a 'static lifetime on the local acceptor binding.
                        let acc = acceptor.clone().unwrap();
                        tokio::spawn(async move {
                            match acc.accept(stream).await {
                                Ok(tls) => {
                                    handle_connection(
                                        tls, cfg, registry, admission, pipelines, chunks, health,
                                        peer.ip(),
                                    )
                                    .await
                                }
                                Err(err) => {
                                    log::debug!("TLS handshake failed from {peer}: {err:#}")
                                }
                            }
                        });
                    }
                    None => {
                        tokio::spawn(async move {
                            handle_connection(
                                stream, cfg, registry, admission, pipelines, chunks, health,
                                peer.ip(),
                            )
                            .await;
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
