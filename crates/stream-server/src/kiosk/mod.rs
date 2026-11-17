//! 输入锁定（kiosk）：让"从网页进来的输入"只落在游戏里，出不去。
//!
//! ## 为什么需要它，而不是"游戏全屏就够了"
//!
//! 玩家按一下 `Alt+Tab`，游戏窗口就退到后面；`Win` 键弹开始菜单；`Alt+F4` 直接
//! 关掉游戏。这三件事在串流里都很糟，但只有第一件是**致命的**：串流采的是整块
//! 桌面（DXGI 桌面复制），游戏被切走之后画面就变成桌面 —— 而全屏的 MC 在失去
//! 焦点时会自己最小化到 `-32000,-32000`，玩家看到的就是一片空白，且从网页上
//! 没有任何办法把它叫回来。
//!
//! ## 三层，各管一段
//!
//! | 层 | 位置 | 拦什么 | 为什么只有它能拦 |
//! |---|---|---|---|
//! | 注入端 | [`guard`] + `dispatch::win_input` | 浏览器上报的危险组合 | 传输之前就没了，最干净 |
//! | 钩子 | [`hook`] | 物理键盘、别的进程的注入 | 系统级，只有它看得见 |
//! | 策略 | [`policy`] | `Win` 组合、任务管理器 | 注册表层，钩子挡不住的地方 |
//!
//! 三层都**不碰**同一次按键：注入的键由注入端判（钩子放行 `LLKHF_INJECTED`），
//! 物理的键由钩子判。判据本身只有一份，在 [`guard::judge`]。
//!
//! ## 一定能被解开的理由
//!
//! 锁住的机器最怕的是"锁死了"。这里的出口有四个，且互相独立：
//!
//! 1. `Ctrl+Alt+Del` —— SAS，任何用户态手段都拦不住，操作系统自己处理；
//! 2. `--unlock` —— 命令行，SSH 进去就能跑（SSH 的输入走网络，不经过键盘钩子）；
//! 3. 网页上的管理员口令 —— `POST /api/v1/control/kiosk/unlock`；
//! 4. 进程重启 —— 启动时 [`recover`] 会把上一轮残留的策略还回去。
//!
//! 第 4 条是兜底里的兜底：`taskkill /F` 之后什么都没跑，但下一次服务起来时策略
//! 会被还原。这也是把还原记录落盘（而不是只放内存）的唯一理由。

pub mod guard;
pub mod hook;
pub mod policy;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// 解锁请求文件：`--unlock` 落一个文件，运行中的服务看见就 disarm。
///
/// **为什么不是命名事件。** 第一版用的是 `Local\` 命名事件，在 `.128` 上实测
/// 直接失败：SSH 进来的进程在 **session 0**，而计划任务拉起的服务在 **session 1**
/// （console）。`Local\` 命名空间按会话隔离，两边看不见彼此 —— 也就是说，恰好
/// 在"最需要它"的那条通道（ssh 进来救场）上它是坏的。
///
/// 文件没有这个问题：同一个用户、同一个安装目录，跨会话跨桌面都成立。代价是
/// 轮询（400 ms 一次 `is_file`），换来的是"它一定看得见"。
const UNLOCK_REQUEST: &str = "kiosk-unlock.req";

/// 口令哈希的迭代次数。10 000 轮在现代 CPU 上是十几毫秒 —— 人点一次"解锁"
/// 感觉不到，而它把"拿到配置文件的明文就知道口令"变成"还得算一遍"。
///
/// 这不是在防离线爆破（能读到 `config.toml` 的人本来就有更高权限），而是防
/// "口令被看见"：运维拷一份配置去排障、贴进聊天窗口，里面的字串不该等于口令。
pub const HASH_ROUNDS: u32 = 10_000;

/// 正在锁定的状态位。注入端的热路径上只读这一个原子量。
static ARMED: AtomicBool = AtomicBool::new(false);
/// 锁定开始时刻（Unix 秒）。
static SINCE: AtomicU64 = AtomicU64::new(0);

/// 一次性初始化的东西：还原记录路径、配置开关。
struct Boot {
    state_file: PathBuf,
    policy: bool,
    hook: bool,
    password: Option<String>,
}

static BOOT: OnceLock<Boot> = OnceLock::new();
static REASON: Mutex<Option<String>> = Mutex::new(None);

