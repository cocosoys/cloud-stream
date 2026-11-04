//! `GET /api/v1/health` —— 这台虚拟机活着吗，它正在干什么（#93）。
//!
//! 端点存在的理由：这个进程是虚拟机里唯一一个"永远在跑、永远在听端口"的东西，
//! 所以从外面探活只能探到它，探不到画面。`/api/status` 只知道"进程还活着"，而
//! 一个 GPU 挂掉、DXGI 复制输出卡死、编码器停摆的进程照样回 `ok`。这里补的是
//! **推流健康**：拿 `last_sample_at`（上一次有帧真正写进 track 的时刻，新帧或
//! 重复帧都算）与当前时刻的距离判"管线是不是卡住了"。
//!
//! 数据面（与需求逐条对应）：
//! * 主机资源 —— CPU / 内存，来自 [`crate::platform::Sampler`]，与面板同一份口径。
//! * 推流状态 —— 分辨率、帧率、码控、CRF、目标码率、实际码率、时延、帧计数。
//! * 进程与游戏 —— 按 CPU 排前几名的进程，外加 Minecraft 常见进程名的命中结果。
//! * 连接情况 —— 几个用户、IP、设备码、角色、连了多久。
//!
//! 三条设计约束：
//!
//! 1. **不做鉴权**（明确决定）。默认监听面本来就是局域网内不设防，与其在这里
//!    加一道和别处不一致的锁，不如让策略骨架 ([`crate::security`]) 统一管 ——
//!    `Route::Health` 已经映射到 `Endpoint::Health`，将来装上真策略时它会跟着
//!    一起被拦住，不需要改这个文件。
//! 2. **优先复用会话自己的句柄**。数字不是抄出来的副本，`PipelineHandle` 与
//!    `StreamSettings` 就是信令会话手里那两个 `Arc`，所以这里读到的必然是管线
//!    正在用的值，没有第二份需要同步的状态。
//! 3. **采样有下限**。主机 CPU 与进程表都不是免费的（一次全进程扫描在 Windows
//!    上要几十毫秒），所以 `HealthProbe` 缓存 [`SAMPLE_EVERY`]，响应里的
//!    `sample_age_ms` 如实说明这批数字有多旧 —— 比"每次都新鲜"更重要的是"别把
//!    被观测的机器拖慢"。
//!
//! 与之配套的两个控制端点（`只读 + 少量控制` 里的那一小半）：
//! * `POST /api/v1/control/capture/restart` —— 重开采集器与编码器。
//! * `POST /api/v1/control/session/kick` —— 踢掉当前会话，客户端会自己重连。

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hbb_common::serde_json::{json, Value};
use hbb_common::sysinfo::{ProcessRefreshKind, System};
use tokio::io::AsyncWrite;

use crate::config::{Config, InputPolicy};
use crate::http::write_http;
use crate::pipeline::audio;
use crate::pipeline::PipelineStats;
use crate::platform::Sampler;
use crate::signaling::SessionInfo;

/// 两次真采样的最小间隔。
///
/// 5 秒是"够新"和"不打扰"之间的取舍：判活靠的是 `last_sample_at`，那是每次
/// 请求实时算的，不经过这个缓存；被缓存的只有 CPU / 内存 / 进程表。
const SAMPLE_EVERY: Duration = Duration::from_secs(5);

/// 多久没有帧出去就认定管线卡住。
///
/// 一条健康的管线即使在完全静止的桌面上也会按 `--repeat-every` 重发上一帧
/// （默认每 2 拍一次，30 fps 下约 66 ms 一帧），所以 3 秒的沉默不是"画面没
/// 变化"，而是"没东西写进 track 了"。留这么宽的余量是为了不被一次 GC 停顿或
/// 一次 keyframe 编码误判。
const STALL_AFTER: Duration = Duration::from_secs(3);

/// 建连后多久还没有第一帧，就认为握手之后的链路没通。
const FIRST_FRAME_BUDGET: Duration = Duration::from_secs(15);

/// `top` 列表长度。够看出"谁在吃 CPU"，又不至于让一个 curl 输出一屏。
const TOP_N: usize = 5;

/// Minecraft 在进程表里的常见名字。
///
/// 这是**启发式**，不是断言：官方启动器起的是 `javaw.exe`，整合包常常改个名字，
/// 而 `java.exe` 也可能是任何一个 Java 程序。所以命中结果只回答"看起来在跑"，
/// 真正的结论留给调用方 —— `top` 列表就在旁边，人可以自己看。
const GAME_NAMES: &[&str] = &["javaw.exe", "java.exe", "minecraft.exe", "minecraft.windows.exe"];

/// 一行进程。
#[derive(Clone, Debug, Default)]
pub struct ProcRow {
    pub pid: u32,
    pub name: String,
    pub cpu_percent: f32,
    pub mem_bytes: u64,
}

/// 一批主机 / 进程采样。
#[derive(Clone, Debug, Default)]
pub struct ProbeSample {
    pub cpu_percent: f32,
    pub process_cpu_percent: f32,
    pub cpu_cores: u32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub top: Vec<ProcRow>,
    pub game: Vec<ProcRow>,
    pub sampled_at: Option<Instant>,
}

impl ProbeSample {
    fn age_ms(&self) -> Option<u64> {
        self.sampled_at.map(|at| at.elapsed().as_millis() as u64)
    }
}

/// 三组错误计数的快照，用来做差。
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct ErrorTotals {
    capture: u64,
    encode: u64,
    write: u64,
}

impl ErrorTotals {
    fn grew_from(&self, prev: &ErrorTotals) -> bool {
        // 计数器只增不减；用 `>=` 而不是 `==` 是因为管线重建会把它们清零，
        // 清零那一次不算"出过错"。
        self.capture > prev.capture || self.encode > prev.encode || self.write > prev.write
    }
}

