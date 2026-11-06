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

    // #110: `--video-codec` now only seeds each program's settings — there is
    // no process-wide live value any more. Validated here so a name this build
    // cannot encode is reported once at startup instead of silently becoming
    // VP8 on every program with the panel none the wiser.
    let startup_codec =
        pipeline::codec::VideoCodec::from_wire(&cfg.video_codec).unwrap_or_else(|| {
            log::warn!(
                "unknown --video-codec {:?}, programs will start on vp8 (built in: vp8, vp9)",
                cfg.video_codec
            );
            pipeline::codec::VideoCodec::default()
        });

    // #96: how long a game-mode ghost pointer stays on screen without moving.
    // Announced through `server_settings`, not enforced here — the browser owns
    // the pointer that fades.
    dispatch::ghost::set_idle_secs(cfg.ghost_cursor_idle_secs);
    log::info!(
        "game-mode ghost pointer: fades after {}s without an effective move",
        dispatch::ghost::idle_secs()
    );

    // #88/#110: 监听面鉴权骨架。策略必须在 bind 之前装好 —— 第一个可能被裁决的
    // 事件是 `serve` 收到的 HTTP 请求，装晚了就会出现"前几个请求按默认放行"
    // 的窗口。策略名拼错时退回 allow-all 而不是退出：本项目的默认本来就是
    // 全放行，退出只会让一个拼写错误变成一次停服；但那声 warn 必须留着。
    //
    // `auto` 是默认值，也是唯一需要看配置才能定下来的名字：
    //   * 没配令牌表 → `allow-all`。单用户机器上"没有身份"就是"就是主人"，
    //     装 `role-based` 会把它自己的键鼠挡掉（匿名身份是 `viewer`）。
    //   * 配了令牌表 → `role-based`。此时每个过门的人都有真身份，让策略去读它
    //     才是"凭据带身份"的意义；否则令牌只决定"能不能进"，进去以后所有人
    //     一样 —— #110 之前正是这个形态。
    let resolved_policy = if cfg.policy.trim().eq_ignore_ascii_case("auto") {
        if cfg.token_gate().is_some() {
            "role-based"
        } else {
            "allow-all"
        }
    } else {
        cfg.policy.as_str()
    };
    let policy = security::policy::by_name(resolved_policy).unwrap_or_else(|| {
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

    // #101: 口令状态必须每次都写进日志，且两种状态各写一句。
    // "没开"和"开了但你没带 token"在浏览器里长得一模一样（一片空白），
    // 唯一的区分点就是这两行。
    //
    // #110: 判据从 `auth_token` 换成整张令牌表 —— 只用 `--auth-grant` 的部署
    // 以前会在日志里说"没开鉴权"，而实际上 401 已经挡在前面了。
    match cfg.token_gate() {
        Some(gate) => log::info!(
            "auth token REQUIRED on /api/* ({} token(s), policy {}) | open the page as {}://<host>:{}?token=<token> \
             (static assets and /api/status stay open)",
            gate.len(),
            security::policy_name(),
            if cfg.tls_enabled() { "https" } else { "http" },
            cfg.port,
        ),
        None => log::info!(
            "auth token disabled | anyone who can reach port {} can drive this machine \
             (set --auth-token <password> to require one)",
            cfg.port,
        ),
    }
    // #110: 报出**身份表**（只有 `tenant:role`，没有令牌本身）。多令牌部署里
    // "我这张令牌是哪个租户的"是排障第一问，而令牌不能进日志 —— 这两条一起
    // 决定了这里只能打身份，不能打口令。
    if !cfg.auth_grants.is_empty() {
        let mut listing: Vec<String> = Vec::new();
        if cfg.auth_token.is_some() {
            listing.push(format!("{}:{} (--auth-token)", crate::security::TenantId::DEFAULT, "host"));
        }
        for spec in &cfg.auth_grants {
            listing.push(format!("{}:{}", spec.tenant.as_str(), spec.role.as_str()));
        }
        log::info!("auth identity table: {}", listing.join(", "));
    }

    // #95: hardware-encoder reservation. The probe is advisory — this build has
    // no hardware encoder — but it answers the two questions a support report
    // actually asks: "does this machine have an encoder" and "why is the stream
    // software". Probed once, before the listener binds, so the answer is in the
    // log even when the process dies a second later.
    let hw = platform::hwenc::probe();
    log::info!("encoder probe | {}", hw.summary());
    for line in hw.detail() {
        log::info!("encoder probe | {}", line);
    }
    let requested = platform::hwenc::Requested::from_name(&cfg.encoder).unwrap_or_else(|| {
        log::warn!(
            "unknown --encoder {:?}, using auto (builtin: auto, software, nvenc, amf, qsv)",
            cfg.encoder
        );
        platform::hwenc::Requested::Auto
    });
    let (backend, note) = platform::hwenc::decide(requested, &hw)?;
    log::info!("encoder backend={:?} | {}", backend, note);

    // #109: 麦克风写哪个端点。和编码器探测同一个理由——"这台机器有没有能当
    // 麦克风的虚拟设备"是排障会问的第一个问题，答案必须进日志，哪怕进程一秒后
    // 就退出。决议装进进程，麦克风线程和 health 读同一份，避免日志说的端点和
    // 实际写入的端点各说各话。
    let endpoints = platform::virtual_mic::probe();
    log::info!("microphone probe | {}", endpoints.summary());
    for line in endpoints.detail() {
        log::info!("microphone probe | {}", line);
    }
    let mic_target = platform::virtual_mic::resolve(
        &cfg.mic_device,
        &endpoints.render,
        &endpoints.capture,
        endpoints.default_render.as_deref().unwrap_or(""),
        endpoints.parsec_driver,
    );
    if mic_target.usable {
        log::info!(
            "microphone target | provider={} sink={:?} capture={:?} | {}",
            mic_target.provider.as_str(),
            mic_target.sink.as_deref().unwrap_or("(default endpoint)"),
            mic_target.capture.as_deref().unwrap_or(""),
            mic_target.note
        );
    } else {
        // 不可用不是启动失败：视频、输入、文件传输与它无关，把它降级成一条
        // 明确的告警比让整个服务起不来有用得多。
        log::warn!(
            "microphone target | provider={} unusable | {}",
            mic_target.provider.as_str(),
            mic_target.note
        );
    }
    platform::virtual_mic::install(mic_target);

    // P1：把系统默认录音设备切到虚拟麦。
    //
    // 放在启动、不在"第一个麦克风包到达时"，是因为游戏在启动时读一次默认设备
    // 并缓存 —— 等用户点麦克风才切，游戏早就记住了物理麦。`off` 完全不碰系统
    // 设置。失败只降级成告警：视频、输入、文件传输都不依赖它，为了"改不了默认
    // 录音设备"让整个服务起不来是本末倒置。
    let mic_policy = platform::default_mic::Policy::from_name(&cfg.mic_default_device)
        .unwrap_or(platform::default_mic::Policy::Auto);
    // 原值落在日志目录旁边，和日志同生共死：`--log` 指到哪就跟着到哪。停止服务
    // 用的是 Stop-Process -Force，进程没有清理机会，靠这个文件把原值带过下一次
    // 启动，否则一次强杀就让默认录音永远停在虚拟麦上。
    let mic_state = cfg
        .log_file
        .as_ref()
        .and_then(|p| p.parent())
        .map(|dir| dir.join("mic-default.json"));
    if let Some(target) = platform::virtual_mic::target() {
        let mic_default =
            platform::default_mic::apply(mic_policy, target, mic_state.as_deref());
        if mic_default.action == "failed" {
            log::warn!("microphone default device | {}", mic_default.summary());
        } else {
            log::info!("microphone default device | {}", mic_default.summary());
        }
        for line in &mic_default.previous {
            log::info!("microphone default device | previous {line}");
        }
    }

    // #95: the software knob set. Same policy as `--video-codec`: a name this
    // build does not know keeps the default and says so, because the only
    // reason to pass this flag is to compare against old numbers — silently
    // running the new set would fake the comparison.
    match cfg.tune.trim().to_ascii_lowercase().as_str() {
        "tuned" | "default" | "" => {}
        "legacy" | "none" | "off" => {
            pipeline::encoder::set_legacy_tuning(true);
            log::warn!("--tune legacy: pre-#95 encoder settings (diagnostic only)");
        }
        other => log::warn!(
            "unknown --tune {:?}, keeping tuned (builtin: tuned, legacy)",
            other
        ),
    }
    log::info!(
        "encoder tuning | buf={}ms err_res={} screen={} intra_cap={}% (--tune legacy reproduces pre-#95)",
        pipeline::encoder::tuning().buf_sz_ms,
        pipeline::encoder::tuning().error_resilient,
        pipeline::encoder::tuning().screen_content,
        pipeline::encoder::tuning().max_intra_pct,
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
        // #110: the codec the first session will negotiate with. The panel can
        // change it later; the live value now lives on each program.
        startup_codec.wire(),
    );
    log::info!(
        "log_file={} | spf={:.3}ms | upload_dir={} | upload_quota={}",
        cfg.log_file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "stderr".to_owned()),
        cfg.spf().as_secs_f64() * 1000.0,
        // #110: 目录形状变了 —— 每个租户一份子目录。把这件事写进启动日志，因为
        // 旧部署里的文件现在不在 `GET /api/files/` 能看到的位置上了，而"文件
        // 消失了"与"目录换了一层"的排障成本差很远。
        cfg.upload_dir
            .as_ref()
            .map(|p| format!("{} (per tenant: {}/<tenant>)", p.display(), p.display()))
            .unwrap_or_else(|| "disabled".to_owned()),
        if cfg.upload_quota_mb == 0 {
            "unlimited".to_owned()
        } else {
            format!("{} MiB/tenant", cfg.upload_quota_mb)
        },
    );

    // #36 starts the capture loop after Connected notices are sent; the WS
    // listener, media session and DC hook run from this point on.
    let serve = http::serve(cfg);
    let outcome = tokio::select! {
        result = serve => result,
        _ = tokio::signal::ctrl_c() => {
            log::info!("stream-server stopping (ctrl-c).");
            Ok(())
        }
    };
    // P1：无论走哪条路退出（正常返回 / Ctrl+C），都把默认录音设备还回去。
    // `taskkill /F` 不会走到这里 —— 那时默认录音会停在虚拟麦上，表现是"本地
    // 麦克风没声音"，见 `platform::default_mic` 的模块文档。
    platform::default_mic::restore();
    outcome
}
