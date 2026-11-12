//! `--diagnose`：把"这东西为什么起不来"需要的事实一次写成一份文件。
//!
//! # 它替掉了什么
//!
//! 2026-11 之前这是 `toolchain/collect-launch-facts.cmd`（184 行 cmd）。它存在
//! 的理由只有一条：**exe 起不来的时候，谁能去收现场**。而 exe 起不来的头号
//! 原因是缺 VC++ 运行时（`stream-server.exe` 与 `stream-setting.exe` 都动态
//! 链接 `VCRUNTIME140.dll`），所以那个工具必须是"不依赖 VC++"的东西 —— 只能
//! 是脚本。
//!
//! 那个前提现在没了：`vcruntime140.dll` 跟着包走（app-local 部署，安装器把它
//! 放进 `{app}`，加载器优先找同目录，而它不是 KnownDLL）。于是"缺 VC++"不再
//! 是一个失败模式，诊断也就可以搬进 exe —— 搬进来之后它才有单元测试、才能被
//! 工具箱一键调用、才不会随包布局变化而腐掉（旧脚本里那份"必须存在的文件"
//! 清单把它自己都列进去了）。
//!
//! # 为什么九节、为什么这个顺序
//!
//! 顺序照抄旧脚本：文档里"看第 9 节"这类引用继续有效，两边的事实清单能逐节
//! 对照。九节按"从外到内"排 —— 先确认人在哪台机器的哪个会话上（1、2），再看
//! 包本身缺不缺东西（3、4），然后才是进程/任务/日志（5、6），最后是任务计划
//! 事件、杀软与入站可达性（7、8、9）。
//!
//! # 只读
//!
//! 不启动服务、不写注册表、不改任务、不删文件。唯一的写入是它自己的输出文件。
//! 刻意**不**去起一份 `stream-server.exe` 来看真实的报错：那会抢屏、抢 8080，
//! 并且和已经在正常工作的一份打架。

use std::fmt::Write as _;
use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::configfile;

/// 输出文件名。与旧脚本一致：回传流程、文档、口头说法都不用改。
const FACTS_FILE: &str = "cloud-stream-facts.txt";

/// 日志拖尾的行数。旧脚本是 40，够看出"启动到哪一步"而不用整份回传。
const LOG_TAIL: usize = 40;

/// 任务名。安装器建的那个。
const TASK_NAME: &str = "MCStreamSrv";

/// `--diagnose` 在命令行里吗。
///
/// 返回 `Some(退出码)` = 已经处理完；`None` = 不关我事，继续正常启动。
/// 与 `--probe` / `--ops` 同一形态：一次性动作，用完就退出。
pub fn try_run(argv: &[String]) -> Option<i32> {
    if !argv.iter().any(|a| a == "--diagnose") {
        return None;
    }
    Some(run())
}

/// 收一遍现场，写两个文件，再把正文打到 stdout。
///
/// 两个文件是刻意的：运行根那份给"坐在这台机器前的人"（他打开目录就能看见），
/// 桌面那份给"要把文件发回来的人"（不用教他路径）。
pub fn run() -> i32 {
    let Some(root) = crate::run::root() else {
        eprintln!("--diagnose：拿不到 exe 所在目录");
        return 1;
    };
    let text = collect(&root);

    // 落盘用 CRLF：`writeln!` 只写 `\n`，而这份文件的用途是"双击打开、看两行、
    // 然后发给别人"。老记事本遇到纯 LF 会把它显示成一整行 —— 一份读不了的事实
    // 清单等于没有。
    let for_file = crlf(&text);

    let mut wrote: Vec<PathBuf> = Vec::new();
    let primary = root.join(FACTS_FILE);
    match std::fs::write(&primary, for_file.as_bytes()) {
        Ok(()) => wrote.push(primary),
        // 只读目录、被杀软锁住都会走到这里。不 panic：正文还能从 stdout 出去。
        Err(err) => eprintln!("--diagnose：写不进 {}（{err}）", root.join(FACTS_FILE).display()),
    }
    if let Some(desktop) = desktop_dir() {
        let copy = desktop.join(FACTS_FILE);
        if std::fs::write(&copy, for_file.as_bytes()).is_ok() {
            wrote.push(copy);
        }
    }

    // 正文也打一遍：从 cmd 里敲这条命令的人应当马上看到结果，而不是"跑完了，
    // 自己去找文件"。GUI 子系统下没有控制台时这句落到 NUL，安全（见 boot）。
    print!("{text}");
    let _ = std::io::stdout().flush();
    for path in &wrote {
        eprintln!("已写入 {}", path.display());
    }
    if wrote.is_empty() {
        1
    } else {
        0
    }
}