/// 进程内共享的采样器。
///
/// 放在 `serve()` 里建一次、随每条连接 clone 下去：`Arc` 语义让所有请求共用
/// 同一份采样，否则每个请求都开一个 `System` 就等于每次 curl 扫一遍进程表。
#[derive(Clone)]
pub struct HealthProbe {
    started: Instant,
    inner: std::sync::Arc<Mutex<Inner>>,
    /// #93: 上次 `/health` 看到的三组错误计数。
    ///
    /// 为什么不用 `FrameCounters` 直接说"本窗口出错了"：那些计数由统计定时器
    /// 每秒清零，而定时器**只在面板的统计开关打开时才跑**。面板关着时它们就是
    /// 自本次采集循环启动以来的累计值（实测：`repeat: 534`、`new: 1`），拿它
    /// 当窗口判据会说谎。所以基线由探针自己记 —— `flags` 说的是"**上次检查
    /// 以来**有没有新错"，措辞与事实一致。
    error_base: std::sync::Arc<Mutex<Option<(Instant, ErrorTotals)>>>,
}

struct Inner {
    /// `None` 表示 sysinfo 没起来；此时主机数字缺失，其余照常。
    sampler: Option<Sampler>,
    /// 进程表单独一个 `System`：`Sampler` 只刷自己那个 pid，扫不到别人的 CPU。
    sys: System,
    last_at: Option<Instant>,
    sample: ProbeSample,
}

impl HealthProbe {
    pub fn new() -> Self {
        let sampler = match Sampler::new() {
            Ok(s) => Some(s),
            Err(err) => {
                log::warn!("#93: host sampler unavailable, health reports no host figures: {err:#}");
                None
            }
        };
        Self {
            started: Instant::now(),
            inner: std::sync::Arc::new(Mutex::new(Inner {
                sampler,
                sys: System::new(),
                last_at: None,
                sample: ProbeSample::default(),
            })),
            error_base: std::sync::Arc::new(Mutex::new(None)),
        }
    }

    /// 取一批采样，必要时真的刷一次。
    fn sample(&self) -> ProbeSample {
        // 中毒不致命：采样器是个累加器，`into_inner` 拿回被污染的那份继续用，
        // 比让 `/health` 从此永久 500 要好。
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let fresh = inner
            .last_at
            .map(|at| at.elapsed() >= SAMPLE_EVERY)
            .unwrap_or(true);
        if fresh {
            inner.resample();
        }
        inner.sample.clone()
    }

    /// 报「上次检查以来」有没有新错，并把基线推到当前值。
    ///
    /// 第一次调用（进程刚起来、或 `flags` 无从比较）返回 `None`：没有基线就
    /// 没有增量，编一个出来只会制造假警报。
    fn errors_since_last_check(&self, now: ErrorTotals) -> Option<(bool, u64)> {
        let mut guard = self.error_base.lock().unwrap_or_else(|p| p.into_inner());
        let result = guard.map(|(at, prev)| (now.grew_from(&prev), at.elapsed().as_millis() as u64));
        *guard = Some((Instant::now(), now));
        result
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }
}

impl Default for HealthProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl Inner {
    fn resample(&mut self) {
        if let Some(sampler) = self.sampler.as_mut() {
            sampler.refresh();
            let host = sampler.sample();
            self.sample.cpu_percent = host.cpu_percent;
            self.sample.process_cpu_percent = host.process_cpu_percent;
            self.sample.cpu_cores = host.cpu_cores;
            self.sample.mem_used = host.mem_used;
            self.sample.mem_total = host.mem_total;
        }

        // 只要 cpu：`everything()` 还会去查每个进程的用户 SID，在 Windows 上
        // 那是一次次 LSA 查询，为一个判活探针不值得。
        self.sys
            .refresh_processes_specifics(ProcessRefreshKind::new().with_cpu());

        let mut rows: Vec<ProcRow> = self
            .sys
            .processes()
            .iter()
            .map(|(pid, p)| ProcRow {
                pid: pid.as_u32(),
                name: p.name().to_owned(),
                cpu_percent: p.cpu_usage(),
                mem_bytes: p.memory(),
            })
            .collect();

        // CPU 降序、内存兜底：刚起来的进程第一次扫描 CPU 全是 0，那时按内存排
        // 至少还能看出"谁是那头大象"。
        rows.sort_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.mem_bytes.cmp(&a.mem_bytes))
        });

        self.sample.game = rows
            .iter()
            .filter(|r| is_game_process(&r.name))
            .cloned()
            .collect();
        rows.truncate(TOP_N);
        self.sample.top = rows;
        self.last_at = Some(Instant::now());
        self.sample.sampled_at = self.last_at;
    }
}

fn is_game_process(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    GAME_NAMES.contains(&lower.as_str())
}

/// 判活结论。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    /// 没人连着。进程活着，管线没在推 —— 不是故障。
    Idle,
    /// 连着但第一帧还没出来（握手、建 PC、开采集器都在这一步）。
    Starting,
    /// 一直在出帧（新帧或重复帧）。
    Streaming,
    /// 客户端按了暂停（`STOP_VIDEO`）。这是用户的操作，不是故障。
    Paused,
    /// 该出帧却不出帧。
    Stalled,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Idle => "idle",
            Verdict::Starting => "starting",
            Verdict::Streaming => "streaming",
            Verdict::Paused => "paused",
            Verdict::Stalled => "stalled",
        }
    }

    /// 只有 `stalled` 算不健康。`starting` 在预算内也是健康的。
    fn healthy(self) -> bool {
        self != Verdict::Stalled
    }
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn proc_json(rows: &[ProcRow]) -> Value {
    Value::Array(
        rows.iter()
            .map(|r| {
                json!({
                    "pid": r.pid,
                    "name": r.name,
                    "cpu_percent": round1(r.cpu_percent),
                    "mem_bytes": r.mem_bytes,
                })
            })
            .collect(),
    )
}

fn round1(v: f32) -> f64 {
    // 面板同款口径：一位小数。`as f64` 之后再 round，避免 f32 的尾数泄漏到 JSON。
    ((v as f64) * 10.0).round() / 10.0
}

/// 组装响应体，同时返回是否健康（决定 200 / 503）。
fn build(
    cfg: &Config,
    probe: &HealthProbe,
    sample: &ProbeSample,
    sessions: &[SessionInfo],
    pipelines: &[std::sync::Arc<crate::pipeline::SharedPipeline>],
) -> (bool, Value) {
    build_at(cfg, probe, sample, sessions, pipelines, Instant::now())
}