/// 对外报告的状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    /// 是否锁定中。
    pub armed: bool,
    /// 锁定开始时刻（Unix 秒），未锁定时为 0。
    pub since_unix: u64,
    /// 谁触发的（`web` / `autolock` / `--unlock`），只用于日志与面板。
    pub reason: String,
    /// 能力是否启用（`kiosk = true`）。
    pub enabled: bool,
    /// 注册表策略这一层是否启用。
    pub policy: bool,
    /// 键盘钩子这一层是否启用，以及**是否真的装上了**。
    pub hook: bool,
    pub hook_installed: bool,
    /// 是否配了解锁口令。没配时网页上的解锁按钮必然失败，面板要能提前说明。
    pub password_set: bool,
    /// 有没有一份"欠着的"注册表还原记录（上次被强杀留下的）。
    pub policy_pending: bool,
}

impl Status {
    fn off() -> Self {
        Self {
            armed: false,
            since_unix: 0,
            reason: String::new(),
            enabled: false,
            policy: false,
            hook: false,
            hook_installed: false,
            password_set: false,
            policy_pending: false,
        }
    }
}

/// 注入端读这个。`Relaxed` 足够：它只与"这一次按键要不要注入"有关，不需要和
/// 别处的写建立顺序。
pub fn armed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

pub fn state_path(cfg: &Config) -> PathBuf {
    // 与游戏共用 `games\runtime`：一次锁定的还原记录和一次启动的 cmdline 是同一
    // 类东西 —— 都是"这次运行的痕迹"，都不该混进安装目录。
    match crate::game::Paths::resolve(cfg) {
        Ok(paths) => paths.runtime().join(policy::STATE_FILE),
        // 连 exe 目录都拿不到时只能放临时目录：宁可还原记录跟着重启丢掉，
        // 也不要因为路径算不出来就把策略留在机器上。
        Err(_) => std::env::temp_dir().join(policy::STATE_FILE),
    }
}

/// 进程启动时调用一次。
///
/// 无论 `kiosk` 是否启用都会跑，理由只有一个：**上一轮可能留下了改过的注册表**。
/// 那台机器可能是被 `taskkill /F` 结束的，也可能是崩溃的 —— 策略还在，而改它的
/// 那个进程已经不在了。如果这里因为"本次配置没开 kiosk"就跳过，那台机器会一直
/// 带着"任务管理器被禁用"运行下去。
pub fn recover(cfg: &Config) {
    let file = state_path(cfg);
    if policy::pending(&file) {
        match policy::restore(&file) {
            Ok(n) => log::info!("kiosk: 启动时还原了上一轮残留的 {n} 条策略（{}）", file.display()),
            Err(err) => log::warn!("kiosk: 启动时还原策略失败：{err}"),
        }
    }

    let boot = Boot {
        state_file: file,
        policy: cfg.kiosk && cfg.kiosk_policy,
        hook: cfg.kiosk && cfg.kiosk_hook,
        password: cfg.kiosk_password.clone().filter(|s| !s.trim().is_empty()),
    };
    let (policy_on, password_set) = (boot.policy, boot.password.is_some());
    if BOOT.set(boot).is_err() {
        return;
    }

    if !cfg.kiosk {
        return;
    }

    // 钩子装上之后不立刻生效（`set_armed(false)`），等真正锁定才拦。
    // 这样"启用了 kiosk 但暂时没锁"的部署里，物理键盘一切照旧。
    if cfg.kiosk_hook {
        match hook::start() {
            Ok(()) => hook::set_armed(false),
            // 装不上不是致命错误：还有注入端与策略两层，而且钩子失败最常见的
            // 原因是安全软件，运维需要知道结果但不需要服务起不来。
            Err(err) => log::warn!("kiosk: 键盘钩子没装上：{err}"),
        }
    }
    spawn_unlock_watcher();
    log::info!(
        "kiosk: 已启用（策略={} 钩子={} 口令={}）",
        policy_on,
        hook::installed(),
        if password_set { "有" } else { "未设置" }
    );
}

/// 是否要在游戏启动后自动上锁。
pub fn autolock(cfg: &Config) -> bool {
    cfg.kiosk && cfg.kiosk_autolock
}

/// 上锁。
pub fn engage(reason: &str) -> Result<Status, String> {
    let boot = BOOT.get().ok_or_else(|| "kiosk 尚未初始化".to_owned())?;
    if ARMED.load(Ordering::Relaxed) {
        return Ok(status());
    }

    // 顺序：策略 → 钩子 → 注入端过滤。
    //
    // 策略放最前面，因为它最可能失败（注册表被策略锁、被杀软保护）。它失败时
    // 我们还没动任何别的开关，可以直接返回 —— 反过来（先开过滤再失败）会留下
    // 一个"输入被吞但按键还能切窗口"的半锁状态。
    if boot.policy {
        if let Err(err) = policy::engage(&boot.state_file) {
            // 策略失败不阻止锁定：注入端与钩子已经能挡住绝大部分路径，而
            // "拒绝上锁"会让全屏游戏失去它唯一的保护。这是有意的不对称。
            log::warn!("kiosk: 策略层失败，继续用钩子与注入端锁定：{err}");
        }
    }
    if boot.hook {
        hook::set_armed(true);
    }
    ARMED.store(true, Ordering::Relaxed);
    let now = crate::game::now_unix();
    SINCE.store(now, Ordering::Relaxed);
    *REASON.lock().unwrap_or_else(|p| p.into_inner()) = Some(reason.to_owned());
    log::info!("kiosk: 已锁定（{reason}）");
    Ok(status())
}