/// 桌面目录。`OneDrive\桌面` 那种重定向不归这里管 —— `%USERPROFILE%\Desktop`
/// 是安装器判定的同一份，与 `run::upload_dir` 的口径一致。
fn desktop_dir() -> Option<PathBuf> {
    let profile = std::env::var_os("USERPROFILE")?;
    let desktop = PathBuf::from(profile).join("Desktop");
    desktop.is_dir().then_some(desktop)
}

/// 九节正文。
fn collect(root: &Path) -> String {
    let mut out = String::with_capacity(16 * 1024);
    let _ = writeln!(out, "cloud-stream launch facts");
    let _ = writeln!(out, "script dir   {}", root.display());
    let _ = writeln!(out, "collected    {}", crate::run::stamp_for_log());
    let _ = writeln!(out);

    section_windows(&mut out);
    section_account(&mut out);
    section_bundle(&mut out, root);
    section_loader(&mut out, root);
    section_runtime(&mut out, root);
    section_logs(&mut out, root);
    section_task_events(&mut out);
    section_defender(&mut out);
    section_inbound(&mut out, root);

    let _ = writeln!(out, "==== done ====");
    out
}

// ---------------------------------------------------------------------------
// 1. 系统与会话
// ---------------------------------------------------------------------------

/// 会话这一节为什么重要：计划任务用的是 `InteractiveToken`，服务必须跑在
/// **交互式桌面会话**里才能抓屏。会话在 0（服务会话）时，进程起得来、日志也
/// 正常，但采集出来是黑屏 —— 那种"看起来成功"的故障最难猜。
fn section_windows(out: &mut String) {
    let _ = writeln!(out, "==== 1. windows and session ====");
    let _ = writeln!(
        out,
        "arch         {}",
        std::env::var("PROCESSOR_ARCHITECTURE").unwrap_or_else(|_| "?".to_owned())
    );
    out.push_str(&tool("cmd.exe", &["/C", "ver"]));
    let _ = writeln!(out);
    let _ = writeln!(out, "-- quser（桌面会话必须是 console/1，0 表示无人在座）--");
    out.push_str(&tool("quser.exe", &[]));
    let _ = writeln!(out);
    let _ = writeln!(out, "-- whoami --");
    out.push_str(&tool("whoami.exe", &["/user"]));
    let _ = writeln!(out);
}

// ---------------------------------------------------------------------------
// 2. 账户策略
// ---------------------------------------------------------------------------

/// 这一节是历史遗留，但留着：老部署里可能有手工建的 `/RP`（存密码）任务，
/// 那种任务在"空密码 + `LimitBlankPasswordUse=1`"的机器上会以 0x800704DD
/// 失败，而现象同样只是"没起来"。现在安装器用 `InteractiveToken`，不需要密码。
fn section_account(out: &mut String) {
    let _ = writeln!(out, "==== 2. account policy ====");
    let _ = writeln!(out, "-- LimitBlankPasswordUse（只对遗留的 /RP 任务有意义）--");
    out.push_str(&tool(
        "reg.exe",
        &[
            "query",
            r"HKLM\SYSTEM\CurrentControlSet\Control\Lsa",
            "/v",
            "LimitBlankPasswordUse",
        ],
    ));
    let _ = writeln!(out);
    let _ = writeln!(out, "-- net user --");
    let user = std::env::var("USERNAME").unwrap_or_default();
    out.push_str(&tool("net.exe", &["user", &user]));
    let _ = writeln!(out);
}

// ---------------------------------------------------------------------------
// 3. 包内容
// ---------------------------------------------------------------------------