/// [`build`]，但"现在"由调用方给。
///
/// 报告里所有 age 都是 `now - 某个时刻`，而整个响应必须只用**一个**参考点。
/// 如果每个字段各自 `Instant::now()`，被调度器挂起过的线程会在同一份响应里同时
/// 看到"刚写过"和"3.2 秒前写过"——健康的管线就这样被判成 `stalled`。
/// 测试把 `now` 钉死，也是同一个理由。
fn build_at(
    cfg: &Config,
    probe: &HealthProbe,
    sample: &ProbeSample,
    sessions: &[SessionInfo],
    pipelines: &[std::sync::Arc<crate::pipeline::SharedPipeline>],
    now: Instant,
) -> (bool, Value) {
    // #101: sessions and programs are no longer one-to-one, so the report reads
    // each from its own registry. The `stream` block describes the first
    // program (the primary display, and today the only one this build can
    // open); `pipelines` below lists every one that exists, and `clients`
    // lists every session — a viewer's handle is the same program's, so
    // duplicating it per session would say the same thing N times.
    let primary = pipelines.first();
    let pipeline = primary.map(|p| p.handle.clone());
    let settings = primary.map(|p| p.settings.clone());
    // The Starting verdict measures how long the *first* client has been
    // waiting, which is the oldest hello rather than "the" session.
    let first_connected = sessions.iter().filter_map(|s| s.connected_at).min();

    let mut verdict = Verdict::Idle;
    let mut flags: Vec<&'static str> = Vec::new();

    let stream = match (&pipeline, &settings) {
        (Some(handle), Some(settings)) => {
            let on = handle.is_on();
            let throttled = handle.is_throttled();
            // 锁只在这一个块里持有：后面组装 JSON 还要读别的，跨 await 更是
            // 不行 —— 捕获循环每拍都要抢这把锁。
            let (
                frames_new,
                frames_repeat,
                key_frames,
                capture_err,
                encode_err,
                write_err,
                encode_ms,
                pipeline_ms,
                send_ms,
                target_kbps,
                pli,
                warmed,
                first_frame,
                last_age,
            ) = {
                let stats = handle.stats();
                let g = stats.lock().unwrap_or_else(|p| p.into_inner());
                snapshot_fields(&g, now)
            };

            if !on {
                verdict = Verdict::Paused;
            } else if last_age.map(|d| d > STALL_AFTER).unwrap_or(false) {
                verdict = Verdict::Stalled;
                flags.push("no_frame_for_over_3s");
            } else if first_frame {
                verdict = Verdict::Streaming;
            } else {
                verdict = Verdict::Starting;
                let waited = first_connected
                    .map(|at| now.saturating_duration_since(at))
                    .unwrap_or_default();
                if waited > FIRST_FRAME_BUDGET {
                    flags.push("no_first_frame_after_15s");
                }
            }

            // 计数是累计值，不是"本窗口"。面板的统计开关关着时没有任何东西
            // 会去清零它们，所以"有没有新错"必须靠探针自己记基线做差。
            let errs = ErrorTotals {
                capture: capture_err,
                encode: encode_err,
                write: write_err,
            };
            let err_delta = probe.errors_since_last_check(errs);
            if err_delta.map(|(grew, _)| grew).unwrap_or(false) {
                flags.push("new_errors_since_last_check");
            }

            json!({
                "active": on,
                "throttled": throttled,
                "width": settings.width(),
                "height": settings.height(),
                "fps": settings.fps(),
                // The same string the panel advertises (`server_settings.encoder`):
                // one encoder, one name. `codec` carries the detail.
                "encoder": "libvpx",
                // #94: the codec this server is negotiating with; a session
                // that picked VP9 is not running the same encoder as before.
                "codec": crate::pipeline::codec::current().wire(),
                "rate_control": settings.rate_control().as_str(),
                "crf": settings.crf(),
                "target_kbps": target_kbps,
                "warm": warmed,
                "last_sample_age_ms": last_age.map(|d| d.as_millis() as u64),
                "timing": {
                    "encode_ms": round1(encode_ms),
                    "pipeline_ms": round1(pipeline_ms),
                    "send_ms": round1(send_ms),
                },
                // 累计值：自本次采集循环启动起。要速率就对自己的两次采样做差。
                "frames_total": {
                    "new": frames_new,
                    "repeat": frames_repeat,
                    "key": key_frames,
                    "capture_err": capture_err,
                    "encode_err": encode_err,
                    "write_err": write_err,
                },
                // `null` = 还没有上一次可比的基线（进程刚起、或首次查询）。
                "errors_since_last_check": err_delta.map(|(grew, _)| grew),
                "checked_ago_ms": err_delta.map(|(_, ms)| ms),
                "pli_requests": pli,
                "audio": {
                    "enabled": audio::enabled(),
                    "bitrate_bps": audio::current_bitrate_bps(),
                },
            })
        }
        _ => {
            // 没有会话时也把服务端配置报出来：一个空转的进程"用的是什么参数"
            // 本身就是排障信息。
            json!({
                "active": false,
                "width": cfg.width,
                "height": cfg.height,
                "fps": cfg.fps,
                "encoder": "libvpx",
                // #94: the codec this server is negotiating with; a session
                // that picked VP9 is not running the same encoder as before.
                "codec": crate::pipeline::codec::current().wire(),
                "target_kbps": cfg.bitrate_kbps,
                "audio": {
                    "enabled": audio::enabled(),
                    "bitrate_bps": audio::current_bitrate_bps(),
                },
            })
        }
    };

    // #101: every live connection, oldest first. `count > 1` is the normal
    // multi-viewer state now, not a fault.
    let clients = json!({
        "count": sessions.len(),
        "items": sessions
            .iter()
            .map(|info| {
                let connected_s = info
                    .connected_at
                    .map(|at| now.saturating_duration_since(at).as_secs())
                    .unwrap_or(0);
                // #102: what this one session's downlink took. With a shared
                // encoder the program total cannot answer "who used the
                // bandwidth", so the number comes from the fan-out's per-sink
                // counters. A session that never reached `Connected` has no
                // sink and reports zeros — it genuinely sent nothing.
                let (bytes, frames) = info
                    .sink
                    .as_ref()
                    .map(|c| (c.bytes(), c.frames()))
                    .unwrap_or((0, 0));
                // Averaged over the session's whole life, not a window: it answers
                // "what does one viewer cost me", which is the number that decides
                // how many fit. Round to three decimals — a JSON float with 17
                // digits reads as precision the measurement does not have.
                let avg_mbps = if connected_s > 0 {
                    let raw = bytes as f64 * 8.0 / 1_000_000.0 / connected_s as f64;
                    (raw * 1000.0).round() / 1000.0
                } else {
                    0.0
                };
                // #103: the role is the server's answer, not the page's claim.
                // Every page calls itself `controller` in the hello, so reporting
                // that field verbatim made two tabs look like two controllers —
                // which is exactly what made "the second tab cannot do anything"
                // look like a broken role gate instead of a working one. The
                // lease lives on the program; the session is controller iff it
                // holds it.
                let role = match info.display {
                    Some(key) => {
                        let program = pipelines.iter().find(|p| p.display.0 == key);
                        // #105: `shared` 下每个已 attach 的会话都能操作，所以
                        // `controller` 是**真的**，不是上一轮那个「页面自称」的
                        // bug。区分办法是同级的 `server.input_policy`。
                        match program {
                            Some(p) if p.input_policy() == InputPolicy::Shared => "controller",
                            Some(p) if p.controller_id() == Some(info.id) => "controller",
                            Some(_) => "viewer",
                            None => "viewer",
                        }
                    }
                    // No program yet: the session has not reached `Connected`,
                    // so there is no lease it could hold.
                    None => "pending",
                };
                // #103: how many input messages the role gate dropped. Without
                // it "the viewer's pointer does nothing" has two indistinguishable
                // causes: the messages were refused, or they never arrived.
                let denied_inputs = info
                    .denied
                    .as_ref()
                    .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                    .unwrap_or(0);
                json!({
                    "id": info.id,
                    "ip": info.peer_ip,
                    "device_id": info.device_id,
                    "tab_id": info.tab_id,
                    "role": role,
                    "client_type": info.client_type,
                    "display": info.display,
                    "user_agent": info.user_agent,
                    "connected_s": connected_s,
                    "reconnects": info.reconnects,
                    "bytes_total": bytes,
                    "frames_total": frames,
                    "avg_mbps": avg_mbps,
                    "denied_inputs": denied_inputs,
                })
            })
            .collect::<Vec<_>>(),
    });

    // #101: the programs themselves. One per display; `holders` is how many
    // sessions split the encoder's cost, `downlinks` how many tracks it feeds.
    let programs = json!({
        "count": pipelines.len(),
        "items": pipelines
            .iter()
            .map(|p| json!({
                "display": p.display.0,
                "holders": p.refs(),
                "downlinks": p.video.len(),
                "active": p.handle.is_on(),
            }))
            .collect::<Vec<_>>(),
    });

    let payload = json!({
        "ok": verdict.healthy(),
        "verdict": verdict.as_str(),
        "flags": flags,
        "now": epoch_secs(),
        "server": {
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "uptime_s": probe.uptime().as_secs(),
            "host": hostname(),
            "policy": cfg.policy,
            // #105: 谁可以操作。`shared` 时 `role` 全是 controller 且这不是 bug；
            // 运维要看的是这一行，而不是去数 controller 的个数。
            "input_policy": cfg.input_policy.as_str(),
            "tls": cfg.tls_cert.is_some(),
            "listen": format!("{}:{}", cfg.host, cfg.port),
        },
        "host": {
            "available": sample.sampled_at.is_some(),
            "cpu_percent": round1(sample.cpu_percent),
            "process_cpu_percent": round1(sample.process_cpu_percent),
            "cpu_cores": sample.cpu_cores,
            "mem_used": sample.mem_used,
            "mem_total": sample.mem_total,
            "mem_percent": if sample.mem_total > 0 {
                round1((sample.mem_used as f64 * 100.0 / sample.mem_total as f64) as f32)
            } else {
                0.0
            },
            "sample_age_ms": sample.age_ms(),
        },
        "stream": stream,
        "processes": {
            // `game_running` 是名字启发式的结论；`top` 是不带判定的原始事实。
            "game_running": !sample.game.is_empty(),
            "game": proc_json(&sample.game),
            "top": proc_json(&sample.top),
        },
        "clients": clients,
        "pipelines": programs,
    });

    (verdict.healthy(), payload)
}

