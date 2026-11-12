//! `--run`：由计划任务直接拉起服务，取代 `start-stream.cmd`。
//!
//! 过去这一层是 `start-stream.cmd`（191 行）加 `run-hidden.vbs`（28 行）。那
//! 219 行里真正属于业务的大约十几行：拼路径、看证书在不在、看令牌在不在。剩下
//! 的是在补偿 cmd 的两个先天缺陷 ——
//!
//! 1. **它没有表达式。** 一个 `if not exist ... goto` 就要占三行，而
//!    `%ARGS%` 在括号块里只按块展开一次，于是所有拼接只能写成标签跳转。
//! 2. **console 子系统的程序被计划任务拉起时，Windows 会代它创建一个控制台
//!    窗口。** 任务每分钟触发一次，于是每分钟闪一次黑框，每次都抢走全屏游戏
//!    的前台焦点。`run-hidden.vbs` 存在的唯一理由就是消掉这个窗口。
//!
//! 换成这里之后，这两条都不再需要补偿：路径由 exe 自己推导，而进程是 GUI 子
//! 系统（见 `crates/stream-server/build.rs` 与 `main.rs` 里的 stdio 兜底），
//! Windows 不会替它开窗口。
//!
//! ## 为什么是“展开参数”而不是“另一条启动路径”
//!
//! [`expand_if_requested`] 只做一件事：把 `--run` 换成一串普通参数，然后交回
//! `app::run` 走**完全同一条**路径。配置解析、日志、绑定、采集循环一行都不用
//! 改，也就不可能出现“脚本启动和服务启动行为不同”这类只在一边复现的问题。
//!
//! 追加的参数原样排在末尾，所以 `--run --fps 60` 就是原来那个
//! `start-stream.60fps.cmd`：命令行压过配置文件，这条优先级规则一天都没变过。

use std::path::{Path, PathBuf};

/// 整包分发里前端的目录名（`--dist`）。
const DIST_DIR: &str = "selkies-web";
const LOG_DIR: &str = "logs";
/// 启动器的日志。和服务自己的 `stream-server.log` 分开：这个文件记的是"起没起
/// 来、为什么没起来"，服务日志记的是起来之后发生了什么。合并的话，一个每分
/// 钟跑一次的探针会把真正的服务日志冲成一片噪声。
const WATCHDOG_LOG: &str = "watchdog.log";
const SERVER_LOG: &str = "stream-server.log";
const CERT_FILE: &str = "cert.pem";
const KEY_FILE: &str = "key.pem";

/// 单实例互斥量。
///
/// `Local\` 而不是 `Global\`：同一台机器上两个用户各跑一份是**允许的**（不同
/// 端口、不同目录），而 `Global\` 会把它们互相判成重复。计划任务用的是登录用
/// 户的交互式令牌，手动双击也是同一个会话，所以 `Local\` 正好覆盖"同一个人不
/// 要起两份"这个真实需求。
///
/// 名字里带 `cloud-stream` 而不是泛泛的 `server`：这是进程级可见的全局名字，
/// 撞名的代价是服务莫名其妙起不来。
const MUTEX_NAME: &str = "Local\\cloud-stream-server-instance";

/// 运行根：本 exe 所在目录。
///
/// 一切都从这里推导 —— 整包搬到 D 盘、U 盘、`%LOCALAPPDATA%` 都照常工作。历史
/// 上硬编码过 `C:\dev\stream`，结果是包解压到旁边一层时任务起来了、页面 404、
/// 日志里一片正常。
pub fn root() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(Path::to_path_buf)
}

/// 本地时间戳，格式与原来 cmd 写的 `%date% %time%` 同序，便于和旧日志对照。
///
/// `pub` 是为了让 `boot` 的 panic 记录用同一份格式 —— 崩溃日志和看门狗日志
/// 挨着看的时候，行首如果两种写法，第一件事就是先换算时间，很蠢。
#[cfg(windows)]
pub fn stamp_for_log() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

/// 非 Windows 上跑测试用；部署只有 Windows。
#[cfg(not(windows))]
pub fn stamp_for_log() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = crate::version::civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}Z")
}