/// 必须存在的文件。
///
/// 这张表**跟着包的实际布局走**，不跟着历史走。旧脚本列的是它自己和
/// `set-password.cmd` —— 那两个文件 2026-11 删掉之后，那两条会永远报 MISSING，
/// 于是一份"体检报告"自己先坏了两行。
const EXPECTED: &[&str] = &[
    "stream-server.exe",
    "ViGEmClient.dll",
    "vcruntime140.dll",
    "stream-setting.exe",
    "stream-config.toml",
];

fn section_bundle(out: &mut String, root: &Path) {
    let _ = writeln!(out, "==== 3. bundle content ====");
    match std::fs::read_dir(root) {
        Ok(entries) => {
            let mut rows: Vec<String> = entries
                .filter_map(Result::ok)
                .map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let meta = entry.metadata().ok();
                    let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                    match (is_dir, meta) {
                        (true, _) => format!("<DIR>  {name}"),
                        (false, Some(m)) => format!("{:>10}  {name}", m.len()),
                        (false, None) => format!("{:>10}  {name}", "?"),
                    }
                })
                .collect();
            rows.sort();
            for row in rows {
                let _ = writeln!(out, "{row}");
            }
        }
        Err(err) => {
            let _ = writeln!(out, "列不出 {}：{err}", root.display());
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "-- files that must exist --");
    for name in EXPECTED {
        let _ = writeln!(out, "{} {name}", mark(root.join(name).is_file()));
    }
    // 前端根单独判：它是目录，而且"少了它"的表现是页面 404 而不是起不来。
    let index = root.join("selkies-web").join("index.html");
    let _ = writeln!(out, "{} selkies-web\\index.html", mark(index.is_file()));
    // 证书是可选的：没有就是明文 HTTP（剪贴板同步不可用），不算错。
    let cert = root.join("cert.pem");
    let _ = writeln!(out, "{} cert.pem（没有 = 明文 HTTP，剪贴板同步关闭）", mark(cert.is_file()));
    let _ = writeln!(out);
    let _ = writeln!(out, "-- exe size（被截断的拷贝是经典坏解压）--");
    match std::fs::metadata(root.join("stream-server.exe")) {
        Ok(meta) => {
            let _ = writeln!(out, "{} bytes", meta.len());
        }
        Err(err) => {
            let _ = writeln!(out, "读不到 stream-server.exe 的大小：{err}");
        }
    }
    let _ = writeln!(out);
}

fn mark(exists: bool) -> &'static str {
    if exists {
        "OK     "
    } else {
        "MISSING"
    }
}

// ---------------------------------------------------------------------------
// 4. 加载器前提
// ---------------------------------------------------------------------------

/// `stream-server.exe` 由 `--run` 拉起，加载器在**写第一行日志之前**就跑完。
/// 缺运行库的现象是"双击毫无反应"：没有窗口、没有日志、任务计划里只留一个
/// 退出码 `-1073741515`（0xC0000135）。
///
/// 这一节的语义 2026-11 变了：以前是"检查这台机器装没装 VC++"，现在是
/// "随包那一份在不在"。包里有 = 一定起得来；包里的没了才需要看系统目录。
fn section_loader(out: &mut String, root: &Path) {
    let _ = writeln!(out, "==== 4. loader prerequisites ====");
    let local = root.join("vcruntime140.dll");
    if local.is_file() {
        let size = std::fs::metadata(&local).map(|m| m.len()).unwrap_or(0);
        let _ = writeln!(out, "OK      {}\\vcruntime140.dll（{size} B，随包）", root.display());
    } else {
        let _ = writeln!(out, "MISSING vcruntime140.dll（随包那一份没了）—— 见下一行");
    }
    let system = std::env::var("SystemRoot")
        .map(|sys| PathBuf::from(sys).join("System32").join("vcruntime140.dll"))
        .unwrap_or_default();
    if system.is_file() {
        let _ = writeln!(out, "OK      {}（系统）", system.display());
    } else {
        let _ = writeln!(out, "MISSING {}（系统也没有）", system.display());
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "-- 刻意不从这里启动 exe --");
    let _ = writeln!(out, "它会抢屏、抢 8080，而且第二份会和正在工作的那一份打架。");
    let _ = writeln!(out, "要看真实报错，在这个目录开一个 cmd 然后跑：stream-server.exe --run");
    let _ = writeln!(out, "加载器失败会写 \"VCRUNTIME140.dll was not found\" 并返回 0xC0000135，");
    let _ = writeln!(out, "在那台机器上表现为退出码 -1073741515。");
    let _ = writeln!(out);
}

