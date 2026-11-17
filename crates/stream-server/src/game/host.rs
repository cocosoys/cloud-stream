//! 游戏进程的生命周期：起、停、看状态。
//!
//! ## 一条必须守住的规矩：停止不是 kill
//!
//! 这个项目的上一个游戏存档就是被 `taskkill /F` 杀坏的 —— 区块文件停在半写状态，
//! 下次进世界时 `Unexpected end of ZLIB input stream`。MC 在正常退出时会做两件
//! 事：存档落盘、把**内存里的** `options.txt` 整份写回去。强杀两件都不做。
//!
//! 所以 [`GameHost::stop`] 的顺序是：`WM_CLOSE` → 等 `stop_timeout_sec` → 才
//! `TerminateProcess`。宽限期默认 10 秒：一台 4 vCPU 的虚拟机上，从按下关闭到
//! 世界真正写完大约 2–5 秒，10 秒足够，又不会让"点了停止没反应"变成一个投诉。
//!
//! ## 为什么状态里要报窗口
//!
//! 因为这台机器的全屏有一个已知脾气：**一失去焦点就自己最小化**到
//! `-32000,-32000`。进程活着、日志正常、端口照开，只有屏幕上是空的。没有窗口
//! 这一栏，运维看到的永远是 `state=running`，然后怀疑是串流坏了。

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::Mutex;

use super::{DisplayModel, InstanceSpec, Paths};
use crate::config::Config;

/// 一次退出的记录。
#[derive(Debug, Clone, Serialize)]
pub struct Exit {
    /// 退出码。被 `TerminateProcess` 时是那个异常码（如 1），不是 `None`。
    pub code: Option<i32>,
    /// "exited"（自己退的）或 "killed"（宽限期到了被结束）。
    pub reason: &'static str,
    pub at_unix: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    pub hwnd: u64,
    pub class: String,
    pub width: i32,
    pub height: i32,
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    pub minimized: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub instance: String,
    pub name: String,
    /// `stopped` / `running`
    pub state: &'static str,
    pub pid: Option<u32>,
    pub uptime_secs: u64,
    pub started_at_unix: Option<u64>,
    pub last_exit: Option<Exit>,
    pub restarts: u32,
    pub display_model: &'static str,
    pub display_width: u32,
    pub display_height: u32,
    /// 存档目录下 `crash-reports\*.txt` 的数量。
    ///
    /// 只报数量不报内容：数量变化说明"刚刚崩过"，内容要人去读。把内容塞进 JSON
    /// 会让这个端点在崩溃后突然变成几 MB，而看它的人只是想确认"是不是又崩了"。
    pub crash_reports: usize,
    pub window: Option<WindowInfo>,
}

struct Inner {
    child: Option<Child>,
    started_at: Option<Instant>,
    last_exit: Option<Exit>,
    restarts: u32,
    /// 上一次启动算出来的窗口尺寸。`status()` 拿不到 `Config`（它只是个读接口），
    /// 所以把"启动时定下的尺寸"记在这里，而不是每次去猜一个默认值。
    size: (u32, u32),
}

/// 单实例的游戏托管。由 `app` 创建一次，HTTP 控制端点共享。
pub struct GameHost {
    paths: Paths,
    id: String,
    spec: InstanceSpec,
    inner: Mutex<Inner>,
}

impl GameHost {
    /// 载入实例。实例文件缺失/写错**不是**启动失败：串流本身还能用，运维要能
    /// 通过 `/api/v1/health` 看到"游戏配置坏了"，而不是面对一个起不来的服务。
    pub fn new(cfg: &Config) -> Result<Arc<Self>, String> {
        let paths = Paths::resolve(cfg)?;
        let id = cfg.game_instance.clone();
        let spec = InstanceSpec::load(&paths, &id)?;
        Ok(Arc::new(Self {
            paths,
            id,
            spec,
            inner: Mutex::new(Inner {
                child: None,
                started_at: None,
                last_exit: None,
                restarts: 0,
                size: (0, 0),
            }),
        }))
    }