/// 解锁。幂等。
pub fn disarm(reason: &str) -> Result<Status, String> {
    let was = ARMED.swap(false, Ordering::Relaxed);
    SINCE.store(0, Ordering::Relaxed);

    // 顺序与上锁相反：先还原注册表（它对桌面的影响最大），再撤钩子与过滤。
    // 每一层独立成败 —— 一层失败不能让后面的层留在锁着的状态。
    let mut problems = Vec::new();
    if let Some(boot) = BOOT.get() {
        match policy::restore(&boot.state_file) {
            Ok(n) => {
                if n > 0 {
                    log::info!("kiosk: 已还原 {n} 条策略");
                }
            }
            Err(err) => problems.push(err),
        }
        if boot.hook {
            hook::set_armed(false);
        }
    } else {
        // 没初始化过（比如 `--unlock` 在游戏托管关闭的部署里跑）：只清策略。
        let file = std::env::temp_dir().join(policy::STATE_FILE);
        let _ = policy::restore(&file);
    }

    // 解锁后不再保留"上一次为什么锁" —— 一个 `armed: false` 却带着
    // `reason: "web"` 的报告，面板会照着画一行"锁定中（web）"。
    *REASON.lock().unwrap_or_else(|p| p.into_inner()) = None;
    if was {
        log::info!("kiosk: 已解锁（{reason}）");
    }
    if problems.is_empty() {
        Ok(status())
    } else {
        Err(problems.join("；"))
    }
}

/// 只在"这次是自动锁的"时候解锁。返回是否真的解了。
///
/// 给"游戏停了"用：`kiosk_autolock` 锁上的桌面，游戏一停就该还回去，否则运维每次
/// 都要额外记一个动作。但**手工锁（面板上点的）不解** —— 那条路上的锁是有人明确
/// 要求的，不该因为一次停游戏就失效，否则"点停止"就成了绕过锁定的后门。
pub fn disarm_if_autolock(reason: &str) -> bool {
    if !armed() {
        return false;
    }
    let by_autolock = REASON
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_deref()
        == Some("autolock");
    if !by_autolock {
        return false;
    }
    match disarm(reason) {
        Ok(_) => true,
        Err(err) => {
            log::warn!("kiosk: 游戏结束后自动解锁失败：{err}");
            false
        }
    }
}

/// 当前状态。
pub fn status() -> Status {
    // 先填"与初始化无关"的那部分：即使 `recover` 还没跑（或者本次部署根本
    // 没启用 kiosk），`armed` 读到的也必须是真的当前值，而不是一个常量。
    let mut status = Status::off();
    status.armed = ARMED.load(Ordering::Relaxed);
    status.since_unix = SINCE.load(Ordering::Relaxed);
    status.reason = REASON
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .unwrap_or_default();

    let Some(boot) = BOOT.get() else {
        return status;
    };
    status.enabled = true;
    status.policy = boot.policy;
    status.hook = boot.hook;
    status.hook_installed = hook::installed();
    status.password_set = boot.password.is_some();
    status.policy_pending = policy::pending(&boot.state_file);
    status
}

// ------------------------------------------------------------------ 口令

/// 生成口令哈希，格式 `sha256i:<轮数>:<盐 hex>:<哈希 hex>`。
///
/// 盐取本机熵（纳秒时钟 + pid + 栈地址）再 SHA-256 —— 不需要密码学级随机源，
/// 它唯一的任务是"同一口令在两台机器上不能得出同一个哈希"。
pub fn hash_password(plain: &str) -> String {
    let mut seed = Vec::with_capacity(32);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    seed.extend_from_slice(&nanos.to_le_bytes());
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    let local = 0u8;
    seed.extend_from_slice(&(&local as *const u8 as usize).to_le_bytes());
    let salt = sha256(&seed);
    let salt = &salt[..16];

    let mut acc = sha256_parts(&[salt, plain.as_bytes()]);
    for _ in 1..HASH_ROUNDS {
        acc = sha256_parts(&[&acc, salt]);
    }
    format!(
        "sha256i:{HASH_ROUNDS}:{}:{}",
        hex(salt),
        hex(&acc)
    )
}