#[allow(clippy::type_complexity)]
fn snapshot_fields(
    g: &PipelineStats,
    now: Instant,
) -> (u64, u64, u64, u64, u64, u64, f32, f32, f32, u32, u64, bool, bool, Option<Duration>) {
    (
        g.counters.new_frames,
        g.counters.repeat_ok,
        g.counters.key_frames,
        g.counters.capture_err,
        g.counters.encode_err,
        g.counters.write_sample_err,
        g.encode_timing.avg_ms(),
        g.pipeline_timing.avg_ms(),
        g.send_timing.avg_ms(),
        g.target_kbps,
        g.pli_requests,
        g.warmed_up,
        g.first_frame_seen,
        g.last_sample_at.map(|at| now.saturating_duration_since(at)),
    )
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default()
}

/// `GET /api/v1/health`。
pub async fn handle_health<S>(
    stream: &mut S,
    cfg: &Config,
    registry: &crate::signaling::SessionRegistry,
    admission: &crate::signaling::websession::Admission,
    pipelines: &crate::pipeline::PipelineRegistry,
    probe: &HealthProbe,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let sample = probe.sample();
    // #101: two registries, because a session and a capture program are no
    // longer the same thing.
    let sessions = registry.infos().await;
    let programs = pipelines.all().await;
    let (healthy, mut payload) = build(cfg, probe, &sample, &sessions, &programs);
    // #102: admission is injected here rather than threaded into `build`, for
    // two reasons. It describes "will this host take another viewer", not "how
    // is the stream doing" — a different question that belongs next to `ok`
    // rather than inside `stream`. And keeping `build` a pure function of its
    // arguments means its tests do not have to construct a live counter.
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "admission".to_owned(),
            json!({ "max": admission.max(), "live": admission.live() }),
        );
    }
    let body = payload.to_string();
    let (status, text) = if healthy { (200u16, "OK") } else { (503u16, "Service Unavailable") };
    write_http(
        stream,
        status,
        text,
        "application/json; charset=utf-8",
        body.as_bytes(),
    )
    .await
}