// ---------------------------------------------------------------------------
// 5. 运行状态
// ---------------------------------------------------------------------------

fn section_runtime(out: &mut String, root: &Path) {
    let _ = writeln!(out, "==== 5. runtime state ====");
    let _ = writeln!(out, "-- process --");
    // 排除自己。诊断进程的映像名就叫 `stream-server.exe`，`tasklist` 一定会
    // 把它列出来 —— 而那一行会让"服务没在跑"读成"服务在跑"，正好把这一节
    // 要回答的问题答反。
    let self_pid = std::process::id().to_string();
    let exclude = format!("PID ne {self_pid}");
    out.push_str(&tool(
        "tasklist.exe",
        &["/FI", "IMAGENAME eq stream-server.exe", "/FI", &exclude],
    ));
    let _ = writeln!(out);

    // 端口从**生效配置**读，不假设 8080：改过端口的机器上，按 8080 去探会得到
    // 一个"没在听"的假结论，而那时候服务其实好好跑着。
    let port = effective_port(root);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listening = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok();
    let _ = writeln!(out, "-- port {port}（127.0.0.1）--");
    let _ = writeln!(
        out,
        "{}",
        if listening {
            "OK      正在监听"
        } else {
            "NOT listening（这一行空白 = 没人听）"
        }
    );
    let _ = writeln!(out);

    let _ = writeln!(out, "-- scheduled task {TASK_NAME} --");
    out.push_str(&tool(
        "schtasks.exe",
        &["/Query", "/TN", TASK_NAME, "/FO", "LIST", "/V"],
    ));
    let _ = writeln!(out);

    let _ = writeln!(out, "-- 上次运行结果（0x41301 = 还在跑，0xFFFFFFFF = 从没跑过）--");
    out.push_str(&powershell(&format!(
        "try {{ $i = Get-ScheduledTaskInfo -TaskName '{TASK_NAME}' -ErrorAction Stop; \
         'LastRunTime    ' + $i.LastRunTime; 'LastTaskResult 0x{{0:X8}}' -f $i.LastTaskResult }} \
         catch {{ 'no task {TASK_NAME}: ' + $_.Exception.Message }}"
    )));
    let _ = writeln!(out);

    // principal 单独查：`LastTaskResult` 只说明"跑了没"，`LogonType` 说明
    // "跑在谁的桌面上"。后者是 Interactive 才有屏幕可抓。
    let _ = writeln!(
        out,
        "-- task principal（LogonType 必须是 Interactive，否则跑在桌面之外）--"
    );
    out.push_str(&powershell(&format!(
        "try {{ $t = Get-ScheduledTask -TaskName '{TASK_NAME}' -ErrorAction Stop; \
         'UserId    ' + $t.Principal.UserId; 'LogonType ' + $t.Principal.LogonType; \
         'RunLevel  ' + $t.Principal.RunLevel; 'State     ' + $t.State; \
         foreach ($a in $t.Actions) {{ 'Action    ' + $a.Execute + ' ' + $a.Arguments }} }} \
         catch {{ 'no task {TASK_NAME}: ' + $_.Exception.Message }}"
    )));
    let _ = writeln!(out);
}

/// 生效端口。配置文件里有就用它，否则 8080（`Config::default`）。
///
/// 不解析命令行：诊断跑的时候没有命令行可依，服务是任务拉起来的。配置是
/// "这台机器的常态"，也就是唯一稳定的答案。
fn effective_port(root: &Path) -> u16 {
    // 两个名字都认：整包分发是 `stream-config.toml`，开发机 / 手工部署是
    // `config.toml`。`resolve` 也是这个顺序，这里不调用它是因为要按 root
    // 找而不是按 exe 目录找（诊断可能在解开的包里跑）。
    for name in [configfile::BUNDLE_CONFIG, "config.toml"] {
        let path = root.join(name);
        if !path.is_file() {
            continue;
        }
        if let Some(port) = configfile::load(&path).ok().and_then(|file| file.port) {
            return port;
        }
    }
    8080
}

