//! 第 2 档：低级键盘钩子（`WH_KEYBOARD_LL`）。
//!
//! 注入端只过滤"浏览器上报的按键"。这一层拦的是**另外一条路**：被控机本地插上
//! 的键盘、别的程序直接调 `SendInput`、远程桌面自己的热键。它们都不经过我们的
//! 消息通道，只有系统级钩子看得见。
//!
//! ## 必须放行 `LLKHF_INJECTED`
//!
//! 这是整个模块最容易写错的一行。我们自己的游戏按键也是 `SendInput` 注入的，
//! 在低级钩子里同样会以"注入事件"的身份出现。如果把注入事件也判一遍危险组合，
//! 那么玩家按 `Alt`（放行）+ 按 `Tab`（`Alt+Tab` 判定成立）时，**注入端已经放行
//! 的 Tab 会被钩子再吃一次** —— 表现是"游戏里键盘时灵时不灵"，而且只在按过
//! 修饰键之后出现，极难复现。
//!
//! 所以：`INJECTED` 一律交给下一个钩子。这两层的分工是硬边界 ——
//! 注入的键由注入端判定，物理的键由钩子判定，谁也不重复判。
//!
//! ## 为什么 `Win` 的按下状态要自己记
//!
//! 钩子把 `Win` 的 key-down 吞掉之后，**系统层根本没收到过这个键**，于是
//! `GetAsyncKeyState(VK_LWIN)` 靠不住。这也正是要自己记的原因：`Win` 按住期间的
//! 后续按键（`Win+X` 的 `X`）必须继续吞，否则系统会把它当普通键送给游戏 —— 那
//! 还算好的，更糟的是系统可能在 `Win` 松开时补一次开始菜单。
//!
//! ## 线程模型
//!
//! 低级钩子**必须**在装了它的那个线程上有消息循环，否则回调永远不来。所以这里
//! 起一条专用线程，`SetWindowsHookExW` 与 `GetMessageW` 都在它上面；
//! [`stop`] 用 `PostThreadMessageW(WM_QUIT)` 让它自己退出并卸载钩子。

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// 钩子是否处于"锁定"状态。回调里第一件事就是读它。
static ARMED: AtomicBool = AtomicBool::new(false);
/// `Win` 是否正被按住（且按下事件已被我们吞掉）。
static WIN_HELD: AtomicBool = AtomicBool::new(false);
/// 装了钩子的线程 id，用于 `PostThreadMessageW(WM_QUIT)`。
static TID: AtomicU32 = AtomicU32::new(0);
/// 钩子句柄，`0` 表示没装上。
static HANDLE: AtomicIsize = AtomicIsize::new(0);

/// 钩子是否已装上。
pub fn installed() -> bool {
    HANDLE.load(Ordering::Relaxed) != 0
}

/// 切换锁定状态。装上钩子之后由 [`super::engage`] / [`super::disarm`] 调用。
pub fn set_armed(armed: bool) {
    if !armed {
        WIN_HELD.store(false, Ordering::Relaxed);
    }
    ARMED.store(armed, Ordering::Relaxed);
}

pub fn armed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

/// 装上钩子。失败时返回原因（比如被安全软件拦掉）。
pub fn start() -> Result<(), String> {
    if installed() {
        return Ok(());
    }
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("kiosk-hook".to_owned())
        .spawn(move || imp::run(tx))
        .map_err(|err| format!("起钩子线程失败：{err}"))?;
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result,
        Err(_) => Err("钩子线程 5 秒内没有报到（挂住了？）".to_owned()),
    }
}

/// 卸下钩子。幂等。
pub fn stop() {
    ARMED.store(false, Ordering::Relaxed);
    let tid = TID.swap(0, Ordering::Relaxed);
    if tid != 0 {
        imp::quit_thread(tid);
    }
    HANDLE.store(0, Ordering::Relaxed);
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::Sender;

    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, SetWindowsHookExW,
        UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN,
        WM_SYSKEYUP,
    };

    use super::{ARMED, HANDLE, TID, WIN_HELD};
    use crate::kiosk::guard::{hook_blocks, Mods};

    /// 另一个 hook 来源（UIPI 降权进程注入）的标记。
    ///
    /// 与 `LLKHF_INJECTED` 同样处理：那不是物理按键，交给下一层。
    const LLKHF_LOWER_IL_INJECTED: u32 = 0x0000_0002;

    fn pressed(vk: u16) -> bool {
        // 高位 0x8000 = 当前按下。低位（"自上次查询以来被按过"）在这里无意义。
        unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
    }

    pub(super) fn quit_thread(tid: u32) {
        unsafe {
            let _ = windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                tid,
                WM_QUIT,
                0,
                0,
            );
        }
    }

    pub(super) fn run(tx: Sender<Result<(), String>>) {
        unsafe {
            TID.store(
                windows_sys::Win32::System::Threading::GetCurrentThreadId(),
                Ordering::Relaxed,
            );
            let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(callback), 0, 0);
            if hook == 0 {
                let _ = tx.send(Err(format!(
                    "SetWindowsHookExW 失败（GetLastError={}）",
                    windows_sys::Win32::Foundation::GetLastError()
                )));
                TID.store(0, Ordering::Relaxed);
                return;
            }
            HANDLE.store(hook, Ordering::Relaxed);
            let _ = tx.send(Ok(()));
            log::info!("kiosk: 低级键盘钩子已装上");

            let mut msg: MSG = std::mem::zeroed();
            loop {
                let ret = GetMessageW(&mut msg, 0, 0, 0);
                if ret <= 0 {
                    break;
                }
            }
            let _ = UnhookWindowsHookEx(hook);
            log::info!("kiosk: 低级键盘钩子已卸载");
        }
    }

    unsafe extern "system" fn callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code < 0 || !ARMED.load(Ordering::Relaxed) {
            return CallNextHookEx(0, code, wparam, lparam);
        }
        let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
        // 见模块头：注入事件一律放行，否则会把自家注入的游戏按键吃掉。
        if kb.flags & LLKHF_INJECTED != 0 || kb.flags & LLKHF_LOWER_IL_INJECTED != 0 {
            return CallNextHookEx(0, code, wparam, lparam);
        }

        let msg = wparam as u32;
        let vk = kb.vkCode as u16;

        // `Win` 的按下/抬起由我们自己记账（系统收不到它）。
        if vk == VK_LWIN || vk == VK_RWIN {
            if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                WIN_HELD.store(true, Ordering::Relaxed);
            } else if msg == WM_KEYUP || msg == WM_SYSKEYUP {
                WIN_HELD.store(false, Ordering::Relaxed);
            }
            return 1;
        }

        let mods = Mods {
            alt: pressed(VK_MENU),
            ctrl: pressed(VK_CONTROL),
            shift: pressed(VK_SHIFT),
            win: false,
        };
        if hook_blocks(vk, mods, WIN_HELD.load(Ordering::Relaxed)) {
            log::debug!("kiosk: 钩子吞掉 vk=0x{vk:02x}");
            return 1;
        }
        CallNextHookEx(0, code, wparam, lparam)
    }
}

#[cfg(not(windows))]
mod imp {
    use std::sync::mpsc::Sender;

    pub(super) fn quit_thread(_tid: u32) {}

    pub(super) fn run(tx: Sender<Result<(), String>>) {
        let _ = tx.send(Err("键盘钩子只在 Windows 上可用".to_owned()));
    }
}
