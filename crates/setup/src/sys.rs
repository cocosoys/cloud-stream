//! 安装和卸载都要用的那几个 Windows 调用。
//!
//! 分成三类，都是"两个流程一模一样、抄一份就会分叉"的东西：
//!
//! * 说话 —— `dialog` / `confirm`（GUI 子系统里唯一的输出方式）；
//! * 跑系统程序 —— `command` / `netsh` / `run_bounded`（都不闪黑框）；
//! * 提权与账户 —— `elevated` / `elevate_raw` / `current_account`。
//!
//! # 为什么提权是"原样交出参数"而不是"交出结构化的意图"
//!
//! 提权是**重新跑一遍自己**（`ShellExecuteW` + `runas`），子进程拿到的是一张
//! 命令行。所以调用方给什么参数，子进程就按什么参数解析 —— 中间没有一层"把
//! 意图翻译成参数"的映射，也就没有"翻译错了"这种失败方式。`--dir` 里带空格
//! 时这里只负责把它包进引号，其余一个字都不动。
//!
//! # 为什么 `run_bounded` 要自己实现超时
//!
//! `stream-server.exe --restore-mic` 自己会 5 秒退出，但那是在"它是我认识的那个
//! 版本"的前提下。卸载时遇到的第一个问题恰恰是"这个 exe 可能是旧版本" —— 旧
//! 版本不认识这个开关，`Config::parse` 会把不知道的参数忽略掉，于是它起的是一个
//! **完整的服务**：永不返回、占住端口、顺手把录音设备再抢一遍。所以调用方先用
//! `exe_has_marker` 确认能力，这里再用超时兜住"确认了也不返回"的那一种。

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Globalization::{GetOEMCP, MultiByteToWideChar};
use windows_sys::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONWARNING, MB_OK, MB_YESNO, SW_SHOWNORMAL,
};

/// 子进程不要弹黑框。
///
/// 安装/卸载全程都是这一套 exe 在动，中间闪一下黑框就是"装了个奇怪的东西"。
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ---------------------------------------------------------------------------
// 说话
// ---------------------------------------------------------------------------

/// 这一次运行不说话（`--quiet`）。
///
/// 外面套了一层安装器（Inno / MSI）时用它：结论走**退出码 + 结论文件**，
/// 弹框只会和外面那层向导打架。更糟的是无人值守安装里，一个 `MessageBox`
/// 会让进程一直等到有人点确定 —— 那正是 `--quiet` 要消掉的东西。
///
/// 做成进程级开关而不是给每个函数加参数：`dialog` 的调用点有二十多个，
/// 逐个传参会把"静默"扩散进所有签名，而它本来只是这一次运行的一个属性。
static QUIET: AtomicBool = AtomicBool::new(false);

pub fn set_quiet(on: bool) {
    QUIET.store(on, Ordering::Relaxed);
}

pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

/// UTF-16 转换。Win32 的 `W` 系列全要这个，每个调用点自己写一遍就是
/// `encode_utf16().chain(once(0))` 抄 N 遍。
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 一个只有"知道了"按钮的对话框。
///
/// `flags` 用 `MB_ICONERROR` / `MB_ICONWARNING` / `MB_ICONINFORMATION`。
///
/// `--quiet` 下不弹，改写到 stderr：文字本身有用（被重定向时能查），
/// 但不能占着屏幕等人点。
pub fn dialog(text: &str, caption: &str, flags: u32) {
    if quiet() {
        eprintln!("[{caption}] {text}");
        return;
    }
    let t = wide(text);
    let c = wide(caption);
    unsafe {
        MessageBoxW(0, t.as_ptr(), c.as_ptr(), MB_OK | flags);
    }
}

/// 是/否。返回 true = 用户点了"是"。
///
/// 默认按钮是"否"（`MB_YESNO` 的默认是第一个按钮，而 `MB_DEFBUTTON2` 才是
/// 第二个）—— 这里刻意**不加** `MB_DEFBUTTON2`：卸载和安装的确认框里，
/// 用户是主动点进来的，回车等于确认是合理的。真正危险的那个（删整个目录）
/// 在 `uninstall` 里单独列了清单。
///
/// `--quiet` 下视为"是"：静默和 `--yes` 是同一件事的两面 —— 外面那层
/// 已经问过用户了。
pub fn confirm(text: &str, caption: &str) -> bool {
    if quiet() {
        return true;
    }
    let t = wide(text);
    let c = wide(caption);
    // IDYES = 6
    unsafe { MessageBoxW(0, t.as_ptr(), c.as_ptr(), MB_YESNO | MB_ICONWARNING) == 6 }
}