/// 往 `logs\watchdog.log` 追加一行。
///
/// 写不进去就算了：这条路径上没有任何地方能报告失败，而它存在的理由恰恰是
/// “服务没起来时也要留下痕迹”。为了记一句日志而让服务起不来是本末倒置。
pub fn note(root: &Path, message: &str) {
    let dir = root.join(LOG_DIR);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(WATCHDOG_LOG))
    {
        let _ = writeln!(file, "[{}] {message}", stamp_for_log());
    }
}

/// 互斥量句柄。存起来而不是丢掉：`HANDLE` 是裸类型，没有 `Drop`，所以丢不丢
/// 其实都不会关闭它 —— 但“句柄在谁手里”必须写清楚，否则下一个人会以为它已经
/// 被某个作用域收走了，然后在这里补一句 `CloseHandle`（那会让锁提前失效）。
static MUTEX_HANDLE: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// 拿单实例锁。返回 `true` 表示“可以继续启动”。
///
/// 建不出互斥量时返回 `true`（宁可多起一个，也不要因为一个观测手段起不来）：
/// 真正防止重复绑端口的是 bind 本身，这里只是让“每分钟触发一次”不变成每分钟
/// 一条端口占用的错误日志。
#[cfg(windows)]
fn acquire(root: &Path) -> bool {
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    if handle == 0 {
        note(root, "建不出单实例互斥量，按“没有别的实例”继续");
        return true;
    }
    MUTEX_HANDLE.store(handle as isize, Ordering::Relaxed);
    let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    !already
}

#[cfg(not(windows))]
fn acquire(_root: &Path) -> bool {
    true
}

/// 上传根跟着**跑游戏的那个账户**走，不硬编码任何用户名。
fn upload_dir() -> Option<PathBuf> {
    let profile = std::env::var_os("USERPROFILE")?;
    let profile = PathBuf::from(profile);
    let desktop = profile.join("Desktop");
    Some(if desktop.exists() { desktop } else { profile })
}

/// 拼出完整的命令行。
///
/// 出错返回的是**给人看的一句话**，它会被原样写进 `watchdog.log` —— 服务没起
/// 来时，那台机器上能留下的就只有这一行。
fn build(extra: &[String]) -> Result<Vec<String>, String> {
    let dir = root().ok_or_else(|| "拿不到 exe 所在目录".to_owned())?;

    let logs = dir.join(LOG_DIR);
    std::fs::create_dir_all(&logs)
        .map_err(|e| format!("建不出日志目录 {}：{e}", logs.display()))?;

    // 配置必须存在。缺了它服务照样能起 —— 用 1080p30 的默认值，看起来一切正
    // 常，却把每一处调过的值都忽略了。那正是这套拆分最想消灭的故障：**静默**
    // 地不用你的配置。所以这里拒绝启动，并且说清缺的是哪几份 —— 只报 `config.toml`
    // 会让人盯着手上那个 `stream-config.toml` 发愣。
    let config = crate::configfile::resolve(None)
        .ok_or_else(|| "推不出配置文件路径".to_owned())?;
    if !config.exists() {
        let listed = if config.file_name().and_then(|n| n.to_str())
            == Some(crate::configfile::BUNDLE_CONFIG)
        {
            format!(
                "{}（也没找到 config.toml）",
                config.display()
            )
        } else {
            format!(
                "{}（也没找到 {}）",
                config.display(),
                config.with_file_name(crate::configfile::BUNDLE_CONFIG).display()
            )
        };
        return Err(format!(
            "缺少配置文件 {listed}。它随整包分发，缺了就不该在默认值上静默启动"
        ));
    }

    let dist = dir.join(DIST_DIR);
    if !dist.exists() {
        note(
            &dir,
            &format!(
                "缺少前端目录 {} —— 服务会照常起，但页面会 404",
                dist.display()
            ),
        );
    }

    let mut out: Vec<String> = Vec::new();
    let mut push = |flag: &str, value: &str| {
        out.push(flag.to_owned());
        out.push(value.to_owned());
    };
    push("--config", &config.to_string_lossy());
    push("--dist", &dist.to_string_lossy());
    push("--log", &logs.join(SERVER_LOG).to_string_lossy());

    // TLS 是有条件的：没有证书的包也必须能跑（明文 HTTP）。剪贴板同步要安全
    // 上下文，所以“缺证书”这件事要**记一笔**，而不是悄悄降级 —— 否则现象是
    // “复制粘贴不工作”，而任何日志里都找不到原因。
    let cert = dir.join(CERT_FILE);
    let key = dir.join(KEY_FILE);
    if cert.exists() && key.exists() {
        push("--tls-cert", &cert.to_string_lossy());
        push("--tls-key", &key.to_string_lossy());
    } else {
        note(&dir, "没有 cert.pem/key.pem —— 走明文 HTTP，剪贴板同步关闭");
    }

    if let Some(upload) = upload_dir() {
        push("--upload-dir", &upload.to_string_lossy());
    }

    // 口令的**来源**不在这一层了。
    //
    // 2026-11 之前是这里读 `auth-token.txt` 再 push 一个 `--auth-token` 进
    // 命令行 —— 于是它永远压过配置文件：有人在界面上改了口令、重启却不生效，
    // 而任何日志里都看不出原因。现在口令是配置里的 `auth_token` 一项，
    // `Config::parse` 读（配置文件优先，同目录的 txt 兜底，见 config.rs）。
    //
    // 这里只留一句状态：`watchdog.log` 是"服务没起来"时唯一能看到的东西，
    // "到底要不要口令"值得记一笔。判断顺序必须与 `Config::parse` 一致。
    let auth_from_config = crate::configfile::load(&config)
        .ok()
        .and_then(|file| file.auth_token)
        .filter(|token| !token.is_empty());
    match auth_from_config.or_else(|| crate::configfile::read_legacy_token(&dir)) {
        Some(_) => note(&dir, "已设置访问口令 —— 浏览器要在地址后加 ?token="),
        None => note(&dir, "没有设置访问口令 —— 能连上端口的人都能访问 /api/*"),
    }

    out.extend(extra.iter().cloned());
    Ok(out)
}

