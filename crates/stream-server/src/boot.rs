//! 进程启动前的平台准备：GUI 子系统的 stdio、panic 落盘。
//!
//! 这个模块只有一个调用点（`main.rs` 第一行），但它存在的原因是三条会**静默**
//! 杀死服务的平台行为 —— 静默是关键词：它们不报错、不留日志，进程就没了。
//!
//! ## 1. 为什么必须是 GUI 子系统
//!
//! `stream-server.exe` 由计划任务每分钟拉起一次。console 子系统的程序在这种
//! 场合会被 Windows 代建一个控制台窗口：每次触发都闪一下，并且抢走全屏游戏的
//! 前台焦点。此前用一个 28 行的 `run-hidden.vbs` 去消掉它（`WScript.Shell.Run`
//! 带窗口模式 0）—— 那是绕，不是解。
//!
//! GUI 子系统的进程"自己负责画界面"，Windows 不会代劳，所以没有窗口。代价是
//! **启动时标准句柄是无效的**，而 Rust 的 `println!` 在写失败时会 panic。于是
//! 有了下面第 2 条。
//!
//! ## 2. stdio 的三种处境
//!
//! | 谁拉起的 | 标准句柄 | 该做什么 |
//! |---|---|---|
//! | 计划任务（无控制台、无重定向） | 无效 | 指到 `NUL`：让 `println!` 变成成功的空操作 |
//! | 从 cmd / PowerShell 手动跑 | 无效（GUI 进程不继承控制台） | `AttachConsole` 接上父控制台，输出照常 |
//! | 被 `Command::output()` 用管道抓 | **有效** | 什么都不做，否则会把管道换掉 |
//!
//! 第三行是 `--version json` 能被更新器读到的全部原因：如果无条件
//! `AttachConsole`，那条管道就被控制台顶掉，更新器永远读不到版本。
//!
//! ## 3. panic 去哪了
//!
//! GUI 子系统下 `stderr` 指向 `NUL`，默认 panic hook 的输出就此蒸发 —— 现象是
//! "服务跑一会儿就没了，日志里什么都没有"。所以 hook 自己写一份
//! `logs\crash.log`，同时保留原来的 stderr 输出（有控制台时照样看得见）。

/// 进程启动时就做、且必须**早于任何输出**的准备工作。
///
/// `main` 的第一行调用它。放在 `#[tokio::main]` 里也是安全的：运行时的创建不
/// 碰标准输出，而 `std::io::stdout()` 的句柄是第一次使用时才取的。
pub fn prepare() {
    fix_stdio();
    install_panic_hook();
}

/// GUI 子系统下的标准句柄修复。参见模块文档的三种处境表。
#[cfg(windows)]
fn fix_stdio() {
    use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn usable(handle: isize) -> bool {
        handle != 0 && handle != INVALID_HANDLE_VALUE as isize
    }

    unsafe {
        // 被管道/文件重定向时句柄是有效的 —— 绝对不能动，那是 `--version json`
        // 与 `--print-config > file` 的唯一通路。
        if usable(GetStdHandle(STD_OUTPUT_HANDLE)) && usable(GetStdHandle(STD_ERROR_HANDLE)) {
            return;
        }

        // 有父控制台就接上，并且**显式**再打开一次 CONOUT$/CONIN$：只调
        // `AttachConsole` 时，那些原本无效的标准句柄是否被系统补上，取决于
        // 它们当初是 NULL 还是 INVALID_HANDLE_VALUE —— 两种情况都真实存在。
        // 显式 SetStdHandle 一遍就不必赌。
        if AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
            let conout = CreateFileW(
                wide("CONOUT$").as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                0,
            );
            if usable(conout) {
                SetStdHandle(STD_OUTPUT_HANDLE, conout);
                SetStdHandle(STD_ERROR_HANDLE, conout);
            }
            let conin = CreateFileW(
                wide("CONIN$").as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                0,
            );
            if usable(conin) {
                SetStdHandle(STD_INPUT_HANDLE, conin);
            }
            return;
        }

        // 没有控制台：指向 NUL。这是"安全的失败"——写进去的内容消失，但写这个
        // 动作本身成功，于是 `println!` 不 panic、服务继续跑。
        let nul = CreateFileW(
            wide("NUL").as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            0,
        );
        if usable(nul) {
            SetStdHandle(STD_INPUT_HANDLE, nul);
            SetStdHandle(STD_OUTPUT_HANDLE, nul);
            SetStdHandle(STD_ERROR_HANDLE, nul);
        }
    }
}

#[cfg(not(windows))]
fn fix_stdio() {}

/// panic 落盘。
///
/// 除了写 `logs\crash.log`，还保留原 hook 的输出（有控制台时人照样看得见）。
/// 文件写不进去也继续：panic 的现场信息已经丢了一半，不能再因为写日志失败而
/// 掩盖原始位置。
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(root) = crate::run::root() {
            let dir = root.join("logs");
            let _ = std::fs::create_dir_all(&dir);
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("crash.log"))
            {
                // 时间戳复用看门狗那一份，两边的行首格式一致才便于对照。
                let _ = writeln!(file, "=== {} ===", crate::run::stamp_for_log());
                let _ = writeln!(file, "{info}");
                if let Some(location) = info.location() {
                    let _ = writeln!(file, "at {}:{}", location.file(), location.line());
                }
                let _ = writeln!(file);
            }
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// hook 装两次不该炸（测试里可能重复调用），也不该丢掉上一层的输出。
    #[test]
    fn installing_the_hook_twice_is_safe() {
        install_panic_hook();
        install_panic_hook();
    }
}
