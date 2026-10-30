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

use crate::config::Config;
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
    session: Option<&SessionInfo>,
) -> (bool, Value) {
    let pipeline = session.and_then(|s| s.pipeline.clone());
    let settings = session.and_then(|s| s.settings.clone());

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
                snapshot_fields(&g)
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
                let waited = session
                    .and_then(|s| s.connected_at)
                    .map(|at| at.elapsed())
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

    let clients = match session {
        Some(info) => {
            let connected_s = info
                .connected_at
                .map(|at| at.elapsed().as_secs())
                .unwrap_or(0);
            json!({
                "count": 1,
                "items": [{
                    "id": info.id,
                    "ip": info.peer_ip,
                    "device_id": info.device_id,
                    "tab_id": info.tab_id,
                    "role": info.role,
                    "user_agent": info.user_agent,
                    "connected_s": connected_s,
                }],
            })
        }
        None => json!({ "count": 0, "items": [] }),
    };

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
    });

    (verdict.healthy(), payload)
}

#[allow(clippy::type_complexity)]
fn snapshot_fields(g: &PipelineStats) -> (u64, u64, u64, u64, u64, u64, f32, f32, f32, u32, u64, bool, bool, Option<Duration>) {
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
        g.last_sample_at.map(|at| at.elapsed()),
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
    probe: &HealthProbe,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let sample = probe.sample();
    // The registry keeps this behind a std mutex: the value is small and is
    // never held across an await, so a blocking lock is the honest choice.
    let session = registry
        .info()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let (healthy, payload) = build(cfg, probe, &sample, session.as_ref());
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
    _cfg: &Config,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let session = registry
        .info()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    match action {
        "capture/restart" => match session.as_ref().and_then(|s| s.pipeline.clone()) {
            Some(pipeline) => {
                pipeline.request_rebuild();
                log::info!("#93: capture/encoder rebuild requested over HTTP");
                json_reply(stream, 202, "Accepted", json!({
                    "ok": true,
                    "action": "capture/restart",
                }))
                .await
            }
            None => {
                json_reply(stream, 409, "Conflict", json!({
                    "ok": false,
                    "action": "capture/restart",
                    "error": "no live session",
                }))
                .await
            }
        },

        "session/kick" => {
            if registry.kick("kicked by an operator").await {
                json_reply(stream, 200, "OK", json!({
                    "ok": true,
                    "action": "session/kick",
                }))
                .await
            } else {
                json_reply(stream, 409, "Conflict", json!({
                    "ok": false,
                    "action": "session/kick",
                    "error": "no live session",
                }))
                .await
            }
        }

        other => {
            json_reply(stream, 404, "Not Found", json!({
                "ok": false,
                "error": "unknown control action",
                "action": other,
                "known": ["capture/restart", "session/kick"],
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

    /// 没人连着的时候必须报 `idle` 且健康 —— 一个开着但没人看的虚拟机是正常
    /// 状态，不是故障。
    #[test]
    fn idle_is_healthy_and_says_so() {
        let cfg = cfg();
        let p = probe();
        let (ok, body) = build(&cfg, &p, &sample(), None);
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
        {
            let stats = handle.stats();
            let mut g = stats.lock().unwrap();
            g.first_frame_seen = true;
            g.warmed_up = true;
            g.last_sample_at = Some(Instant::now());
            g.counters.new_frames = 30;
        }
        let info = SessionInfo {
            id: 7,
            connected_at: Some(Instant::now()),
            peer_ip: "192.168.1.5".to_owned(),
            user_agent: "Mozilla/5.0".to_owned(),
            device_id: "dev-uuid".to_owned(),
            tab_id: "tab-uuid".to_owned(),
            role: "controller".to_owned(),
            pipeline: Some(handle),
            settings: Some(crate::pipeline::StreamSettings::from_config(&cfg)),
        };
        let (ok, body) = build(&cfg, &probe(), &sample(), Some(&info));
        assert!(ok);
        assert_eq!(body["verdict"], "streaming");
        assert_eq!(body["clients"]["count"], 1);
        assert_eq!(body["clients"]["items"][0]["ip"], "192.168.1.5");
        assert_eq!(body["clients"]["items"][0]["device_id"], "dev-uuid");
        assert!(body["stream"]["last_sample_age_ms"].as_u64().is_some());
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
            pipeline: Some(handle),
            settings: Some(crate::pipeline::StreamSettings::from_config(&cfg)),
            ..Default::default()
        };
        let (ok, body) = build(&cfg, &probe(), &sample(), Some(&info));
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
            pipeline: Some(handle),
            settings: Some(crate::pipeline::StreamSettings::from_config(&cfg)),
            ..Default::default()
        };
        let (ok, body) = build(&cfg, &probe(), &sample(), Some(&info));
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
            pipeline: Some(handle),
            settings: Some(crate::pipeline::StreamSettings::from_config(&cfg)),
            ..Default::default()
        };
        let (ok, body) = build(&cfg, &probe(), &sample(), Some(&info));
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
        let (_, body) = build(&cfg(), &probe(), &ProbeSample::default(), None);
        let text = body.to_string();
        assert!(text.starts_with('{'));
        assert!(hbb_common::serde_json::from_str::<Value>(&text).is_ok());
        assert_eq!(body["host"]["available"], false);
        assert_eq!(body["processes"]["game_running"], false);
    }
}