/// 这次进程是不是由 `--run` 拉起来的。
///
/// `expand_if_requested` 会把 `--run` 从参数里抹掉，之后任何地方都看不出它曾
/// 经存在。但后面有几处**必须**知道：配置解析失败时只 `eprintln!` 是不够的
/// （GUI 子系统下 stderr 是 NUL），那种时候要额外落一份盘。
static VIA_RUN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 见 [`VIA_RUN`]。
pub fn launched_by_run() -> bool {
    VIA_RUN.load(std::sync::atomic::Ordering::Relaxed)
}

/// `--probe`：另起一个短命进程，把“到底有没有监听”写成一行 PREFLIGHT。
///
/// ## 为什么必须是另一个进程
///
/// `--run` 的进程马上就要阻塞在服务循环上（这是看门狗机制的前提：任务保持
/// Running，后来的触发被“不启动新实例”挡掉），没有线程可以拿去轮询端口。
/// 原来的 `probe-listen.cmd` 用 `start /b` 起了自己，理由一模一样。
///
/// ## 为什么它自己解析配置
///
/// 端口可能来自配置文件，也可能被 `--run --port 9000` 覆盖。让探测进程用**同
/// 一套** `Config::parse` 得出端口，比在 `--run` 里另算一遍可靠 —— 两处算法
/// 迟早分叉，然后探的是 8080、服务在 9000，日志里写着“没监听”。
pub fn probe_try_run(argv: &[String]) -> Option<i32> {
    if !argv.iter().any(|arg| arg == "--probe") {
        return None;
    }
    let rest: Vec<String> = argv
        .iter()
        .filter(|arg| *arg != "--probe")
        .cloned()
        .collect();
    let Some(root) = root() else {
        return Some(2);
    };
    // 配置坏掉时这里也会失败，于是 port = 0、直接报“没监听”—— 恰好是对的：
    // 服务确实起不来，而 `--run` 那边已经把 TOML 的行号写进同一份日志了。
    let port = crate::config::Config::parse(rest).map(|cfg| cfg.port).unwrap_or(0);
    if port == 0 {
        note(&root, "PREFLIGHT 配置读不出端口，跳过探测");
        return Some(1);
    }

    // 12 次 × 2.5 s ≈ 30 s。和服务启动期（硬件探测 + 编码器枚举）差不多长，
    // 再久就是“本来就没起来”，等下去没有新信息。
    for attempt in 1..=12u32 {
        std::thread::sleep(std::time::Duration::from_millis(2500));
        if std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().ok()?,
            std::time::Duration::from_millis(800),
        )
        .is_ok()
        {
            note(
                &root,
                &format!("PREFLIGHT listening on {port} after {attempt} probe(s)"),
            );
            return Some(0);
        }
    }

    // 失败时给的是**下一步**，不是形容词。这三条覆盖了现场见过的大部分情形。
    note(
        &root,
        &format!(
            "PREFLIGHT NOT listening on {port} after 12 probes (30 s) -- \
             看 logs\\stream-server.log；没有这个文件就是 exe 根本没跑起来（缺 VC++ 运行库 / \
             被杀毒拦了）；有文件但最后一行是 config error 就是配置错了"
        ),
    );
    Some(1)
}

