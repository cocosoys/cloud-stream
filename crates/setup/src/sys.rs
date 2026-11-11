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
use std::time::{Duration, Instant};

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

/// UTF-16 转换。Win32 的 `W` 系列全要这个，每个调用点自己写一遍就是
/// `encode_utf16().chain(once(0))` 抄 N 遍。
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 一个只有"知道了"按钮的对话框。
///
/// `flags` 用 `MB_ICONERROR` / `MB_ICONWARNING` / `MB_ICONINFORMATION`。
pub fn dialog(text: &str, caption: &str, flags: u32) {
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
pub fn confirm(text: &str, caption: &str) -> bool {
    let t = wide(text);
    let c = wide(caption);
    // IDYES = 6
    unsafe { MessageBoxW(0, t.as_ptr(), c.as_ptr(), MB_YESNO | MB_ICONWARNING) == 6 }
}

// ---------------------------------------------------------------------------
// 跑系统程序
// ---------------------------------------------------------------------------

/// 跑一个系统命令，把 stdout 和 stderr 一起拿回来。
///
/// 注意这是**文本**：`schtasks` 的输出在中文系统上是 GBK，转成 UTF-8 时非 ASCII
/// 部分会变成替换字符。所以调用方只应在这段文本里找 ASCII 片段（`--run`、
/// `InteractiveToken`、`<Enabled>false</Enabled>`），不要指望读中文。
pub fn command(program: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("起不了 {program}：{e}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    if !out.status.success() && text.trim().is_empty() {
        return Err(format!(
            "{program} 退出码 {}",
            out.status.code().unwrap_or(-1)
        ));
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
    let mut text = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    if text.is_empty() {
        text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
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
    let user = std::env::var("USERNAME").ok().filter(|s| !s.is_empty())?;
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => Some(format!("{domain}\\{user}")),
        _ => Some(user),
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
}