// ---------------------------------------------------------------------------
// 6. 日志尾部
// ---------------------------------------------------------------------------

fn section_logs(out: &mut String, root: &Path) {
    let _ = writeln!(out, "==== 6. logs (tail {LOG_TAIL} each) ====");
    let watchdog = root.join("logs").join("watchdog.log");
    if watchdog.is_file() {
        let _ = writeln!(out, "-- logs\\watchdog.log --");
        out.push_str(&tail(&watchdog, LOG_TAIL));
    } else {
        let _ = writeln!(
            out,
            "no watchdog.log —— `stream-server.exe --run` 没有从这个目录跑过"
        );
    }
    let _ = writeln!(out);
    let server = root.join("logs").join("stream-server.log");
    if server.is_file() {
        let _ = writeln!(out, "-- logs\\stream-server.log --");
        out.push_str(&tail(&server, LOG_TAIL));
    } else {
        let _ = writeln!(
            out,
            "no stream-server.log —— exe 没走到打开日志文件那一步"
        );
    }
    let _ = writeln!(out);
}

/// 文件最后 `lines` 行。
///
/// 整份读进来再切：这两个日志有上限（由日志库自己轮转），而"按字节 seek 到
/// 尾部"在这种文件大小上省不了什么，却要多一份"万一没找到换行"的分支。
fn tail(path: &Path, lines: usize) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let text = decode(&bytes);
            let all: Vec<&str> = text.lines().collect();
            let start = all.len().saturating_sub(lines);
            let mut out = String::new();
            for line in &all[start..] {
                let _ = writeln!(out, "{line}");
            }
            out
        }
        Err(err) => format!("读不出来：{err}\n"),
    }
}

// ---------------------------------------------------------------------------
// 7. 任务计划事件
// ---------------------------------------------------------------------------

/// 任务计划自己的日志比我们的日志更"上游"：任务被禁用、被策略拦下、动作路径
/// 写错，都在它这里留痕，而 `watchdog.log` 里一个字都不会有。
fn section_task_events(out: &mut String) {
    let _ = writeln!(out, "==== 7. task scheduler events (last 8) ====");
    out.push_str(&powershell(
        "try { Get-WinEvent -LogName 'Microsoft-Windows-TaskScheduler/Operational' \
         -MaxEvents 8 -ErrorAction Stop | ForEach-Object { \
         $_.TimeCreated.ToString('yyyy-MM-dd HH:mm:ss') + '  ' + $_.Id + '  ' + \
         ($_.Message -replace '\\s+', ' ') } } catch { 'task scheduler log: ' + $_.Exception.Message }",
    ));
    let _ = writeln!(out);
}

// ---------------------------------------------------------------------------
// 8. Defender
// ---------------------------------------------------------------------------

/// 杀软"静默处理"是这类"文件明明在、就是起不来"的经典成因：包里的 exe 被
/// 隔离，目录还在，文件没了。
fn section_defender(out: &mut String) {
    let _ = writeln!(out, "==== 8. defender ====");
    out.push_str(&powershell(
        "try { $d = Get-MpComputerStatus -ErrorAction Stop; \
         'RealTimeProtection ' + $d.RealTimeProtectionEnabled; \
         Get-MpThreatDetection -ErrorAction SilentlyContinue | Select-Object -Last 3 | \
         ForEach-Object { 'threat ' + $_.InitialDetectionTime.ToString() + ' id ' + $_.ThreatID } } \
         catch { 'defender query failed: ' + $_.Exception.Message }",
    ));
    let _ = writeln!(out);
}

// ---------------------------------------------------------------------------
// 9. 入站可达性
// ---------------------------------------------------------------------------