// ---------------------------------------------------------------------------
// 跑系统程序
// ---------------------------------------------------------------------------

/// 把一段子进程输出解成文本。
///
/// # 为什么不能一律 `from_utf8_lossy`
///
/// 这里踩过一次坑，代价是一个假失败：`schtasks` 是**系统工具**，输出走控制台
/// 代码页（中文机器是 936/GBK），`from_utf8_lossy` 会把
/// `成功: 成功创建计划任务 "MCStreamSrv"。` 解成 `鎴愬姛: …`。而
/// `register_task` 当时正是靠 `contains("成功")` 判断成败的 ——
/// 于是**装成功也报失败**：任务已创建、服务已监听 8080，界面却弹"注册计划任务
/// 失败"（2026-11-11 在 .150 上实测）。
///
/// 反过来的情况同样真实：`--ops rollback` 的输出来自我们自己那个 Rust 程序，
/// 是 UTF-8。所以**先严格试 UTF-8，失败再按 OEM 代码页**，两边都不猜。
///
/// 边界：一段 GBK 字节恰好构成合法 UTF-8 的概率极低但不为零（GBK 首字节
/// 0x81-0xFE 里有 0xC2-0xF4 这一段）。真撞上时会解成另一种文字 —— 所以
/// **判成败不要看文本**，看退出码（`run` 会把它带回来）。
pub fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => decode_oem(bytes),
    }
}

/// 按系统 OEM 代码页解码。
///
/// 用 `GetOEMCP()` 而不是写死 936：日文机是 932、西欧机是 850，`MultiByteToWideChar`
/// 认识这些表，而我们把代码页写死就等于只支持一种 Windows。
fn decode_oem(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let cp = unsafe { GetOEMCP() };
    let len = unsafe {
        MultiByteToWideChar(
            cp,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        )
    };
    if len <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    // `len` 是 UTF-16 单元数（不含结尾 NUL，因为刚才传的 cchWideChar = 0）。
    let mut wide = vec![0u16; len as usize];
    let written = unsafe {
        MultiByteToWideChar(
            cp,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            len,
        )
    };
    if written <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    wide.truncate(written as usize);
    String::from_utf16_lossy(&wide)
}

/// 跑一个系统命令，返回 `(退出码, 输出)`。stdout 与 stderr 首尾相接。
///
/// **判成败用退出码，不要用文本。** 靠 `contains("success")` 判断有两个独立的
/// 失效方式：文案随语言包变（英文机是 SUCCESS，中文机是"成功"），以及任何一次
/// 编码变化让那句文案根本不存在。`schtasks` 的退出码只有 0 / 非 0 两种含义，
/// 它不会翻译。
pub fn run(program: &str, args: &[&str]) -> Result<(i32, String), String> {
    let out = std::process::Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("起不了 {program}：{e}"))?;
    let mut text = decode(&out.stdout);
    text.push_str(&decode(&out.stderr));
    Ok((out.status.code().unwrap_or(-1), text))
}

/// 跑一个系统命令，只要文本（不要退出码时用）。
///
/// 唯一的错误是"进程都没起来"；非 0 退出但**有输出**的，照样把输出交回去 ——
/// 那种输出就是失败原因，吞掉它只会让人无从查起。
pub fn command(program: &str, args: &[&str]) -> Result<String, String> {
    let (code, text) = run(program, args)?;
    if code != 0 && text.trim().is_empty() {
        return Err(format!("{program} 退出码 {code}"));
    }
    Ok(text)
}

/// 跑一条 netsh 子命令。成功返回空串，失败返回 stderr（给日志看）。
///
/// 整条命令用 `raw_arg` 原样交出去。规则名里有空格：
/// `name="cloud-stream 8080"`，让 Rust 自己拼命令行会被二次加引号成
/// `name=""cloud-stream 8080""`，netsh 只认得前一种。
pub fn netsh(raw: &str) -> Result<String, String> {
    let out = std::process::Command::new("netsh")
        .raw_arg(raw)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("起不了 netsh：{e}"))?;
    if out.status.success() {
        return Ok(String::new());
    }
    // 和 `command` 同一个理由：netsh 的报错是 GBK，直接 `from_utf8_lossy`
    // 会把"需要提升的权限。"解成一串问号，而那正是要看的那句话。
    let mut text = decode(&out.stderr).trim().to_owned();
    if text.is_empty() {
        text = decode(&out.stdout).trim().to_owned();
    }
    if text.is_empty() {
        text = format!("退出码 {}", out.status.code().unwrap_or(-1));
    }
    Err(text)
}