/// 校验。格式不对、轮数为 0、盐或哈希长度不对，一律 `false` —— 没有口令就
/// 不该有"猜对"的可能。
pub fn verify_password(stored: &str, plain: &str) -> bool {
    let mut parts = stored.trim().split(':');
    if parts.next() != Some("sha256i") {
        return false;
    }
    let rounds: u32 = match parts.next().and_then(|s| s.parse().ok()) {
        Some(n) if n > 0 => n,
        _ => return false,
    };
    let salt = match parts.next().and_then(unhex) {
        Some(v) => v,
        None => return false,
    };
    let want = match parts.next().and_then(unhex) {
        Some(v) => v,
        None => return false,
    };
    if salt.is_empty() || want.is_empty() || parts.next().is_some() {
        return false;
    }

    let mut acc = sha256_parts(&[&salt, plain.as_bytes()]);
    for _ in 1..rounds {
        acc = sha256_parts(&[&acc, &salt]);
    }
    // 常数时间比较：长度不同也走完整轮，避免"前几位对不对"从耗时里漏出去。
    if acc.len() != want.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in acc.iter().zip(want.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn sha256(data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().to_vec()
}

fn sha256_parts(parts: &[&[u8]]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for part in parts {
        h.update(part);
    }
    h.finalize().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

// ------------------------------------------------------- 跨进程解锁信号

/// 请求正在运行的服务解锁。返回是否成功落下请求文件。
///
/// `cfg` 而不是从 [`BOOT`] 读：调用它的是 `--unlock` 那个**一次性进程**，它的
/// `recover` 根本没跑过，BOOT 是空的。
pub fn signal_disarm(cfg: &Config) -> bool {
    let request = state_path(cfg).with_file_name(UNLOCK_REQUEST);
    if let Some(dir) = request.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 内容只是日志线索（谁、什么时候），存在本身就是信号。
    match std::fs::write(&request, format!("requested_at={}\n", crate::game::now_unix())) {
        Ok(()) => true,
        Err(err) => {
            log::warn!("kiosk: 写解锁请求 {} 失败：{err}", request.display());
            false
        }
    }
}

fn spawn_unlock_watcher() {
    let Some(boot) = BOOT.get() else {
        return;
    };
    let request = boot.state_file.with_file_name(UNLOCK_REQUEST);
    std::thread::Builder::new()
        .name("kiosk-unlock".to_owned())
        .spawn(move || loop {
            if request.is_file() {
                // 先删再 disarm：disarm 里的策略还原可能要几毫秒，而删文件在前
                // 可以保证"请求只被处理一次"，不会因为下一轮又看见它而重复解锁。
                let _ = std::fs::remove_file(&request);
                if armed() {
                    match disarm("本机 --unlock") {
                        Ok(_) => {}
                        Err(err) => log::warn!("kiosk: --unlock 解锁时出错：{err}"),
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(400));
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trip() {
        let hashed = hash_password("cloud-2026");
        assert!(hashed.starts_with("sha256i:10000:"));
        assert!(verify_password(&hashed, "cloud-2026"));
        assert!(!verify_password(&hashed, "cloud-2025"));
        assert!(!verify_password(&hashed, ""));
    }

    #[test]
    fn same_password_hashes_differently_each_time() {
        // 盐是随机的，所以两次哈希不该相等 —— 否则"这个口令被人用过"能一眼看出。
        let a = hash_password("same");
        let b = hash_password("same");
        assert_ne!(a, b);
        assert!(verify_password(&a, "same"));
        assert!(verify_password(&b, "same"));
    }

    #[test]
    fn malformed_hashes_never_verify() {
        for bad in [
            "",
            "plain-text",
            "sha256i:0:00:00",
            "sha256i:10000:zz:00",
            "sha256i:10000:0011",
            "sha256i:abc:00:00",
            "sha256i:10000:0011:2233:extra",
        ] {
            assert!(!verify_password(bad, "anything"), "{bad:?} 不该被接受");
        }
    }

    #[test]
    fn hex_helpers_round_trip() {
        let data = [0u8, 1, 15, 16, 255];
        assert_eq!(unhex(&hex(&data)).unwrap(), data.to_vec());
        assert!(unhex("abc").is_none(), "奇数长度不是合法 hex");
        assert!(unhex("xy").is_none());
    }

    #[test]
    fn status_reports_off_before_boot() {
        // 这个测试不碰全局初始化，只看默认形态：未初始化时必须诚实地说"没开"，
        // 而不是报一个"已启用但没锁"的半真状态。
        let s = status();
        if BOOT.get().is_none() {
            assert!(!s.enabled);
            assert!(!s.armed);
        }
    }

    #[test]
    fn disarmed_by_default() {
        assert!(!armed());
    }
}