/// 这一节回答的是"服务好了，但别的机器连不上"。
///
/// 三个坑各自能让 8080 从外面看不见，而本地一切都正常：
/// * 网络配置文件是 **Public** —— Windows 默认丢弃 Public 上的所有入站；
/// * 防火墙规则在、但 profile 不含当前网络；
/// * 规则指着一个旧的 exe 路径（程序规则）。
fn section_inbound(out: &mut String, root: &Path) {
    let _ = writeln!(out, "==== 9. inbound reachability ====");

    let _ = writeln!(out, "-- 网络配置文件（Public 会丢光所有入站包）--");
    out.push_str(&powershell(
        "try { Get-NetConnectionProfile | ForEach-Object { \
         $_.InterfaceAlias + '  ' + $_.NetworkCategory } } \
         catch { 'Get-NetConnectionProfile failed: ' + $_.Exception.Message }",
    ));
    let _ = writeln!(out);

    let _ = writeln!(out, "-- 本机 IPv4（别的机器要用的地址）--");
    out.push_str(&powershell(
        "try { Get-NetIPAddress -AddressFamily IPv4 | \
         Where-Object { $_.IPAddress -ne '127.0.0.1' } | ForEach-Object { \
         $_.InterfaceAlias + '  ' + $_.IPAddress } } \
         catch { 'Get-NetIPAddress failed: ' + $_.Exception.Message }",
    ));
    let _ = writeln!(out);

    // 四条规则各一段。名称是安装器里写死的那几个（install.rs 的 RULE_*），
    // 改名会让这一节静默变成"四条都 NOT FOUND"。
    for (title, script) in [
        (
            "-- rule \"cloud-stream 8080\" --",
            "try { $r = Get-NetFirewallRule -DisplayName 'cloud-stream 8080' -ErrorAction Stop; \
             $r | ForEach-Object { 'enabled=' + $_.Enabled + '  direction=' + $_.Direction + \
             '  action=' + $_.Action + '  profile=' + $_.Profile }; \
             $r | Get-NetFirewallPortFilter | ForEach-Object { 'protocol=' + $_.Protocol + \
             '  localport=' + $_.LocalPort } } \
             catch { 'NOT FOUND -- run: stream-setting.exe --inbound (as administrator)' }",
        ),
        (
            "-- rule \"cloud-stream ICMPv4\" --",
            "try { $r = Get-NetFirewallRule -DisplayName 'cloud-stream ICMPv4' -ErrorAction Stop; \
             $r | ForEach-Object { 'enabled=' + $_.Enabled + '  direction=' + $_.Direction + \
             '  action=' + $_.Action + '  profile=' + $_.Profile } } \
             catch { 'NOT FOUND -- ping 会超时，即使服务本身没问题' }",
        ),
        (
            "-- rule \"cloud-stream webrtc UDP\"（默认 40000-40049）--",
            "try { $r = Get-NetFirewallRule -DisplayName 'cloud-stream webrtc UDP' -ErrorAction Stop; \
             $r | ForEach-Object { 'enabled=' + $_.Enabled + '  direction=' + $_.Direction + \
             '  action=' + $_.Action + '  profile=' + $_.Profile }; \
             $r | Get-NetFirewallPortFilter | ForEach-Object { 'protocol=' + $_.Protocol + \
             '  localport=' + $_.LocalPort } } \
             catch { 'NOT FOUND -- WebRTC 会报媒体通路失败，而 WebSockets 正常' }",
        ),
        (
            "-- rule \"cloud-stream app\"（程序规则，覆盖所有端口）--",
            "try { $r = Get-NetFirewallRule -DisplayName 'cloud-stream app' -ErrorAction Stop; \
             $r | ForEach-Object { 'enabled=' + $_.Enabled + '  direction=' + $_.Direction + \
             '  action=' + $_.Action + '  profile=' + $_.Profile }; \
             $r | Get-NetFirewallApplicationFilter | ForEach-Object { 'program=' + $_.Program } } \
             catch { 'NOT FOUND -- run: stream-setting.exe --inbound (as administrator)' }",
        ),
    ] {
        let _ = writeln!(out, "{title}");
        out.push_str(&powershell(script));
        let _ = writeln!(out);
    }

    // 启动期自检的结论行。这是"服务自己怎么看"与"外面怎么看"之间最短的一条
    // 线索：它在每次启动时写一行，成功失败都有。
    let _ = writeln!(out, "-- 启动自检结论（watchdog.log 里的 PREFLIGHT）--");
    let watchdog = root.join("logs").join("watchdog.log");
    match std::fs::read(&watchdog) {
        Ok(bytes) => {
            let text = decode(&bytes);
            let hits: Vec<&str> = text
                .lines()
                .filter(|line| line.contains("PREFLIGHT"))
                .collect();
            if hits.is_empty() {
                let _ = writeln!(out, "没有 PREFLIGHT 行 —— 这个包早于启动自检");
            } else {
                let start = hits.len().saturating_sub(12);
                for line in &hits[start..] {
                    let _ = writeln!(out, "{line}");
                }
            }
        }
        Err(_) => {
            let _ = writeln!(out, "没有 watchdog.log —— 计划任务没有从这个目录跑过");
        }
    }
    let _ = writeln!(out);
}