/// 跑一个有超时的子进程，返回（退出码，合并后的输出）。
///
/// 超时之后 `TerminateProcess` 而不是"就这样吧"：那正是"服务已经起来了、
/// 占着端口、正准备永远跑下去"的那一种，放它活着等于把卸载卡死在这儿。
pub fn run_bounded(
    program: &Path,
    args: &[&str],
    cwd: &Path,
    seconds: u64,
) -> Result<(i32, String), String> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = std::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("起不了 {}：{e}", program.display()))?;

    // 两个读线程：不读的话管道写满 4 KB，子进程会阻塞在 write 上，
    // 于是"超时"变成唯一可能的结局 —— 而失败原因看起来像"这个程序坏了"。
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out_thread = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err_thread = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut killed = false;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => {}
            Err(_) => break -1,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            killed = true;
            break child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let mut text = String::from_utf8_lossy(&out_thread.join().unwrap_or_default()).into_owned();
    text.push_str(&String::from_utf8_lossy(&err_thread.join().unwrap_or_default()));
    if killed {
        text.push_str(&format!("\n[timeout] {seconds} 秒没有退出，已强制结束"));
    }
    Ok((code, text))
}

// ---------------------------------------------------------------------------
// 提权与账户
// ---------------------------------------------------------------------------

pub fn elevated() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

pub enum ElevateError {
    /// 用户拒了 UAC。
    Denied,
    /// 其它失败，带 ShellExecute 的返回值（<= 32 的都是错误码）。
    Failed(isize),
}

/// 提权重跑自己，参数字符串由调用方给。
///
/// `dir` 是子进程的启动目录。**不要传当前目录**：提权进程的当前目录在
/// 某些情况下是 `System32`，而 `--run` 与 `--ops` 都按"exe 所在目录"算运行根，
/// 传它只是为了让日志和相对路径有个合理起点。
pub fn elevate_raw(params: &str, dir: &Path) -> Result<(), ElevateError> {
    let me = std::env::current_exe().map_err(|_| ElevateError::Failed(0))?;
    let verb = wide("runas");
    let file = wide(&me.to_string_lossy());
    let args = wide(params);
    let cwd = wide(&dir.display().to_string());
    let rc = unsafe {
        ShellExecuteW(
            0,
            verb.as_ptr(),
            file.as_ptr(),
            args.as_ptr(),
            cwd.as_ptr(),
            SW_SHOWNORMAL as i32,
        )
    } as isize;
    if rc > 32 {
        return Ok(());
    }
    // 5 = 访问被拒（UAC 里点了"否"）；1223 = ERROR_CANCELLED，也是"否"。
    if rc == 5 || rc == 1223 {
        return Err(ElevateError::Denied);
    }
    Err(ElevateError::Failed(rc))
}

/// 当前账户，`DOMAIN\user` 形式（任务 XML 的 `UserId` 要的就是这个写法）。
///
/// **不能只写 `%USERNAME%`**：实测任务计划程序会拒绝注册，报
/// "账户名与安全标识间无任何映射完成 (5,145)"。
pub fn current_account() -> Option<String> {
    // 先问 `whoami`。它给的 `computer\user` / `domain\user` 正是 Task Scheduler
    // 认的那种写法，不依赖环境变量怎么填。
    //
    // # 为什么不能拿 `%USERDOMAIN%` 拼
    //
    // 工作组（非域）机器上 `%USERDOMAIN%` 会返回字面量 `WORKGROUP` —— 那是
    // 工作组的名字，不是任何账户所在的容器。拼出来的 `WORKGROUP\admin` 到
    // `schtasks /Create` 那里是"帐户名与安全标识间无任何映射完成"（.150 实测，
    // 2026-11-11）。同一个 bug 只在"从会话/脚本里跑"时才露头：双击运行时
    // `%USERDOMAIN%` 又是计算机名，看起来完全正常。
    //
    // whoami 的输出是 ASCII（账户名本身可能是中文，那种情况下面 `run` 已经按
    // OEM 解过），第一行就是身份。
    if let Ok((0, text)) = run("whoami", &[]) {
        let line = text.lines().next().unwrap_or("").trim();
        if line.contains('\\') {
            return Some(line.to_owned());
        }
    }
    // 退路：环境变量。`WORKGROUP` 不是容器名，遇到它就换成计算机名。
    account_from_env(
        std::env::var("USERNAME").ok(),
        std::env::var("USERDOMAIN").ok(),
        std::env::var("COMPUTERNAME").ok(),
    )
}

