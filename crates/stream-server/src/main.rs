//! Stage1 stream server entry point.
//!
//! Stage1 assembly order (design 7.2 -> 3.1 -> 6.1-6.5 -> 5.2/5.3 -> 4.3):
//! 1. Parse CLI, initialize the file logger.
//! 2. (#35) Bind the listener: plain HTTP, httparse request parse, tungstenite
//!    upgrade on `/api/<app>/signaling/`. TLS wraps the accepted stream in #37.
//! 3. (#34) Build the media session: SettingEngine, VP8 track, two data
//!    channels, offer/answer/ICE over the signaling lines.
//! 4. (#36) Start the gated capture/encode loop writing Samples to the track;
//!    periodic host/process stats over the control channel.

mod config;
mod dispatch;
mod http;
mod pipeline;
mod platform;
mod security;
mod signaling;
mod webrtc;

use std::sync::Arc;

/// Stage1 log filter. Quiet the noisy transport internals while keeping webrtc
/// ICE gather errors visible (hbb_common init_log:455 pattern).
const LOG_FILTER: &str = "info,rustls=warn,tungstenite=warn,webrtc-sctp=warn,webrtc=warn,webrtc_ice::agent::agent_internal=error,webrtc::peer_connection=error";

/// Initialize flexi_logger. Stage1 writes to `logs\stream-server.log` under the
/// working directory; rotation/age/size cleanup is deferred to stage4 per
/// design 9.2 #5. The returned handle is held on the task-local root so its
/// lifetime spans the whole process.
fn init_logger(cfg: &config::Config) -> anyhow::Result<Option<hbb_common::flexi_logger::LoggerHandle>> {
    use hbb_common::flexi_logger::{FileSpec, Logger, WriteMode};

    let logger = if let Some(log_file) = &cfg.log_file {
        // try_from keeps a fixed name (no startup timestamp suffix) per design
        // 9.2 #5: logs\stream-server.log; rotation/append semantics are stage4.
        let spec = FileSpec::try_from(log_file.to_owned())?;
        if let Some(parent) = log_file.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        Logger::try_with_str(LOG_FILTER)?
            .log_to_file(spec)
            .write_mode(WriteMode::Direct)
            .start()?
    } else {
        Logger::try_with_str(LOG_FILTER)?
            .log_to_stderr()
            .write_mode(WriteMode::Direct)
            .start()?
    };

    Ok(Some(logger))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // #65: Windows' default timer granularity (~15.6 ms) makes the beat's
    // `sleep` overshoot, which capped the loop at ~57 Hz even with a 5.8 ms beat.
    // One-millisecond resolution puts the settle sleep on target. Since Windows 10
    // the request is scoped to this process.
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::Media::timeBeginPeriod(1);
    }
    let cfg = Arc::new(config::Config::parse(std::env::args().skip(1).collect()));
    // #60: VMware SVGA 3D has no HDR desktop; `--no-hdr` skips the guaranteed
    // DuplicateOutput1 probe and its per-start warning.
    if cfg.no_hdr {
        scrap::dxgi::hdr::UNAVAILABLE.store(true, std::sync::atomic::Ordering::Relaxed);
        log::info!("HDR duplication disabled by --no-hdr (VMware SVGA fallback)");
    }
    let _logger = init_logger(&cfg)?;

    // #94: the codec the process starts on. A name this build cannot encode
    // keeps VP8 and says so — silently accepting it would leave the panel
    // claiming a codec nothing produces.
    match pipeline::codec::VideoCodec::from_wire(&cfg.video_codec) {
        Some(codec) => {
            pipeline::codec::set(codec);
        }
        None => log::warn!(
            "unknown --video-codec {:?}, keeping {} (built in: vp8, vp9)",
            cfg.video_codec,
            pipeline::codec::current().wire()
        ),
    }

    // #88: 监听面鉴权骨架。策略必须在 bind 之前装好 —— 第一个可能被裁决的
    // 事件是 `serve` 收到的 HTTP 请求，装晚了就会出现"前几个请求按默认放行"
    // 的窗口。策略名拼错时退回 allow-all 而不是退出：本项目的默认本来就是
    // 全放行，退出只会让一个拼写错误变成一次停服；但那声 warn 必须留着。
    let policy = security::policy::by_name(&cfg.policy).unwrap_or_else(|| {
        log::warn!(
            "unknown --policy {:?}, falling back to allow-all (builtin: {})",
            cfg.policy,
            security::policy::BUILTIN.join(", ")
        );
        Arc::new(security::AllowAll) as Arc<dyn security::Policy>
    });
    if security::install(policy).is_err() {
        log::warn!(
            "authz policy already installed, keeping {}",
            security::policy_name()
        );
    }
    // 启动本身也算一个被监听的事件：未来可以有"只允许在维护窗口启动"之类的
    // 策略，但现在它的主要作用是让日志里出现第一条 surface，证明链路是通的。
    if !security::allow(security::EventKind::Startup, &security::Principal::default()) {
        anyhow::bail!(
            "startup denied by policy {} (surface=startup)",
            security::policy_name()
        );
    }
    // 措辞刻意避开 `authz deny surface=` 这串：那行是拒绝记录的固定前缀，
    // 提示里带上它会让 `grep -c "authz deny surface="` 永远至少得 1。
    log::info!(
        "authz policy={} surfaces=on (denials are logged with the `authz deny` prefix)",
        security::policy_name()
    );

    log::info!(
        "stream-server stage1 | listen={}:{} tls={} dist={} app={} video={}x{}@{}fps bitrate={}kbps codec={}",
        cfg.host,
        cfg.port,
        cfg.tls_enabled(),
        cfg.dist.display(),
        cfg.app_name,
        cfg.width,
        cfg.height,
        cfg.fps,
        cfg.bitrate_kbps,
        // #94: the codec the first session will negotiate with. The panel can
        // change it later; `pipeline::codec` holds the live value.
        pipeline::codec::current().wire(),
    );
    log::info!(
        "log_file={} | spf={:.3}ms | upload_dir={}",
        cfg.log_file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "stderr".to_owned()),
        cfg.spf().as_secs_f64() * 1000.0,
        cfg.upload_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "disabled".to_owned()),
    );

    // #36 starts the capture loop after Connected notices are sent; the WS
    // listener, media session and DC hook run from this point on.
    let serve = http::serve(cfg);
    tokio::select! {
        result = serve => {
            result?;
        }
        _ = tokio::signal::ctrl_c() => {
            log::info!("stream-server stopped.");
        }
    }
    Ok(())
}