    pub fn instance_id(&self) -> &str {
        &self.id
    }

    /// 起。
    ///
    /// 已经有一个活着的时候返回错误而不是再起一个：同一份 `saves` 上两个 MC
    /// 会同时写 `session.lock`，第二个的表现是"进游戏立刻被踢回主菜单"，而原因
    /// 藏在日志最后一行。
    pub async fn start(&self, cfg: &Config) -> Result<Status, String> {
        let mut guard = self.inner.lock().await;
        if let Some(child) = guard.child.as_mut() {
            if let Ok(None) = child.try_wait() {
                return Err(format!(
                    "游戏已经在跑（pid {}），先停止再启动",
                    child.id()
                ));
            }
            // 已经退出的旧进程先收尸，否则下面会以为还在跑。
            guard.child = None;
        }

        let saves = self.spec.saves_path(&self.paths);
        std::fs::create_dir_all(&saves)
            .map_err(|e| format!("建存档目录失败 {}：{e}", saves.display()))?;
        std::fs::create_dir_all(self.paths.runtime())
            .map_err(|e| format!("建 runtime 目录失败：{e}"))?;

        let plan = self.spec.plan(cfg, &self.paths)?;
        // 窗口形态先写进 options.txt。失败只警告：windowed 模式下缺这个文件
        // 游戏照样能起，把它变成"起不来"是过度反应。
        if let Err(err) = self.spec.write_options(cfg, &self.paths) {
            log::warn!("game: options.txt 未更新（继续启动）：{err}");
        }
        let _ = std::fs::write(self.paths.runtime().join("cmdline.txt"), plan.render());

        log::info!(
            "game: starting {} (pid pending) java={} cwd={} size={}x{} model={:?}",
            self.id,
            plan.java.display(),
            plan.cwd.display(),
            plan.size.0,
            plan.size.1,
            plan.model
        );
        if !plan.missing.is_empty() {
            log::warn!("game: classpath 缺 {} 项：{:?}", plan.missing.len(), plan.missing);
        }

        let child = Command::new(&plan.java)
            .args(&plan.args)
            .current_dir(&plan.cwd)
            // 子进程的 stdio 全部指向 NUL：游戏自己写 logs\latest.log，而父进程
            // 是 GUI 子系统，把句柄传下去只会多一条没人读的管道。
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("启动 javaw 失败（{}）：{e}", plan.java.display()))?;