/// `whoami` 不可用时的退路。
///
/// 抽成纯函数是为了能钉住 `WORKGROUP` 那条规则 —— 它是这次 bug 的核心，
/// 而 `whoami` 在开发机上永远成功，这段代码平时根本走不到。
fn account_from_env(
    user: Option<String>,
    domain: Option<String>,
    computer: Option<String>,
) -> Option<String> {
    let user = user.filter(|s| !s.is_empty())?;
    match domain.filter(|s| !s.is_empty()) {
        // 域机器：`%USERDOMAIN%` 就是域名，直接用。
        Some(d) if !d.eq_ignore_ascii_case("WORKGROUP") => Some(format!("{d}\\{user}")),
        // 工作组机器：换成计算机名。都拿不到就只给用户名 ——
        // 那在 Task Scheduler 里多半解析不了，但至少比 `WORKGROUP\x` 诚实。
        _ => computer
            .filter(|s| !s.is_empty())
            .map(|c| format!("{c}\\{user}"))
            .or(Some(user)),
    }
}

// ---------------------------------------------------------------------------
// 能力探测
// ---------------------------------------------------------------------------

/// exe 里有没有这段 ASCII 字节。
///
/// 用来判断"这个 `stream-server.exe` 认不认识某个开关"。**不能拿开关名去探**：
/// rustc 会把短字符串字面量折成立即数比较，`--restore-mic` 这种名字在二进制
/// 里根本不存在（实测 0 次命中）。要探的是那条路径上**输出**的文本，比如
/// `MICRESTORE`（只在有还原逻辑的构建里才有）。
///
/// 为什么这个探测是必须的：旧版本 exe 不认识 `--restore-mic`，`Config::parse`
/// 会忽略不认识的参数，于是它**起一个完整的服务** —— 永不返回、占住端口、
/// 顺手把录音设备再抢一遍。卸载会卡死在那儿，而看起来像"卡在第三步"。
pub fn exe_has_marker(exe: &Path, needle: &str) -> bool {
    has_marker_with_chunk(exe, needle, 256 * 1024)
}