// ---------------------------------------------------------------------------
// 跑命令
// ---------------------------------------------------------------------------

/// 跑一条系统命令，返回它的输出（stdout 与 stderr 合并）。
///
/// 三条规矩：
/// * **绝对路径**。旧脚本踩过"PATH 里排着一个 conda 的 powershell"这种事，
///   在别人的机器上重演一遍没有意义。
/// * **不经过 cmd 包装**。`cmd /C "chcp 65001 & ..."` 看起来能统一编码，但
///   Rust 给参数加引号之后 `cmd` 的引号规则会把路径搞坏。编码在 [`decode`]
///   里处理，更直接。
/// * 命令不存在不算错：某些精简版 Windows 没有 `quser.exe`，那时输出一行
///   说明比"整个诊断崩掉"有用。
fn tool(program: &str, args: &[&str]) -> String {
    let exe = sys32(program);
    let mut cmd = Command::new(&exe);
    cmd.args(args);
    match cmd.output() {
        Ok(output) => {
            let mut text = decode(&output.stdout);
            if !output.stderr.is_empty() {
                text.push_str(&decode(&output.stderr));
            }
            if text.is_empty() {
                text.push_str("(没有输出)\n");
            }
            text
        }
        Err(err) => format!("跑不起来 {}：{err}\n", exe.display()),
    }
}

/// 跑一段 PowerShell。
///
/// `-NonInteractive` 是必须的：机器上装了 PowerShell 7 时，`PSModulePath` 常
/// 指向 pwsh 的模块目录，5.1 会去加载那些签名不受信任的 `.ps1xml` 并停在交互
/// 式询问上 —— 在被控机上看起来就是"诊断卡住了"。这里把 `PSModulePath` 收窄
/// 到 5.1 自己的目录，再加 `-NonInteractive` 兜底。
fn powershell(script: &str) -> String {
    let exe = sys32(r"WindowsPowerShell\v1.0\powershell.exe");
    let mut cmd = Command::new(&exe);
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        script,
    ]);
    if let Ok(system_root) = std::env::var("SystemRoot") {
        cmd.env(
            "PSModulePath",
            format!(r"{system_root}\System32\WindowsPowerShell\v1.0\Modules"),
        );
    }
    match cmd.output() {
        Ok(output) => {
            let mut text = decode(&output.stdout);
            if !output.stderr.is_empty() {
                text.push_str(&decode(&output.stderr));
            }
            if text.is_empty() {
                text.push_str("(没有输出)\n");
            }
            text
        }
        Err(err) => format!("跑不起来 {}：{err}\n", exe.display()),
    }
}

/// `%SystemRoot%\System32\` 下的一个程序。
fn sys32(program: &str) -> PathBuf {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
    PathBuf::from(root).join("System32").join(program)
}