        log::info!("game: started {} pid={}", self.id, child.id());
        guard.child = Some(child);
        guard.started_at = Some(Instant::now());
        guard.last_exit = None;
        guard.size = plan.size;
        drop(guard);
        Ok(self.status().await)
    }

    /// 停。先礼后兵，见模块文档。
    pub async fn stop(&self) -> Result<Status, String> {
        let (mut child, pid) = {
            let mut guard = self.inner.lock().await;
            match guard.child.take() {
                Some(child) => {
                    let pid = child.id();
                    (child, pid)
                }
                None => return Err("游戏没有在跑".to_owned()),
            }
        };

        let timeout = Duration::from_secs(self.spec.lifecycle.stop_timeout_sec.max(1));
        let hwnd = find_game_window(pid);
        let mut polite = false;
        if let Some(hwnd) = hwnd {
            polite = post_close(hwnd);
        }
        log::info!(
            "game: stop requested pid={} window={:?} wm_close={} grace={}s",
            pid,
            hwnd.map(|h| format!("0x{:x}", h)),
            polite,
            timeout.as_secs()
        );

        // 等它自己退，但**窗口一没就不等了**。
        //
        // 2026-11-17 实测（.128，MC 1.12.2 + LWJGL2）：给 LWJGL 窗口发 WM_CLOSE 的
        // 结果是"窗口销毁、javaw 不退出"——进程停在 71 MB，既没有画面也没有日志，
        // 先 SetForegroundWindow 也一样。也就是说这台机器上的 MC **没有**可用的
        // 外部优雅退出路径，早先那次"15 秒后强杀"其实是在等一件不会发生的事。
        //
        // 于是这里的判据从"等够时间"改成"还有没有意义等"：窗口消失后再给 1 秒
        // 收尾（它可能正在写 session.lock），然后结束。这样停止按钮的反馈从
        // 16 秒变成 2 秒，而且不会把一个没有窗口的进程留在 tasks 里。
        let mut waited = Duration::ZERO;
        let mut exit = None;
        let mut window_lost = false;
        let mut lost_at: Option<Duration> = None;
        while waited < timeout {
            match child.try_wait() {
                Ok(Some(status)) => {
                    exit = Some(Exit {
                        code: status.code(),
                        reason: "exited",
                        at_unix: now_unix(),
                    });
                    break;
                }
                Ok(None) => {}
                Err(err) => {
                    log::warn!("game: try_wait 失败：{err}");
                    break;
                }
            }
            if let Some(h) = hwnd {
                if !window_alive(h) {
                    window_lost = true;
                    match lost_at {
                        None => lost_at = Some(waited),
                        Some(t) if waited.saturating_sub(t) >= Duration::from_secs(1) => break,
                        _ => {}
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            waited += Duration::from_millis(200);
        }

        if exit.is_none() {
            let reason = if window_lost { "killed-window-gone" } else { "killed" };
            log::warn!(
                "game: 结束 pid={pid}（reason={reason}, waited={:?}, window_lost={window_lost}）",
                waited
            );
            let _ = child.kill();
            let status = child.wait().ok();
            exit = Some(Exit {
                code: status.and_then(|s| s.code()),
                reason,
                at_unix: now_unix(),
            });
        }

        let mut guard = self.inner.lock().await;
        guard.last_exit = exit;
        guard.started_at = None;
        drop(guard);
        log::info!("game: stopped {pid}");
        Ok(self.status().await)
    }

    /// 重启 = 停（如果在跑）+ 起。
    pub async fn restart(&self, cfg: &Config) -> Result<Status, String> {
        if self.is_running().await {
            self.stop().await?;
        }
        self.start(cfg).await
    }

    async fn is_running(&self) -> bool {
        let mut guard = self.inner.lock().await;
        match guard.child.as_mut() {
            Some(child) => !matches!(child.try_wait(), Ok(Some(_))),
            None => false,
        }
    }

    /// 当前状态。会顺手收尸：一个已经退出的子进程在这里被发现并记进 `last_exit`，
    /// 而不是永远留在 `child` 里让 `state` 一直报 `running`。
    pub async fn status(&self) -> Status {
        let mut guard = self.inner.lock().await;

        if let Some(child) = guard.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                guard.last_exit = Some(Exit {
                    code: status.code(),
                    reason: "exited",
                    at_unix: now_unix(),
                });
                guard.child = None;
                guard.started_at = None;
            }
        }

        let pid = guard.child.as_ref().map(|c| c.id());
        let uptime_secs = guard.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let started_at_unix = guard
            .started_at
            .map(|t| now_unix().saturating_sub(t.elapsed().as_secs()));
        let state = if pid.is_some() { "running" } else { "stopped" };
        let last_exit = guard.last_exit.clone();
        let restarts = guard.restarts;
        let size = guard.size;
        drop(guard);

        Status {
            instance: self.id.clone(),
            name: if self.spec.name.is_empty() {
                self.id.clone()
            } else {
                self.spec.name.clone()
            },
            state,
            pid,
            uptime_secs,
            started_at_unix,
            last_exit,
            restarts,
            display_model: match self.spec.display.model {
                DisplayModel::Exclusive => "exclusive",
                DisplayModel::Windowed => "windowed",
            },
            display_width: size.0,
            display_height: size.1,
            crash_reports: count_crash_reports(&self.spec.saves_path(&self.paths)),
            window: pid.and_then(describe_window),
        }
    }

}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `crash-reports` 里有多少份报告。目录不存在 = 0（不是错误：新装的世界还没有）。
fn count_crash_reports(saves: &std::path::Path) -> usize {
    let dir = saves.join("crash-reports");
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .ends_with(".txt")
            })
            .count(),
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------- Win32 窗口