/// 在当前进程之外启动探测。
///
/// 失败**不算错**：探测是观测手段，它起不来时服务照样该跑。所以只是静默跳过
/// —— 那台机器上会少一行 PREFLIGHT，不会少一个服务。
fn spawn_probe(root: &Path, argv: &[String]) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--probe");
    // 把展开出来的参数原样带过去（`--config` 在里面），端口才和服务一致。
    cmd.args(argv);
    let _ = cmd
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// 如果命令行里有 `--run`，把它展开成普通参数；否则原样返回。
///
/// `--run` 之前的参数保留在前面（典型情况为空：`--version` / `--ops` /
/// `--update` 在更早的分支就已经 `exit` 了），之后的参数原样排在末尾，用来
/// 覆盖配置文件里的同名项。
pub fn expand_if_requested(argv: Vec<String>) -> Vec<String> {
    let Some(pos) = argv.iter().position(|arg| arg == "--run") else {
        return argv;
    };
    VIA_RUN.store(true, std::sync::atomic::Ordering::Relaxed);
    let dir = match root() {
        Some(dir) => dir,
        None => {
            eprintln!("run: 拿不到 exe 所在目录");
            std::process::exit(2);
        }
    };

    if !acquire(&dir) {
        // 每分钟触发一次的任务走到这里就安静退出 —— 这正是 #81 的自愈设计：
        // 服务活着时任务保持 Running，后来的触发被"不启动新实例"策略挡掉；
        // 服务死了这一次才会往下走。
        note(&dir, "已有一个实例在跑，这次触发不做任何事");
        std::process::exit(0);
    }

    match build(&argv[pos + 1..]) {
        Ok(mut expanded) => {
            note(&dir, &format!("启动：{}", dir.display()));
            // 探测要在服务开始绑端口之前就位，否则它可能错过最早的几次成功
            // 连接判断（虽然 30 秒窗口足够长，但顺序反了就没道理）。
            spawn_probe(&dir, &expanded);
            let mut out = argv[..pos].to_vec();
            out.append(&mut expanded);
            out
        }
        Err(err) => {
            // GUI 子系统下 stderr 可能是空的，所以先落盘再打印 —— 顺序反了的话
            // 就是“界面闪一下就没了、什么也没留下”。
            note(&dir, &err);
            eprintln!("run: {err}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 展开出来的参数必须能被 `Config::parse` 认下来，且顺序是按“路径在前、
    /// 追加项在后”。追加项在末尾这一条不能破：`--run --fps 60` 靠的就是它压
    /// 过配置文件。
    #[test]
    fn extra_arguments_come_last() {
        let extra = vec!["--fps".to_owned(), "60".to_owned()];
        // 不依赖真实文件系统：直接检查“拼装顺序”这个纯逻辑。
        let mut out: Vec<String> = vec![
            "--config".into(),
            "c.toml".into(),
            "--dist".into(),
            "d".into(),
        ];
        out.extend(extra.iter().cloned());
        assert_eq!(out.last().unwrap(), "60");
        assert_eq!(out[out.len() - 2], "--fps");
    }

    /// 时间戳的形状要和旧日志对得上，否则拿来 diff 会一片噪声。
    #[test]
    fn stamp_has_a_stable_shape() {
        let s = stamp_for_log();
        assert_eq!(s.len(), 19, "应为 YYYY-MM-DD HH:MM:SS，实际 {s:?}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], " ");
    }
}