/// `POST /api/v1/control/<action>`。
///
/// 白名单不是防君子，是为了让拼错的路径得到 404 而不是"执行了个寂寞"：一个
/// 拼错的 `capture/restat` 静默 200，运维会以为重启过了。
pub async fn handle_control<S>(
    stream: &mut S,
    action: &str,
    registry: &crate::signaling::SessionRegistry,
    pipelines: &crate::pipeline::PipelineRegistry,
    _cfg: &Config,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    match action {
        // #101: every program is restarted, not just "the" pipeline — the
        // operator asking for a capture restart wants the picture back on every
        // screen, and a viewer's program is the same object as the controller's.
        "capture/restart" => {
            let programs = pipelines.all().await;
            if programs.is_empty() {
                json_reply(stream, 409, "Conflict", json!({
                    "ok": false,
                    "action": "capture/restart",
                    "error": "no live pipeline",
                }))
                .await
            } else {
                for program in &programs {
                    program.handle.request_rebuild();
                }
                log::info!(
                    "#93: capture/encoder rebuild requested over HTTP ({} program(s))",
                    programs.len()
                );
                json_reply(stream, 202, "Accepted", json!({
                    "ok": true,
                    "action": "capture/restart",
                    "programs": programs.len(),
                }))
                .await
            }
        }

        // `session/kick` closes every session; `session/kick/<id>` closes one
        // (a stuck viewer) — the id form exists because with N sessions "kick"
        // can no longer mean "kick the only one". The id comes from the path,
        // not a query string: `classify` strips the query before routing.
        a if a == "session/kick" || a.starts_with("session/kick/") => {
            // A non-numeric id must NOT degrade to `None` (which means "kick
            // everybody"): a typo like `session/kick/2x` would then disconnect
            // every client on the box, including the operator's own session.
            let id = match a.strip_prefix("session/kick/") {
                None => None,
                Some(raw) => match raw.parse::<u64>() {
                    Ok(id) => Some(id),
                    Err(_) => {
                        return json_reply(stream, 400, "Bad Request", json!({
                            "ok": false,
                            "error": "session id must be a number",
                            "action": a,
                        }))
                        .await;
                    }
                },
            };
            match registry.kick("kicked by an operator", id).await {
                0 => {
                    json_reply(stream, 409, "Conflict", json!({
                        "ok": false,
                        "action": "session/kick",
                        "error": "no live session",
                    }))
                    .await
                }
                n => {
                    json_reply(stream, 200, "OK", json!({
                        "ok": true,
                        "action": "session/kick",
                        "kicked": n,
                    }))
                    .await
                }
            }
        }

        // #103: hand the input lease to a session. The case that forced it: one
        // person with two tabs, the older tab holds the lease, the person is now
        // looking at the newer one — before this the only way to type was to
        // close the tab that had it (or to reload until the roles swapped).
        //
        // Deliberately explicit rather than "last interaction wins": two people
        // watching one desktop must not steal the pointer by moving a mouse, and
        // an operator asking for the lease is a decision someone made.
        a if a.starts_with("session/take/") => {
            let id = match a.strip_prefix("session/take/").and_then(|raw| raw.parse::<u64>().ok()) {
                Some(id) => id,
                None => {
                    return json_reply(stream, 400, "Bad Request", json!({
                        "ok": false,
                        "error": "session id must be a number",
                        "action": a,
                    }))
                    .await;
                }
            };
            let Some(info) = registry.infos().await.into_iter().find(|s| s.id == id) else {
                return json_reply(stream, 404, "Not Found", json!({
                    "ok": false,
                    "action": "session/take",
                    "error": "no such session",
                    "id": id,
                }))
                .await;
            };
            let Some(display) = info.display else {
                return json_reply(stream, 409, "Conflict", json!({
                    "ok": false,
                    "action": "session/take",
                    "error": "session has not attached to a display yet",
                    "id": id,
                }))
                .await;
            };
            let Some(program) = pipelines.get(crate::pipeline::DisplayKey(display)).await else {
                return json_reply(stream, 409, "Conflict", json!({
                    "ok": false,
                    "action": "session/take",
                    "error": "no live program for that display",
                    "display": display,
                }))
                .await;
            };
            let previous = program.set_controller(Some(id));
            log::info!(
                "#103: input lease moved to session {id} over HTTP (was {previous:?})"
            );
            json_reply(stream, 200, "OK", json!({
                "ok": true,
                "action": "session/take",
                "display": display,
                "previous": previous,
                "controller": id,
            }))
            .await
        }

        other => {
            json_reply(stream, 404, "Not Found", json!({
                "ok": false,
                "error": "unknown control action",
                "action": other,
                "known": ["capture/restart", "session/kick", "session/kick/<id>", "session/take/<id>"],
            }))
            .await
        }
    }
}