/// 把行尾统一成 CRLF，已经是的不要加第二遍。
///
/// 不能简单 `replace('\n', "\r\n")`：正文里有些段落直接来自子进程输出
/// （`tasklist` / `net user` 给的本来就是 CRLF），那样会变成 `\r\r\n` ——
/// 记事本里每行之间多一个空行，同样难读。
fn crlf(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 32);
    let mut prev = '\0';
    for ch in text.chars() {
        if ch == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(ch);
        prev = ch;
    }
    out
}

/// 把子进程的输出字节解成文本。
///
/// 先按 UTF-8 试，失败再用系统 ANSI 代码页（中文机器上是 936）。这不是洁癖：
/// 控制台程序在管道里的输出是**代码页**编码的，而 `net user`、`tasklist`
/// 的输出标题全是中文，按 UTF-8 硬读会得到一串问号 —— 恰恰是诊断文件里最没
/// 用的东西。
fn decode(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Globalization::{MultiByteToWideChar, CP_ACP};
        let len = bytes.len() as i32;
        unsafe {
            let need =
                MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0);
            if need > 0 {
                let mut buf = vec![0u16; need as usize];
                let got = MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), len, buf.as_mut_ptr(), need);
                if got > 0 {
                    buf.truncate(got as usize);
                    return String::from_utf16_lossy(&buf);
                }
            }
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ASCII 走 UTF-8 那条快路，一个字节都不该变。
    #[test]
    fn plain_ascii_decodes_as_utf8() {
        assert_eq!(decode(b"listening on 8080\n"), "listening on 8080\n");
    }

    /// GBK 的"映像名称"（0xD3 0xB3 0xCF 0xF1 0xC3 0xFB 0xB3 0xC6）不是合法
    /// UTF-8，必须落到代码页那条路上。断言具体汉字是为了守住"回退真的解码了"
    /// —— 只断言"不是空"的话，`from_utf8_lossy` 那串 U+FFFD 也能过。
    #[test]
    fn gbk_falls_back_to_the_ansi_code_page() {
        let gbk = [0xD3u8, 0xB3, 0xCF, 0xF1, 0xC3, 0xFB, 0xB3, 0xC6];
        let text = decode(&gbk);
        // 非中文系统上 CP_ACP 不是 936，这时只要求"解码没崩且没丢字节"。
        assert!(!text.is_empty());
        #[cfg(windows)]
        {
            let ansi = unsafe { windows_sys::Win32::Globalization::GetACP() };
            if ansi == 936 {
                assert_eq!(text, "映像名称");
            }
        }
    }

    /// 纯 LF 要补成 CRLF，已经是 CRLF 的不能变成 `\r\r\n`。
    ///
    /// 后半句才是重点：正文里夹着 `tasklist` / `net user` 的原样输出（本身就是
    /// CRLF），简单 replace 会让那些行之间多出空行 —— 现象是"文件能读，但每段
    /// 之间空一行"，很容易被当成功能正常。
    #[test]
    fn crlf_is_added_once_and_only_once() {
        assert_eq!(crlf("a\nb\n"), "a\r\nb\r\n");
        assert_eq!(crlf("a\r\nb\r\n"), "a\r\nb\r\n");
        assert_eq!(crlf("a\nb\r\nc"), "a\r\nb\r\nc");
        assert_eq!(crlf(""), "");
    }

    /// 尾部截取的边界：不足 N 行时全给，正好 N 行时不多一行空。
    #[test]
    fn tail_keeps_the_last_lines() {
        let dir = std::env::temp_dir().join(format!("cs-diag-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.log");
        std::fs::write(&path, "1\n2\n3\n4\n").unwrap();
        assert_eq!(tail(&path, 2), "3\n4\n");
        assert_eq!(tail(&path, 9), "1\n2\n3\n4\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端口来自配置文件，不是写死的 8080 —— 否则改过端口的机器上这一节永远
    /// 报"没在听"，而服务其实是好的。
    #[test]
    fn port_comes_from_the_config_file() {
        let dir = std::env::temp_dir().join(format!("cs-diag-port-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(configfile::BUNDLE_CONFIG), "port = 9443\n").unwrap();
        assert_eq!(effective_port(&dir), 9443);
        // 没有配置文件时退回默认值，不是报错：诊断要能在"包都缺了"的机器上跑。
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(effective_port(&empty), 8080);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