#[cfg(windows)]
mod win {
    use super::WindowInfo;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindow,
        IsWindowVisible, PostMessageW, WM_CLOSE,
    };

    struct FindCtx {
        pid: u32,
        found: HWND,
    }

    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> i32 {
        let ctx = &mut *(lparam as *mut FindCtx);
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != ctx.pid {
            return 1;
        }
        let mut buf = [0u16; 64];
        let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if n <= 0 {
            return 1;
        }
        let class = String::from_utf16_lossy(&buf[..n as usize]);
        // 只认 LWJGL：同一个进程还有 D3DFocusWindow / SunAwtToolkit / IME 几个
        // 辅助窗口，对它们发 WM_CLOSE 不会让游戏退出，对 IME 发还可能弹出输入法。
        if class == "LWJGL" {
            ctx.found = hwnd;
            return 0;
        }
        1
    }

    pub fn find(pid: u32) -> Option<HWND> {
        let mut ctx = FindCtx { pid, found: 0 };
        unsafe {
            EnumWindows(Some(enum_cb), &mut ctx as *mut FindCtx as LPARAM);
        }
        if ctx.found == 0 {
            None
        } else {
            Some(ctx.found)
        }
    }

    pub fn describe(pid: u32) -> Option<WindowInfo> {
        let hwnd = find(pid)?;
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let ok = unsafe { GetWindowRect(hwnd, &mut rect) };
        if ok == 0 {
            return None;
        }
        Some(WindowInfo {
            hwnd: hwnd as u64,
            class: "LWJGL".to_owned(),
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
            x: rect.left,
            y: rect.top,
            visible: unsafe { IsWindowVisible(hwnd) } != 0,
            minimized: unsafe { IsIconic(hwnd) } != 0,
        })
    }

    pub fn post_close(hwnd: HWND) -> bool {
        unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) != 0 }
    }

    /// 窗口还存不存在。停止逻辑用它判"还有没有等下去的意义"。
    pub fn alive(hwnd: HWND) -> bool {
        unsafe { IsWindow(hwnd) != 0 }
    }
}

#[cfg(windows)]
fn find_game_window(pid: u32) -> Option<windows_sys::Win32::Foundation::HWND> {
    win::find(pid)
}

#[cfg(windows)]
fn describe_window(pid: u32) -> Option<WindowInfo> {
    win::describe(pid)
}

#[cfg(windows)]
fn post_close(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
    win::post_close(hwnd)
}

#[cfg(windows)]
fn window_alive(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
    win::alive(hwnd)
}

// 非 Windows 上编译通过、行为降级：没有窗口可找，停止只能靠超时强杀。
#[cfg(not(windows))]
fn find_game_window(_pid: u32) -> Option<u64> {
    None
}

#[cfg(not(windows))]
fn describe_window(_pid: u32) -> Option<WindowInfo> {
    None
}

#[cfg(not(windows))]
fn post_close(_hwnd: u64) -> bool {
    false
}

#[cfg(not(windows))]
fn window_alive(_hwnd: u64) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_report_count_is_zero_for_missing_dir() {
        let dir = std::env::temp_dir().join("cloud-stream-game-test-no-such-dir");
        assert_eq!(count_crash_reports(&dir), 0);
    }

    #[test]
    fn crash_report_count_ignores_non_txt() {
        let dir = std::env::temp_dir().join("cloud-stream-game-test-crash");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("crash-reports")).unwrap();
        std::fs::write(dir.join("crash-reports").join("a.txt"), "x").unwrap();
        std::fs::write(dir.join("crash-reports").join("b.log"), "x").unwrap();
        assert_eq!(count_crash_reports(&dir), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