/// 分块扫，块与块之间保留 `needle.len() - 1` 个字节的重叠。
///
/// 重叠不是可选项：标记跨块边界时，任何"逐块独立搜索"的实现都会漏掉它，
/// 而漏掉的后果是"明明有这个能力，却被当成旧版本跳过了"。
fn has_marker_with_chunk(exe: &Path, needle: &str, chunk: usize) -> bool {
    use std::io::Read;

    let needle = needle.as_bytes();
    if needle.is_empty() {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(exe) else {
        return false;
    };
    let chunk = chunk.max(needle.len());
    let mut buf = vec![0u8; chunk];
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let read = match file.read(&mut buf) {
            Ok(0) => return false,
            Ok(n) => n,
            Err(_) => return false,
        };
        let mut window = Vec::with_capacity(carry.len() + read);
        window.extend_from_slice(&carry);
        window.extend_from_slice(&buf[..read]);
        if window.windows(needle.len()).any(|wv| wv == needle) {
            return true;
        }
        let keep = needle.len().saturating_sub(1).min(window.len());
        carry = window[window.len() - keep..].to_vec();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_file(name: &str, data: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("cs-marker-{}-{name}", std::process::id()));
        std::fs::write(&path, data).unwrap();
        path
    }

    #[test]
    fn finds_marker_inside_a_chunk() {
        let path = tmp_file("inside", b"....MICRESTORE....");
        assert!(exe_has_marker(&path, "MICRESTORE"));
        let _ = std::fs::remove_file(&path);
    }

    /// 跨块边界的那一条 —— 少了重叠就会漏，而漏了的表现是"这个 exe 明明有
    /// 这个能力，却被当成旧版本跳过"。
    #[test]
    fn finds_marker_across_chunk_boundary() {
        // 块长 4、标记 10 字节：起始位置让它横跨每一条边界。
        let data = b"aaaMICRESTOREbbb".to_vec();
        let path = tmp_file("across", &data);
        for offset in 0..6 {
            let padded = [vec![b'x'; offset], data.clone()].concat();
            std::fs::write(&path, &padded).unwrap();
            assert!(
                has_marker_with_chunk(&path, "MICRESTORE", 4),
                "offset {offset} 时跨块漏掉了"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_marker_and_missing_file_are_false() {
        let path = tmp_file("absent", b"nothing to see here");
        assert!(!exe_has_marker(&path, "MICRESTORE"));
        let _ = std::fs::remove_file(&path);
        assert!(!exe_has_marker(&path, "MICRESTORE"));
    }

    /// 空标记不能命中一切 —— `windows(...)` 在空切片上会退化成逐字节比较。
    #[test]
    fn empty_marker_never_matches() {
        let path = tmp_file("empty", b"anything");
        assert!(!exe_has_marker(&path, ""));
        let _ = std::fs::remove_file(&path);
    }

    // ---- 子进程输出的解码 -------------------------------------------------

    /// ASCII 两边都要无损：`--run` / `InteractiveToken` 这些判据全在 ASCII 里，
    /// 解码器连它们都动了就什么都别谈了。
    #[test]
    fn decode_keeps_ascii_untouched() {
        assert_eq!(
            decode(b"SUCCESS: The scheduled task \"MCStreamSrv\" was created."),
            "SUCCESS: The scheduled task \"MCStreamSrv\" was created."
        );
        assert_eq!(decode(b""), "");
    }

    /// 我们自己那个 exe 的输出（`--ops rollback` 之类）是 UTF-8，不能被 OEM
    /// 那一路劫走 —— 否则中文提示会变成同样难读的东西，只是换了个方向错。
    #[test]
    fn decode_prefers_utf8_when_bytes_are_valid_utf8() {
        assert_eq!(decode("已放回配置".as_bytes()), "已放回配置");
    }

    /// GBK 字节必须按 OEM 代码页解回来。
    ///
    /// 这是 .150 上那次假失败的现场："成功" 两个字的 GBK 字节是 B3 C9 B9 A6，
    /// 当成 UTF-8 解就是 `鎴愬姛`（并且 `from_utf8_lossy` 还会把不成序列的字节
    /// 换成 U+FFFD），于是 `contains("成功")` 永远为假。
    ///
    /// 只在 936 的机器上立得住：换成 1252 的机器，同样的字节在那边本来就是
    /// 另一串西欧字母，"GBK 被误当 UTF-8" 这个场景不存在。
    #[test]
    fn decode_reads_gbk_bytes_as_chinese() {
        if unsafe { GetOEMCP() } != 936 {
            return;
        }
        assert_eq!(decode(&[0xB3, 0xC9, 0xB9, 0xA6]), "成功");
        // 整行（照抄 .150 上 schtasks 实际输出的字节）
        let line = [
            0xB3, 0xC9, 0xB9, 0xA6, b':', b' ', // "成功: "
            0xB3, 0xC9, 0xB9, 0xA6, // "成功"
        ];
        assert_eq!(decode(&line), "成功: 成功");
    }

    /// 断言"按 UTF-8 解一定读不出『成功』" —— 这条钉住的不是解码器，而是
    /// **当初那个判据为什么错**。将来有人想改回文本判定，会先看到这里。
    #[test]
    fn gbk_success_line_is_unreadable_as_utf8() {
        let gbk = [0xB3, 0xC9, 0xB9, 0xA6];
        let wrong = String::from_utf8_lossy(&gbk);
        assert!(!wrong.contains('成'));
        assert_ne!(wrong, "成功");
    }

    // ---- 账户名 -----------------------------------------------------------

    /// 工作组机器上 `%USERDOMAIN%` 是 `WORKGROUP`，它**不是**容器名。
    ///
    /// .150 就栽在这里：拼出 `WORKGROUP\admin`，`schtasks /Create` 答
    /// "帐户名与安全标识间无任何映射完成"。而同一台机器双击运行时
    /// `%USERDOMAIN%` 又变成计算机名 —— 于是这个错只在脚本/会话里跑时才露头。
    #[test]
    fn workgroup_is_not_a_container_name() {
        assert_eq!(
            account_from_env(
                Some("admin".to_owned()),
                Some("WORKGROUP".to_owned()),
                Some("DESKTOP-0L2MMH3".to_owned())
            ),
            Some("DESKTOP-0L2MMH3\\admin".to_owned())
        );
        // 大小写不敏感：环境变量里小写的 workgroup 也要认出来。
        assert_eq!(
            account_from_env(
                Some("admin".to_owned()),
                Some("workgroup".to_owned()),
                Some("PC".to_owned())
            ),
            Some("PC\\admin".to_owned())
        );
        // 域机器照旧用域名。
        assert_eq!(
            account_from_env(
                Some("me".to_owned()),
                Some("CORP".to_owned()),
                Some("PC".to_owned())
            ),
            Some("CORP\\me".to_owned())
        );
        // 连计算机名都没有：只给用户名，不要编一个域出来。
        assert_eq!(
            account_from_env(Some("me".to_owned()), None, None),
            Some("me".to_owned())
        );
        assert_eq!(account_from_env(None, Some("CORP".to_owned()), None), None);
    }
}