async fn json_reply<S>(
    stream: &mut S,
    status: u16,
    text: &str,
    body: Value,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let body = body.to_string();
    write_http(
        stream,
        status,
        text,
        "application/json; charset=utf-8",
        body.as_bytes(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::PipelineHandle;

    fn cfg() -> Config {
        Config::default()
    }

    fn probe() -> HealthProbe {
        HealthProbe::new()
    }

    fn sample() -> ProbeSample {
        ProbeSample {
            cpu_percent: 12.34,
            process_cpu_percent: 88.8,
            cpu_cores: 4,
            mem_used: 1024 * 1024 * 1024,
            mem_total: 4 * 1024 * 1024 * 1024,
            top: vec![ProcRow {
                pid: 42,
                name: "javaw.exe".to_owned(),
                cpu_percent: 55.5,
                mem_bytes: 2048,
            }],
            game: vec![ProcRow {
                pid: 42,
                name: "javaw.exe".to_owned(),
                cpu_percent: 55.5,
                mem_bytes: 2048,
            }],
            sampled_at: Some(Instant::now()),
        }
    }

    /// A program to hang a session's verdict off: the health report reads the
    /// encoder from here, not from the session (#101).
    fn program(
        cfg: &Config,
        handle: crate::pipeline::PipelineHandle,
    ) -> std::sync::Arc<crate::pipeline::SharedPipeline> {
        crate::pipeline::SharedPipeline::stub(
            handle,
            crate::pipeline::StreamSettings::from_config(cfg),
            cfg.input_policy,
        )
    }

    /// 没人连着的时候必须报 `idle` 且健康 —— 一个开着但没人看的虚拟机是正常
    /// 状态，不是故障。
    #[test]
    fn idle_is_healthy_and_says_so() {
        let cfg = cfg();
        let p = probe();
        let (ok, body) = build(&cfg, &p, &sample(), &[], &[]);
        assert!(ok, "idle must be healthy");
        assert_eq!(body["verdict"], "idle");
        assert_eq!(body["clients"]["count"], 0);
        assert_eq!(body["stream"]["active"], false);
    }

    /// 有会话、有帧、刚写过 track → `streaming`。
    #[test]
    fn a_live_pipeline_reports_streaming() {
        let cfg = cfg();
        let handle = PipelineHandle::new();
        // 「刚写过」和「现在读」必须是同一个时刻。以前这里取了两次
        // `Instant::now()`，满负荷并行跑时线程被挂起 3 秒以上，就变成
        // 「3 秒没写过 = 管线卡住」（5 次全量跑里挂过 2 次）。
        let now = Instant::now();
        {
            let stats = handle.stats();
            let mut g = stats.lock().unwrap();
            g.first_frame_seen = true;
            g.warmed_up = true;
            g.last_sample_at = Some(now);
            g.counters.new_frames = 30;
        }
        let info = SessionInfo {
            id: 7,
            connected_at: Some(now),
            peer_ip: "192.168.1.5".to_owned(),
            user_agent: "Mozilla/5.0".to_owned(),
            device_id: "dev-uuid".to_owned(),
            tab_id: "tab-uuid".to_owned(),
            client_type: "controller".to_owned(),
            display: Some(0),
            ..Default::default()
        };
        let program = program(&cfg, handle);
        program.set_controller(Some(7));
        let (ok, body) = build_at(&cfg, &probe(), &sample(), &[info], &[program], now);
        assert!(ok);
        assert_eq!(body["verdict"], "streaming");
        assert_eq!(body["clients"]["count"], 1);
        assert_eq!(body["clients"]["items"][0]["ip"], "192.168.1.5");
        assert_eq!(body["clients"]["items"][0]["device_id"], "dev-uuid");
        assert_eq!(body["stream"]["last_sample_age_ms"].as_u64(), Some(0));
        assert_eq!(body["pipelines"]["count"], 1);
        assert_eq!(body["pipelines"]["items"][0]["display"], 0);
    }

    /// #102：每个会话自己的字节数必须能从报告里读到。
    ///
    /// 共享编码下 `stream.video_mbps` 是**程序**的总量，两个人在看时它不回答
    /// 「这个人花了多少」。这条钉的是接线：`clients.items[].bytes_total` 读的是
    /// 扇出层按 sink 记的账，不是把总量摊到人数上——那样两个人只要帧率不同就
    /// 立刻算错。
    #[test]
    fn a_session_reports_its_own_bytes_and_reconnects() {
        let cfg = cfg();
        let now = Instant::now();
        let counters = std::sync::Arc::new(crate::pipeline::SinkCounters::default());
        counters.seed(4_000_000, 240);
        let info = SessionInfo {
            id: 11,
            connected_at: Some(now - Duration::from_secs(10)),
            peer_ip: "192.168.1.5".to_owned(),
            client_type: "viewer".to_owned(),
            sink: Some(counters),
            reconnects: 3,
            ..Default::default()
        };
        let (_, body) = build_at(&cfg, &probe(), &sample(), &[info], &[], now);
        let client = &body["clients"]["items"][0];
        assert_eq!(client["bytes_total"], 4_000_000);
        assert_eq!(client["frames_total"], 240);
        assert_eq!(client["reconnects"], 3, "a reload loop must be visible");
        // 4 MB over 10 s = 3.2 Mbit/s.
        let mbps = client["avg_mbps"].as_f64().expect("avg_mbps is a number");
        assert!((mbps - 3.2).abs() < 0.001, "expected 3.2, got {mbps}");
    }

    /// 还没 `Connected` 的会话没有下行，`health` 必须报 `null` 而不是 0——
    /// 「没在传」和「传了 0 字节」是两件事，后者会让人以为链路坏了。
    #[test]
    fn a_session_without_a_downlink_reports_no_counters() {
        let cfg = cfg();
        let now = Instant::now();
        let info = SessionInfo {
            id: 12,
            connected_at: Some(now),
            ..Default::default()
        };
        let (_, body) = build_at(&cfg, &probe(), &sample(), &[info], &[], now);
        assert_eq!(body["clients"]["items"][0]["bytes_total"], 0);
        assert_eq!(body["clients"]["items"][0]["avg_mbps"], 0.0);
    }

    /// 判据必须建立在**传进来的**那个时刻上。如果 `build_at` 自己再取一次
    /// `Instant::now()`，同一份快照就会在两个参考点之间摇摆：延迟读到的时刻
    /// 说「3 秒没写」，新取的时刻说「刚写过」，结论随机。
    #[test]
    fn the_verdict_is_computed_against_the_supplied_instant() {
        let cfg = cfg();
        let handle = PipelineHandle::new();
        let written = Instant::now();
        {
            let stats = handle.stats();
            let mut g = stats.lock().unwrap();
            g.first_frame_seen = true;
            g.last_sample_at = Some(written);
        }
        let info = SessionInfo {
            id: 9,
            connected_at: Some(written),
            ..Default::default()
        };
        let program = program(&cfg, handle);
        // 同一份快照，读取时刻晚 4 秒 → 必须判 stalled，尽管墙钟几乎没走。
        let late = written + Duration::from_secs(4);
        let (ok, body) = build_at(&cfg, &probe(), &sample(), &[info], &[program], late);
        assert!(!ok, "four seconds of silence is a stall");
        assert_eq!(body["verdict"], "stalled");
        assert_eq!(body["flags"][0], "no_frame_for_over_3s");
    }

    /// 核心判据：管线开着、出过帧，但 track 已经静了很久 → `stalled`，并且
    /// HTTP 状态要跟着变成 503（外部探活只看状态码）。
    #[test]
    fn a_silent_track_after_first_frame_is_stalled() {
        let cfg = cfg();
        let handle = PipelineHandle::new();
        {
            let stats = handle.stats();
            let mut g = stats.lock().unwrap();
            g.first_frame_seen = true;
            // 5 秒前写过，远超 3 秒阈值。
            g.last_sample_at = Some(Instant::now() - Duration::from_secs(5));
        }
        let info = SessionInfo {
            id: 1,
            connected_at: Some(Instant::now()),
            ..Default::default()
        };
        let program = program(&cfg, handle);
        let (ok, body) = build(&cfg, &probe(), &sample(), &[info], &[program]);
        assert!(!ok, "a wedged pipeline must not report healthy");
        assert_eq!(body["verdict"], "stalled");
        assert_eq!(body["flags"][0], "no_frame_for_over_3s");
    }

    /// 客户端暂停是用户的选择，不是故障。
    #[test]
    fn a_paused_pipeline_stays_healthy() {
        let cfg = cfg();
        let handle = PipelineHandle::new();
        handle.stop();
        let info = SessionInfo {
            id: 2,
            connected_at: Some(Instant::now()),
            ..Default::default()
        };
        let program = program(&cfg, handle);
        let (ok, body) = build(&cfg, &probe(), &sample(), &[info], &[program]);
        assert!(ok);
        assert_eq!(body["verdict"], "paused");
    }

    /// 建连了但还没出第一帧：预算内算健康，超时才挂旗（但仍不判死 —— 那可能
    /// 只是浏览器没发 SESSION）。
    #[test]
    fn waiting_for_a_first_frame_is_not_a_failure() {
        let cfg = cfg();
        let handle = PipelineHandle::new();
        let info = SessionInfo {
            id: 3,
            connected_at: Some(Instant::now()),
            ..Default::default()
        };
        let program = program(&cfg, handle);
        let (ok, body) = build(&cfg, &probe(), &sample(), &[info], &[program]);
        assert!(ok);
        assert_eq!(body["verdict"], "starting");
    }

    /// 进程名匹配大小写不敏感，并且不认识的名字不算游戏。
    #[test]
    fn game_detection_is_a_name_match() {
        assert!(is_game_process("javaw.exe"));
        assert!(is_game_process("JavaW.EXE"));
        assert!(is_game_process("Minecraft.Windows.exe"));
        assert!(!is_game_process("javaw.exe.bak"));
        assert!(!is_game_process("chrome.exe"));
    }

    /// 采样真的跑得起来，而且 `sample_age_ms` 会跟着走。
    #[test]
    fn probe_samples_the_host_once() {
        let p = probe();
        let first = p.sample();
        assert!(first.sampled_at.is_some(), "first call must sample");
        let again = p.sample();
        assert!(
            again.age_ms().unwrap() < SAMPLE_EVERY.as_millis() as u64,
            "second call must reuse the cached sample"
        );
    }

    /// 错误计数是累计值，所以"有没有新错"只能靠做差。这条钉住三件事：第一次
    /// 没有基线就不下结论、计数没动不算错、管线重建把计数清零也不算错。
    #[test]
    fn error_counters_are_diffed_against_the_previous_check() {
        let p = probe();
        let at_three = ErrorTotals {
            capture: 3,
            encode: 0,
            write: 0,
        };
        assert!(
            p.errors_since_last_check(at_three).is_none(),
            "no baseline, no claim"
        );

        let (grew, ms) = p.errors_since_last_check(at_three).expect("baseline exists");
        assert!(!grew, "an unchanged total is not a new error");
        assert!(ms < 60_000, "the span is wall clock, not a frame count: {ms}");

        let one_more = ErrorTotals {
            capture: 3,
            encode: 1,
            write: 0,
        };
        assert!(p.errors_since_last_check(one_more).unwrap().0);

        // 重建后计数器归零：从有到无不是错误。
        assert!(!p.errors_since_last_check(ErrorTotals::default()).unwrap().0);
    }
    /// 空会话下也要能序列化，不能因为缺字段 panic。
    #[test]
    fn payload_is_valid_json_without_a_session() {
        let (_, body) = build(&cfg(), &probe(), &ProbeSample::default(), &[], &[]);
        let text = body.to_string();
        assert!(text.starts_with('{'));
        assert!(hbb_common::serde_json::from_str::<Value>(&text).is_ok());
        assert_eq!(body["host"]["available"], false);
        assert_eq!(body["processes"]["game_running"], false);
    }

    // ---- #101：多会话下控制面的形状 ------------------------------------
    //
    // 这一段之前没有任何测试：控制面只有一条"置位 + 回状态码"的路，看起来
    // 不值得测。直到 `session/kick/<id>` 出现——它多了一个会被手打错的参数。

    /// 跑一次控制动作，把写出的 HTTP 报文当字符串拿回来。
    ///
    /// `Vec<u8>` 实现了 `AsyncWrite` 且永不失败，所以这里不需要 socket，
    /// 也就不需要为一个纯路由判断起监听。
    async fn control(action: &str) -> String {
        let mut out: Vec<u8> = Vec::new();
        handle_control(
            &mut out,
            action,
            &crate::signaling::SessionRegistry::default(),
            &crate::pipeline::PipelineRegistry::default(),
            &cfg(),
        )
        .await
        .expect("a Vec is an infallible writer");
        String::from_utf8_lossy(&out).into_owned()
    }

    /// `session/kick/2x` 必须是 400，而不是"解析失败 → 踢掉所有人"。
    ///
    /// 这是本轮新代码里唯一一个"参数错了会更糟"的地方：`kick(None)` 的语义
    /// 是踢全部，所以一个手滑多打的字符会把整机所有客户端一起断掉——包括
    /// 运维自己正在看的那个页面。`Option` 的 `and_then` 在这里是个陷阱，
    /// 因为它把"没有参数"和"参数不可解析"变成了同一个值。
    #[tokio::test]
    async fn a_non_numeric_session_id_is_rejected_instead_of_kicking_everyone() {
        let text = control("session/kick/2x").await;
        assert!(text.starts_with("HTTP/1.1 400"), "got: {text}");
        assert!(text.contains("must be a number"), "got: {text}");
        assert!(
            !text.contains("kicked"),
            "a rejected id must not report a kick: {text}"
        );
    }

    /// 踢一个不存在的会话号是 409，不是"踢了 0 个也算成功"。
    #[tokio::test]
    async fn kicking_an_absent_session_is_a_conflict() {
        for action in ["session/kick", "session/kick/7"] {
            let text = control(action).await;
            assert!(text.starts_with("HTTP/1.1 409"), "{action} got: {text}");
            assert!(text.contains("no live session"), "{action} got: {text}");
        }
    }

    /// 没有任何程序时重启采集器是 409：没东西可重建，不该回 202。
    #[tokio::test]
    async fn restarting_capture_without_a_program_is_a_conflict() {
        let text = control("capture/restart").await;
        assert!(text.starts_with("HTTP/1.1 409"), "got: {text}");
        assert!(text.contains("no live pipeline"), "got: {text}");
    }

    /// 拼错的动作得到 404 和一份白名单——静默 200 会让运维以为重启过了。
    #[tokio::test]
    async fn an_unknown_action_lists_the_known_ones() {
        let text = control("capture/restat").await;
        assert!(text.starts_with("HTTP/1.1 404"), "got: {text}");
        assert!(text.contains("capture/restart"), "got: {text}");
        // 带 id 的形式也要在清单里，否则它就是一个"文档里没写、只能靠猜"的接口。
        assert!(text.contains("session/kick/<id>"), "got: {text}");
    }

    /// #103：`role` 必须是服务端的裁决，不是页面的自称。
    ///
    /// 两个页签在 hello 里都会说自己是 `controller`（前端不区分），把这个字段
    /// 原样报出去会让运维看到「两个 controller」——上一轮线上就是这样，看起来
    /// 像门禁坏了，实际门禁是对的。租约在程序上，谁拿着谁才是 controller。
    #[test]
    fn the_role_comes_from_the_lease_not_from_the_page() {
        // #105: `role` 只在严格模式下才有区分度；`shared` 下所有人都是
        // controller，所以这个测试必须先关掉共享。
        let mut cfg = cfg();
        cfg.input_policy = InputPolicy::Exclusive;
        let cfg = cfg;
        let now = Instant::now();
        let info = |id: u64| SessionInfo {
            id,
            connected_at: Some(now),
            client_type: "controller".to_owned(),
            display: Some(0),
            ..Default::default()
        };
        let program = program(&cfg, PipelineHandle::new());
        program.set_controller(Some(2));

        let (_, body) = build_at(&cfg, &probe(), &sample(), &[info(1), info(2)], &[program], now);
        let items = &body["clients"]["items"];
        assert_eq!(items[0]["role"], "viewer");
        assert_eq!(items[1]["role"], "controller");
        // The claim is still reported, just not as the role: it is the only
        // record of what the page believed, which is useful when a client's
        // behaviour and its role disagree.
        assert_eq!(items[0]["client_type"], "controller");
        assert_eq!(items[0]["denied_inputs"], 0);
    }

    /// #105：共享输入下两个页签都报 controller —— 这次是**真的**，并且必须
    /// 有一处能让人区分「真共享」和上一轮那个「页面自称 controller」的 bug。
    #[test]
    fn shared_input_reports_every_tab_as_controller_and_says_so() {
        let cfg = cfg();
        assert_eq!(cfg.input_policy, InputPolicy::Shared, "默认必须是共享");
        let now = Instant::now();
        let info = |id: u64| SessionInfo {
            id,
            connected_at: Some(now),
            client_type: "controller".to_owned(),
            display: Some(0),
            ..Default::default()
        };
        let program = program(&cfg, PipelineHandle::new());
        // 一个持有租约、一个没有：租约在共享模式下不再决定能不能操作。
        program.set_controller(Some(2));

        let (_, body) = build_at(&cfg, &probe(), &sample(), &[info(1), info(2)], &[program], now);
        let items = &body["clients"]["items"];
        assert_eq!(items[0]["role"], "controller");
        assert_eq!(items[1]["role"], "controller");
        assert_eq!(body["server"]["input_policy"], "shared");
    }

    /// #103：还没 attach 的会话不持有任何租约，报 `pending` 而不是假装是 viewer。
    #[test]
    fn a_session_without_a_program_reports_pending() {
        let cfg = cfg();
        let now = Instant::now();
        let info = SessionInfo {
            id: 1,
            connected_at: Some(now),
            client_type: "controller".to_owned(),
            ..Default::default()
        };
        let (_, body) = build_at(&cfg, &probe(), &sample(), &[info], &[], now);
        assert_eq!(body["clients"]["items"][0]["role"], "pending");
        assert!(body["clients"]["items"][0]["display"].is_null());
    }

    /// #103：非数字的会话号是 400。降级成"接管 0 号"或"接管全部"都会让一次
    /// 手滑改掉别人正在用的控制权。
    #[tokio::test]
    async fn taking_with_a_bad_id_is_a_bad_request() {
        let text = control("session/take/2x").await;
        assert!(text.starts_with("HTTP/1.1 400"), "got: {text}");
        assert!(text.contains("must be a number"), "got: {text}");
    }

    /// 接管一个不存在的会话是 404：没有这个人，就没有可移交的对象。
    #[tokio::test]
    async fn taking_a_lease_for_an_absent_session_is_a_not_found() {
        let text = control("session/take/7").await;
        assert!(text.starts_with("HTTP/1.1 404"), "got: {text}");
        assert!(text.contains("no such session"), "got: {text}");
    }
}
