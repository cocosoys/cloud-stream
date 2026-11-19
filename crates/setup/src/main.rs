//! stream-server 的工具箱：看状态、改配置、换版本、卸掉。
//!
//! 四动作差距矩阵里，「可视化界面」这一列原本四格全红 —— 安装和卸载只有 cmd
//! 黑框、更新要开发机、修改是文本编辑器。这个 exe 是那一列的残余部分：装和卸
//! 已经归安装器（Inno / MSI），剩下的"看、改、换、卸的入口"归它。
//!
//! # 它是什么，不是什么
//!
//! 它**不是**第二个实现。窗口上的每个按钮最后都落到同一条命令上：
//!
//! ```text
//! 刷新状态   ->  自己查进程/端口/任务 + stream-server.exe --ops state
//! 开始/停止  ->  schtasks /Change|/Run|/End（见 service.rs）
//! 更新       ->  stream-server.exe --update check|apply
//! 卸载       ->  运行根里的 unins000.exe（卸载流程在卸载器里，不在这儿）
//! 改配置     ->  stream-server.exe --ops config-get|config-set
//! ```
//!
//! 所以窗口只是这些命令的一个消费者，同一套动作在命令行里也能做，反之亦然 ——
//! 不存在"界面上能点、命令行里做不到"这种只能靠人记住的差别。
//!
//! # 为什么卸不下来时补一句"和它放同一目录"
//!
//! 分发出去的那一份（带 `--features embed` 构建的）里嵌着整个运行包，双击就该
//! 能装。判断依据不是"我是不是被双击了" —— 那个问题没有答案，而是**我旁边有
//! 没有 `stream-server.exe`**：
//!
//! * 旁边有 → 装上之后的那一份（或者开发机上的整包目录），当工具箱；
//! * 旁边没有但有内置包 → 分发出去的那一份，当安装器。
//!
//! 归档里刻意没有这个 exe 自己（否则就成了"exe 里嵌着包含自己的包"）：安装器
//! 解压完把自己复制进目标目录，从那以后它就是工具箱。见 `pack::place_self`。
//!
//! # 为什么不用 Tauri / egui
//!
//! Tauri 会拖进一个运行时和四百个 crate，而这个窗口要画的东西是四个按钮和两块
//! 文本。`windows-sys` 已经在依赖树里（服务端用它做采集与输入），所以这里的
//! 增量成本是零个新依赖、几秒构建。代价是界面语言是 Win32 而不是 CSS —— 对这
//! 个尺寸的界面，这笔账划算。
//!
//! # 提权的动作怎么把结果说出来
//!
//! 安装的第二段、卸载的整段都需要管理员：`runas` 拉起的是另一个进程（往往
//! 还是另一个账户），它的输出本窗口抓不到 —— 与其假装抓到了再显示一半，不如
//! 走一个两边都能读的地方：`%ProgramData%\cloud-stream\last-install.txt`
//! （固定路径，提权换账户也是同一个字符串）。本窗口在拉起之后盯着那个文件，
//! 出现 `=== RESULT:` 就把整段贴进日志。
//!
//! 安装和卸载写的是同一个文件、同一套标记，所以本窗口的"盯文件"逻辑一行都
//! 不用分情况 —— 换掉实现，不换掉契约。
//!
//! 卸载还多一件事：**它会把本窗口自己也删掉**（本 exe 就在那个目录里）。所以
//! 点完卸载，本窗口必须真的退出；负责删目录的替身进程正等着它退。
//!
//! # 改配置为什么是表单而不是记事本
//!
//! 记事本那条路的失败代价太高：拼错一个键名，服务下次启动才失败，而那一刻
//! 人已经离开了。表单把这一步挪到"保存"时：值域、枚举、布尔在
//! `--ops config-set` 里校验，写之前自动留底（`--ops rollback` 能退回），写
//! 完用启动时同一套解析器复查。界面自己不做判断，也不碰文件。
//!
//! 带 `--config` 启动会直接把表单打开（给快捷方式用）。
//!
//! # 为什么是 `windows_subsystem = "windows"`
//!
//! 这是个纯窗口程序：它没有一行 `println!`，所有反馈都在窗口和日志面板里。
//! 挂 console 子系统的代价却是实打实的 —— 双击之后先弹一个黑框陪到最后，正是
//! "用户感知"这个词最字面的意思。改成 GUI 子系统之后，双击就是只有窗口。
//!
//! `stdio_guard` 是配套的：GUI 进程的标准句柄默认无效，将来谁在这里加一句
//! `println!` 调试，就会得到一个"窗口打开后立刻消失"的幽灵 bug（写失败 →
//! panic → 进程没了）。把它指到 NUL 之后，那种写法至少不会炸。

#![cfg_attr(windows, windows_subsystem = "windows")]

// 安装器的四条腿：`pack` 读内嵌包并铺到目标目录，`install` 走安装的两段
// 流程，`uninstall` 走卸载的两段（主体 + 替身删目录），`report` 把经过写进
// 那个"提权也能读"的结论文件，`sys` 是安装与卸载共用的那几个 Win32 调用。
// 窗口与它们是同一个 exe 的两个角色 —— 谁上场由 `decide_mode` 说了算。
mod install;
mod pack;
mod report;
mod service;
mod sys;
mod uninstall;

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateFontW, DeleteObject, InvalidateRect};
use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDlgItem, GetMessageW,
    GetWindowTextW, LoadIconW,
    LoadCursorW, MessageBoxW, MoveWindow, PostMessageW, PostQuitMessage, RegisterClassW,
    SendMessageW, SetForegroundWindow, SetWindowTextW, SetWindowPos, ShowWindow, TranslateMessage,
    BS_PUSHBUTTON, BS_AUTOCHECKBOX, CW_USEDEFAULT, ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_MULTILINE, ES_PASSWORD,
    ES_READONLY,
    MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_YESNO, MSG, SWP_NOACTIVATE, SWP_NOZORDER,
    SW_SHOWNORMAL, WM_APP, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_DPICHANGED, WM_SETFONT,
    WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_MINIMIZEBOX, WS_SYSMENU, WS_VISIBLE, WS_VSCROLL,
};

// 控件 id。数字直接写死：这个窗口的生命周期里只会有这一套控件。
const ID_STATE: i32 = 1100;
const ID_LOG: i32 = 1101;
const ID_LABEL_STATE: i32 = 1102;
const ID_LABEL_LOG: i32 = 1103;
const BTN_REFRESH: i32 = 1001;
const BTN_UNINSTALL: i32 = 1005;
const BTN_CONFIG: i32 = 1006;
const BTN_UPDATE: i32 = 1007;
/// 开 / 停。文字跟着状态翻（见 `service::power_label`），所以它是唯一一个
/// 标签在运行期会变的按钮。
const BTN_POWER: i32 = 1008;

// 配置表单（另一个窗口）的控件 id。
const BTN_CFG_SAVE: i32 = 1200;
const BTN_CFG_RELOAD: i32 = 1201;
const BTN_CFG_RAW: i32 = 1202;
/// 第 i 个配置格的控件 id = `FIELD_BASE + i`。主窗口与表单窗口的 id 空间是
/// 分开的（两个窗口类各自的 `WM_COMMAND`），所以不会和上面那几个撞。
const FIELD_BASE: i32 = 1300;

// 组合框消息：下拉列表的样式与两条消息。`windows-sys` 把这几个放在
// `Win32_UI_Controls` 里，而为三个常量开一个 feature 不值当。
/// 下拉列表：只能选，不能手打。
const CBS_DROPDOWNLIST: u32 = 0x0003;
const CB_ADDSTRING: u32 = 0x0143;
const CB_SETCURSEL: u32 = 0x014E;

/// 后台线程干完活之后通知 UI 线程来取结果。
const WM_APP_DONE: u32 = WM_APP + 1;

/// 日志面板的全部文本。用 `SetWindowTextW` 整块替换而不是逐行插入：
/// 日志最多几十行，整块替换省掉 EM_SETSEL / EM_REPLACESEL 两套光标算术。
static LOG_TEXT: Mutex<String> = Mutex::new(String::new());

/// 状态面板的文本（`--ops state` 的原样输出）。
static STATE_TEXT: Mutex<String> = Mutex::new(String::new());

unsafe fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// 缩放：Per-Monitor DPI
// ---------------------------------------------------------------------------
//
// 150% 的屏上看到的不该是"被拉大的位图"。三步，缺一不可：
//
// 1. 清单里声明 PerMonitorV2（`stream-setting.manifest` + `build.rs`）—— 不声明
//    的话系统在进程启动那一刻就把窗口当位图放大，代码里做什么都晚了。
// 2. 所有尺寸从 96 DPI 的基准值按窗口当前 DPI 换算，字体也按 DPI 重建。
// 3. 窗口被拖到另一块不同缩放的屏上时系统会发 `WM_DPICHANGED`：按它给的矩形
//    摆好窗口，再按新 DPI 重排一遍控件。只改窗口不重排，控件挤在左上角；
//    只重排不改窗口，控件溢出到框外。

/// 96 DPI 下的主窗口布局：`(控件 id, x, y, 宽, 高)`。
///
/// 写成表而不是散在 `WM_CREATE` 里，是因为 `WM_DPICHANGED` 要按新 DPI 再排
/// 一遍；散着写就得写两份，而两份坐标一定会分叉。
const MAIN_LAYOUT: &[(i32, i32, i32, i32, i32)] = &[
    (ID_LABEL_STATE, 16, 12, 200, 20),
    // 170 高：顶上那几行是运行状态（服务/端口/看门狗），下面接 `--ops state`
    // 的原文。只留 130 时状态区会把原文挤没，人还是得滚。
    (ID_STATE, 16, 34, 748, 168),
    // 五个按钮等宽排一行，铺满状态框的宽度（5×140 + 4×12 = 748）：中文标签
    // 字数接近，等宽比"按字数伸缩"耐看，也不会因为某个标签多两个字就挤歪整排。
    // 顺序即使用频次：看 → 开停 → 换版本 → 卸掉 → 改。
    //
    // 「开始/停止」排在第二格而不是紧挨着「卸载」：它和「刷新」同属"看这台
    // 机器现在怎么样"，而和卸载放在一起迟早有人点错。
    (BTN_REFRESH, 16, 214, 140, 30),
    (BTN_POWER, 168, 214, 140, 30),
    (BTN_UPDATE, 320, 214, 140, 30),
    (BTN_UNINSTALL, 472, 214, 140, 30),
    (BTN_CONFIG, 624, 214, 140, 30),
    (ID_LABEL_LOG, 16, 258, 200, 20),
    (ID_LOG, 16, 280, 748, 210),
];

/// 主窗口的基准尺寸（96 DPI，含边框）。窗口按当前 DPI 等比放大。
const MAIN_WINDOW: (i32, i32) = (796, 522);

/// 一个 96 DPI 下的长度换成像素。
///
/// 整数算、四舍五入：1.25 倍时 100 该是 125 而不是 124（截断会一路攒成可见
/// 的错位）。`+ 48` 就是 `+ half of 96`。
fn scaled(value: i32, dpi: u32) -> i32 {
    (value * dpi as i32 + 48) / 96
}

/// 窗口现在所在那块屏的 DPI。取不到就按 96（100%）—— 老系统上会略小，
/// 但不会画错。
fn window_dpi(hwnd: HWND) -> u32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 {
        96
    } else {
        dpi
    }
}

/// 当前 DPI 对应的界面字体。
///
/// 缓存一份：DPI 变了才重建，旧的 `DeleteObject` 掉 —— GDI 对象不删就漏，
/// 而人把窗口在两块屏之间拖来拖去会反复重排。
static UI_FONT: Mutex<(u32, isize)> = Mutex::new((0, 0));

fn ui_font(dpi: u32) -> isize {
    let mut slot = UI_FONT.lock().unwrap_or_else(|e| e.into_inner());
    if slot.0 == dpi && slot.1 != 0 {
        return slot.1;
    }
    unsafe {
        if slot.1 != 0 {
            DeleteObject(slot.1);
        }
        let font = CreateFontW(
            -scaled(16, dpi),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            1,
            0,
            0,
            0,
            0,
            wide("Segoe UI").as_ptr(),
        );
        *slot = (dpi, font);
        font
    }
}

/// 按当前 DPI 把主窗口的控件摆一遍（`WM_CREATE` 与 `WM_DPICHANGED` 共用）。
fn apply_main_layout(hwnd: HWND) {
    let dpi = window_dpi(hwnd);
    let font = ui_font(dpi);
    for &(id, x, y, w, h) in MAIN_LAYOUT {
        let child = unsafe { GetDlgItem(hwnd, id) };
        if child == 0 {
            continue;
        }
        unsafe {
            MoveWindow(
                child,
                scaled(x, dpi),
                scaled(y, dpi),
                scaled(w, dpi),
                scaled(h, dpi),
                1,
            );
            SendMessageW(child, WM_SETFONT, font as usize, 1);
        }
    }
}

/// 运行根：本 exe 所在目录。和 `ops::run_root` 同一口径 —— 不取当前工作目录，
/// 计划任务拉起进程时那是 `System32`。
pub(crate) fn run_root() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// 这次当谁
// ---------------------------------------------------------------------------

/// 同一个 exe 的两个角色。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// 窗口：装好之后的那一份。
    Console,
    /// 安装器：分发出去的那一份，自带有整个运行包。
    Installer,
}

/// 让这一轮"显式指定了角色"的开关。
///
/// 抽成常量是因为漏掉一个的后果不是"少个功能"，而是**开一个没人能关的窗口**：
/// 装好之后的 exe 旁边有 `stream-server.exe`，判据会把它认成控制台。
/// `--uninstall-step` 就是这样漏过一次的（见 `main` 里那一段），所以它同时
/// 在 `main` 里被提前截住 —— 两处都在，是因为"判据不能改"和"开关可能还会加"。
const CLI_SWITCHES: &[&str] = &[
    "--install",
    "--finish",
    "--uninstall",
    "--purge",
    "--uninstall-step",
    "--inbound",
    "--service",
];

/// 这一轮启动该做什么。
fn decide_mode(args: &[String]) -> Mode {
    // 卸载的几段也算"显式指定了角色"：`--purge` 尤其重要 —— 替身被复制到
    // `%TEMP%` 里运行，那儿没有 `stream-server.exe`，而它自带有内置包，
    // 按下面的判据会被认成"安装器"。判据本身不能改（它是对的），所以把
    // "我是谁"这件事由参数说清楚。
    let explicit = args.iter().any(|a| CLI_SWITCHES.contains(&a.as_str()));
    classify(
        explicit,
        pack::is_embedded(),
        run_root().join("stream-server.exe").exists(),
    )
}

/// 判据拆成三个布尔，是为了让"四种组合各是什么"能被测试钉住。
///
/// 关键的一条是**不按"我是不是被双击了"判断**：那个问题在进程里没有答案。
/// 有答案的是"我旁边有没有 `stream-server.exe`" —— 装好的那一份一定有，
/// 分发出去的那一份一定没有（包里的 exe 是解压完才复制进去的）。
fn classify(explicit_install: bool, has_payload: bool, server_beside: bool) -> Mode {
    if explicit_install {
        return Mode::Installer;
    }
    if has_payload && !server_beside {
        return Mode::Installer;
    }
    Mode::Console
}

/// 追加一行日志并刷新面板。
fn log(line: &str, hwnd: HWND) {
    {
        let mut text = LOG_TEXT.lock().unwrap_or_else(|e| e.into_inner());
        text.push_str(line);
        text.push_str("\r\n");
    }
    unsafe {
        let text = LOG_TEXT.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let w = wide(&text);
        SetWindowTextW(GetDlgItem(hwnd, ID_LOG), w.as_ptr());
    }
}

fn set_state(text: &str, hwnd: HWND) {
    // Win32 的 EDIT 控件不把裸 `\n` 当换行，只认 `\r\n`；而 `--ops state` 走
    // Rust 的 println!，输出的是裸 `\n`。不归一化就能看到"七行挤成一行"——
    // 第一次跑起来就是这样。
    let normalized = text.replace("\r\n", "\n").replace('\n', "\r\n");
    {
        let mut slot = STATE_TEXT.lock().unwrap_or_else(|e| e.into_inner());
        *slot = normalized.clone();
    }
    unsafe {
        let w = wide(&normalized);
        SetWindowTextW(GetDlgItem(hwnd, ID_STATE), w.as_ptr());
    }
}

/// 按钮文字跟着状态翻：在跑写"停止服务"，没跑写"开始服务"。
///
/// 文案只有一处来源（`service::power_label`），判断也只有一个（`running`）——
/// 于是"标签说停止、点下去却是启动"这种自相矛盾不可能出现。
fn update_power_label(hwnd: HWND, running: bool) {
    unsafe {
        let text = wide(service::power_label(running));
        SetWindowTextW(GetDlgItem(hwnd, BTN_POWER), text.as_ptr());
    }
}

/// 跑一个子进程并把它说的话原样拿回来（stdout + stderr）。
///
/// `CREATE_NO_WINDOW`（0x08000000）是必需的：不带它，每次刷新状态都会闪一下
/// 黑框，而"点一下按钮闪三个黑框"是这类工具最先被骂的地方。
fn run_capture(program: &std::path::Path, args: &[&str]) -> (i32, String) {
    const CREATE_NO_WINDOW: u32 = sys::CREATE_NO_WINDOW;
    let output = std::process::Command::new(program)
        .args(args)
        .current_dir(run_root())
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match output {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr).into_owned();
            if !err.trim().is_empty() {
                text.push_str(&err);
            }
            (out.status.code().unwrap_or(-1), text)
        }
        Err(err) => (-1, format!("起不来 {}: {err}", program.display())),
    }
}

/// 在后台线程跑一段活，完成后让 UI 线程去刷新。
///
/// 所有动作都走这里：`install.cmd` 要等 60 秒探活，同步执行会让窗口"未响应"，
/// 而一个未响应的窗口看起来就是崩了。
fn spawn_job<F>(hwnd: HWND, job: F)
where
    F: FnOnce() -> Vec<String> + Send + 'static,
{
    log("… 正在执行，结果会追加在下面", hwnd);
    let target = hwnd as isize;
    std::thread::spawn(move || {
        let lines = job();
        let hwnd = target as HWND;
        for line in lines {
            log(&line, hwnd);
        }
        unsafe {
            PostMessageW(hwnd, WM_APP_DONE, 0, 0);
        }
    });
}

/// 「程序信息」面板最上面那几行：**这台机器现在跑没跑**。
///
/// 为什么不用 `--ops state` 一次问完：那份输出回答的是"配置和快照是什么样"，
/// 而"服务在不在跑"是窗口自己就能查的三件事（进程、端口、计划任务）。让面板
/// 先显示这三件，下面再附 `--ops state` 的原文 —— 人打开窗口第一眼要的通常是
/// 前者，而它是**不需要**服务端配合就能给出来的（服务没起来的时候，
/// `--ops state` 也可能因为配置坏了而失败）。
///
/// 进程表由调用方传进来（`service::pids`）：同一个刷新里，「服务在不在跑」
/// 还要用来决定那个按钮写"开始"还是"停止"，两处各查一遍就成了两次进程快照，
/// 而它们之间还有那么一小会儿可以说出互相矛盾的话。
fn program_info(root: &Path, pids: &[u32]) -> String {
    let exe = root.join("stream-server.exe");
    let mut out = String::new();

    // 进程按**完整路径**数，不能按映像名：同一台机器上可能还有另一个目录的安装，
    // 任务管理器里两个 stream-server.exe 长得一模一样。
    out.push_str(&match pids.len() {
        0 => "服务      未运行\n".to_owned(),
        1 => format!("服务      运行中（pid {}）\n", pids[0]),
        n => format!(
            "服务      运行中（{n} 个：{}）\n",
            pids.iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    });

    // 端口读配置，不假设 8080 —— `cloud-stream 9000` 那种改过端口的机器上，
    // 写死 8080 会显示"没有应答"，而服务其实好好的。
    let port = if exe.exists() {
        install::effective_config(&exe).port
    } else {
        8080
    };
    let listening = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_millis(400),
    )
    .is_ok();
    out.push_str(&format!(
        "端口      {port} · {}\n",
        if listening { "正在监听" } else { "没有应答" }
    ));

    let task = match uninstall::task_state(install::TASK_NAME) {
        uninstall::TaskState::Absent => "未注册",
        uninstall::TaskState::Enabled => "已注册",
        uninstall::TaskState::Disabled => "已注册，但被禁用（看门狗不会拉起服务）",
        uninstall::TaskState::Unknown => "在，但读不出内容",
    };
    out.push_str(&format!("看门狗    {} · {task}\n", install::TASK_NAME));

    out.push_str(&format!("目录      {}\n", root.display()));
    out
}

/// 确认框。返回 true = 继续。
fn confirm(hwnd: HWND, text: &str, caption: &str) -> bool {
    unsafe {
        let t = wide(text);
        let c = wide(caption);
        MessageBoxW(hwnd, t.as_ptr(), c.as_ptr(), MB_YESNO | MB_ICONWARNING) == 6 // IDYES
    }
}

fn notice(hwnd: HWND, text: &str, caption: &str) {
    unsafe {
        let t = wide(text);
        let c = wide(caption);
        MessageBoxW(hwnd, t.as_ptr(), c.as_ptr(), MB_OK | MB_ICONINFORMATION);
    }
}

// ---------------------------------------------------------------------------
// 配置表单
// ---------------------------------------------------------------------------
//
// 「修改」这一行的最后一格原本是"用记事本改 TOML，改坏了下次启动才知道"。
// 这个窗口把它换成表单：格子由服务端的类型表决定（整数范围、枚举选项、布尔），
// 值合不合法由 `configedit` 判断，写文件走 `config-set`（先留底、原子写、改完
// 用启动时同一套解析器复查）。
//
// 三条边界刻意守着：
// * 界面**不**判断值合不合法 —— 判断只有一份，在 `configedit` 里；
// * 界面**不**写文件 —— 写只有一条路，`--ops config-set`；
// * 界面只做两件事：把现在的值摆出来，把改过的值送过去。

/// 表单里的一格。
struct Field {
    name: String,
    brief: String,
    /// 文件里原来的值，原样（枚举带引号，如 `'vp9'`）。`None` = 没写这一行。
    original: Option<String>,
    /// 下拉框的选项。空 = 用文本框。
    options: Vec<String>,
    /// 文本框要不要打码（`ES_PASSWORD`）。只有口令那一项是 true。
    secret: bool,
    /// 输入控件（文本框或下拉框）。
    hwnd: HWND,
    /// 上面那行名字。也要跟着 DPI 重排，所以句柄得留着。
    label: HWND,
}

/// 每格的节拍与宽度：标签 16 + 间隔 2 + 控件 24 + 行距 4。
const FIELD_PITCH: i32 = 46;
const FIELD_W: i32 = 320;
/// 两列的左边界。
const COL_X: [i32; 2] = [16, 360];

/// 表单底部三个按钮：`(文字, 控件 id, 96 DPI 下的 x, 宽)`。
const CFG_BUTTONS: &[(&str, i32, i32, i32)] = &[
    ("保存", BTN_CFG_SAVE, 16, 100),
    ("重新读", BTN_CFG_RELOAD, 124, 100),
    ("打开原文", BTN_CFG_RAW, 232, 100),
];

/// 「显示口令」复选框。原来的口令格是打码的（这块屏幕常被别人远程看着），
/// 但首次安装时管理员要把这串口令抄到 MC 服务端/另一台机器上，所以得有一个
/// **临时**揭开的手段：勾上就明文，关掉窗口再打开又回到打码。
const ID_SHOW_SECRET: i32 = 1020;
/// Win32 里 `EM_SETPASSWORDCHAR`（windows-sys 没导出，值见 winuser.h）。
const EM_SETPASSWORDCHAR: u32 = 0x00CC;
/// `BM_GETCHECK`：返回复选框当前状态。
const BM_GETCHECK: u32 = 0x00F0;
/// `BST_CHECKED`。
const BST_CHECKED: isize = 1;

static FIELDS: Mutex<Vec<Field>> = Mutex::new(Vec::new());

/// 表单窗口。0 = 还没建过。关闭时只隐藏不销毁：重建十八个控件比 `ShowWindow`
/// 慢得多，而且人下次打开时改了一半的内容还在，不会白改。
static CONFIG_HWND: Mutex<isize> = Mutex::new(0);

/// 主窗口句柄。表单要把结果写进主窗口的日志面板，而 Win32 的窗口过程拿不到
/// "我是谁打开的"。存一份比 `GetWindow(hwnd, GW_OWNER)` 稳妥。
static MAIN_HWND: Mutex<isize> = Mutex::new(0);

/// 去掉一层外层引号，并把双引号字符串里的转义还原：
/// `'vp9'` → `vp9`，`'D:\logs\x.log'` → 原样，`"p\"ss\\w"` → `p"ss\w`。
///
/// 表单里显示的是"值"，不是"TOML 语法"。引号由 `configedit::format_value`
/// 在写回时按类型重新加，所以这里剥掉不会丢信息。
///
/// 两种引号的处理**必须不同**：单引号字符串（literal）在 TOML 里没有转义，
/// 里面的反斜杠是字面量（路径就靠这个）；双引号字符串（basic）有转义，
/// 不还原的话口令里一个 `\` 会变成两个，而界面上看不出来 —— 现象是"我明明
/// 设的是这个口令，怎么进不去"。这两条规则与 `quote_basic` 对称。
fn plain(raw: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    let text = raw.trim();
    if text.len() >= 2 {
        let bytes = text.as_bytes();
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if first == b'\'' && last == b'\'' {
            return Cow::Borrowed(&text[1..text.len() - 1]);
        }
        if first == b'"' && last == b'"' {
            let inner = &text[1..text.len() - 1];
            if !inner.contains('\\') {
                return Cow::Borrowed(inner);
            }
            let mut out = String::with_capacity(inner.len());
            let mut chars = inner.chars();
            while let Some(ch) = chars.next() {
                if ch != '\\' {
                    out.push(ch);
                    continue;
                }
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(other) => out.push(other),
                    None => out.push('\\'),
                }
            }
            return Cow::Owned(out);
        }
    }
    Cow::Borrowed(text)
}

/// 读一个控件的文本。下拉框的选中项也走这一条 —— 取值方式只有一种，
/// 省掉复选框那种"读的是勾选状态、不是文本"的分支。
fn control_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

/// 问服务端"这份配置现在是什么样"。
///
/// 走 `--ops config-get --ops-json` 而不是自己读 TOML：键名、范围、枚举值都
/// 只有服务端那份类型表说了算，界面抄一份就是第二套判断。
fn read_config_fields(exe: &std::path::Path) -> Result<Vec<Field>, String> {
    let (code, text) = run_capture(exe, &["--ops", "config-get", "--ops-json"]);
    if code != 0 {
        return Err(format!("[exit {code}] 读配置失败：\n{text}"));
    }
    let doc: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("输出不是 JSON（{e}）：\n{text}"))?;
    let keys = doc["keys"]
        .as_array()
        .ok_or_else(|| format!("输出里没有 keys：\n{text}"))?;
    let mut fields = Vec::with_capacity(keys.len());
    for key in keys {
        let name = key["name"].as_str().unwrap_or_default().to_owned();
        if name.is_empty() {
            continue;
        }
        let brief = match key["brief"].as_str() {
            Some(b) if !b.is_empty() => b.to_owned(),
            _ => name.clone(),
        };
        // 布尔也做成下拉框：`CB_SETCURSEL(-1)` 正好表达"文件里没写这一行"，
        // 而复选框只有勾/不勾两态，说不出"没设过"。
        let options: Vec<String> = match key["kind"].as_str() {
            Some("bool") => vec!["true".to_owned(), "false".to_owned()],
            Some("enum") => key["options"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(|v| v.as_str())
                        .map(|v| v.to_owned())
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        fields.push(Field {
            name,
            brief,
            original: key["raw"].as_str().map(|v| v.to_owned()),
            options,
            // 打码由服务端的类型表决定，界面自己判断"哪个键是口令"就会变成
            // 第二套判断 —— 而它一定会和 `configedit` 分叉。
            secret: key["kind"].as_str() == Some("secret"),
            hwnd: 0,
            label: 0,
        });
    }
    Ok(fields)
}

/// 把 `options` 填进下拉框，并选中文件里那个值；没写这一行就不选。
unsafe fn fill_combo(combo: HWND, options: &[String], selected: Option<&str>) {
    for option in options {
        let w = wide(option);
        SendMessageW(combo, CB_ADDSTRING, 0, w.as_ptr() as isize);
    }
    // `usize::MAX` 就是 -1：不选任何项，显示为空。保存时"空"= 删掉这一行。
    let index = selected
        .and_then(|value| {
            options
                .iter()
                .position(|option| option.eq_ignore_ascii_case(value))
        })
        .map(|i| i as usize)
        .unwrap_or(usize::MAX);
    SendMessageW(combo, CB_SETCURSEL, index, 0);
}

/// 按 `FIELDS` 建控件，返回行数。font 与 instance 在里面取，免得把
/// `HMODULE`/`HFONT` 的具体类型签名抄到这个函数上。
unsafe fn build_config_controls(parent: HWND) -> i32 {
    let instance = GetModuleHandleW(std::ptr::null());
    let mut fields = FIELDS.lock().unwrap_or_else(|e| e.into_inner());
    for (i, field) in fields.iter_mut().enumerate() {
        // 先建成 0 尺寸：位置与字体统一由 `apply_config_layout` 按当前 DPI 定，
        // 免得"创建时用一套坐标、重排时用另一套"。
        let text = wide(&field.brief);
        let c = wide("STATIC");
        let label = CreateWindowExW(
            0,
            c.as_ptr(),
            text.as_ptr(),
            0 | WS_CHILD | WS_VISIBLE,
            0,
            0,
            0,
            0,
            parent,
            0,
            instance,
            std::ptr::null(),
        );
        // 控件：枚举/布尔是下拉框，其余是文本框
        let (class, style) = if field.options.is_empty() {
            // ES_* 在 windows-sys 里是 i32，style 是 u32（WINDOW_STYLE）。
            // 口令那一格加 ES_PASSWORD：不是为了防谁，而是因为这块屏幕
            // 常常正被远程看着（这软件本身就是干这个的），明文口令出现在
            // 别人的画面上是实打实的泄露。
            let extra = if field.secret { ES_PASSWORD as u32 } else { 0 };
            ("EDIT", WS_BORDER | ES_AUTOHSCROLL as u32 | extra)
        } else {
            // 下拉列表（只能选不能打）：值域由类型表定，手打没有意义。
            ("COMBOBOX", CBS_DROPDOWNLIST | WS_VSCROLL)
        };
        let c = wide(class);
        let blank = wide("");
        let input = CreateWindowExW(
            0,
            c.as_ptr(),
            blank.as_ptr(),
            style | WS_CHILD | WS_VISIBLE,
            0,
            0,
            0,
            0,
            parent,
            (FIELD_BASE + i as i32) as isize,
            instance,
            std::ptr::null(),
        );
        let selected = field.original.as_deref().map(plain);
        if field.options.is_empty() {
            let w = wide(selected.as_deref().unwrap_or(""));
            SetWindowTextW(input, w.as_ptr());
        } else {
            // 下拉框的"高度"是展开列表的高度，得给足；重排时会换成按 DPI 算的。
            fill_combo(input, &field.options, selected.as_deref());
        }
        field.hwnd = input;
        field.label = label;
    }
    ((fields.len() + 1) / 2) as i32
}

/// 按当前 DPI 排一遍表单（`WM_CREATE` 与 `WM_DPICHANGED` 共用）。
fn apply_config_layout(hwnd: HWND) {
    let dpi = window_dpi(hwnd);
    let font = ui_font(dpi);
    let rows = {
        let mut fields = FIELDS.lock().unwrap_or_else(|e| e.into_inner());
        let rows = ((fields.len() + 1) / 2) as i32;
        for (i, field) in fields.iter_mut().enumerate() {
            let column = (i as i32) / rows;
            let row = (i as i32) % rows;
            let x = scaled(COL_X[column as usize], dpi);
            let y = scaled(12 + row * FIELD_PITCH, dpi);
            let width = scaled(FIELD_W, dpi);
            // 下拉框给足展开高度（列表项数 × 行高 + 边），否则只有两三行可看。
            let height = if field.options.is_empty() {
                scaled(24, dpi)
            } else {
                scaled(200, dpi)
            };
            unsafe {
                MoveWindow(field.label, x, y, width, scaled(16, dpi), 1);
                MoveWindow(field.hwnd, x, y + scaled(18, dpi), width, height, 1);
                SendMessageW(field.label, WM_SETFONT, font as usize, 1);
                SendMessageW(field.hwnd, WM_SETFONT, font as usize, 1);
            }
        }
        rows
    };
    let y = scaled(12 + rows * FIELD_PITCH + 8, dpi);
    for &(_, id, x, w) in CFG_BUTTONS {
        let child = unsafe { GetDlgItem(hwnd, id) };
        if child == 0 {
            continue;
        }
        unsafe {
            MoveWindow(child, scaled(x, dpi), y, scaled(w, dpi), scaled(30, dpi), 1);
            SendMessageW(child, WM_SETFONT, font as usize, 1);
        }
    }
    // 复选框放在第二列那侧（第一列的按钮只排到 332，COL_X[1] = 360）。
    unsafe {
        let child = GetDlgItem(hwnd, ID_SHOW_SECRET);
        if child != 0 {
            MoveWindow(child, scaled(COL_X[1], dpi), y, scaled(FIELD_W, dpi), scaled(24, dpi), 1);
            SendMessageW(child, WM_SETFONT, font as usize, 1);
        }
    }
}

/// 勾上「显示口令（临时）」→ 所有 secret 格临时明文；取消 → 回到打码。
fn toggle_secret_fields(hwnd: HWND) {
    let fields = FIELDS.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        let box_hwnd = GetDlgItem(hwnd, ID_SHOW_SECRET);
        let revealed = box_hwnd != 0
            && SendMessageW(box_hwnd, BM_GETCHECK, 0, 0) == BST_CHECKED;
        for field in fields.iter() {
            if !field.secret || field.hwnd == 0 {
                continue;
            }
            // 0 = 不设密码字符（明文）；0x25CF = ● 回填。
            let ch = if revealed { 0usize } else { 0x25CFusize };
            SendMessageW(field.hwnd, EM_SETPASSWORDCHAR, ch, 0);
            InvalidateRect(field.hwnd, std::ptr::null(), 1);
        }
    }
}

/// 把 `FIELDS` 里的原值重新摆到控件上（打开窗口时、保存之后各一次）。
fn fill_fields() {
    let fields = FIELDS.lock().unwrap_or_else(|e| e.into_inner());
    for field in fields.iter() {
        if field.hwnd == 0 {
            continue;
        }
        let selected = field.original.as_deref().map(plain);
        unsafe {
            if field.options.is_empty() {
                let w = wide(selected.as_deref().unwrap_or(""));
                SetWindowTextW(field.hwnd, w.as_ptr());
            } else {
                // 重新填一遍选项会把已有的删掉重来，这里只挪选中项就够了。
                let index = selected
                    .as_deref()
                    .and_then(|value| {
                        field
                            .options
                            .iter()
                            .position(|option| option.eq_ignore_ascii_case(value))
                    })
                    .map(|i| i as usize)
                    .unwrap_or(usize::MAX);
                SendMessageW(field.hwnd, CB_SETCURSEL, index, 0);
            }
        }
    }
}

/// 把改过的那些格送去 `--ops config-set`。
///
/// 返回 `(是否真的成功, 要贴进日志的行)`。「没有改动」算成功 —— 第一次写这里
/// 时把它当失败弹了框，点保存没动东西却被告知"没有存成功"，是最冤的一种错。
///
/// 只送**真的改过**的：全量提交会把每一行都重写一遍，diff 里看不出人到底动了
/// 什么，而"我刚才改的是哪一项"是这类工具最常被问到的事。
fn submit_config() -> (bool, Vec<String>) {
    let exe = run_root().join("stream-server.exe");
    let mut sets: Vec<(String, String)> = Vec::new();
    let mut unsets: Vec<String> = Vec::new();
    {
        let fields = FIELDS.lock().unwrap_or_else(|e| e.into_inner());
        for field in fields.iter() {
            if field.hwnd == 0 {
                continue;
            }
            let now = control_text(field.hwnd);
            let now = now.trim();
            let was = field.original.as_deref().map(plain).unwrap_or_default();
            if now == was.as_ref() {
                continue;
            }
            if now.is_empty() {
                // 空 = 用默认值 = 删掉这一行。原来就没写的不用报。
                if field.original.is_some() {
                    unsets.push(field.name.clone());
                }
            } else {
                sets.push((field.name.clone(), now.to_owned()));
            }
        }
    }
    if sets.is_empty() && unsets.is_empty() {
        return (true, vec!["-- 没有改动，这次没有写文件".to_owned()]);
    }
    let mut args: Vec<String> = vec!["--ops".to_owned(), "config-set".to_owned()];
    for (key, value) in &sets {
        args.push("--ops-set".to_owned());
        args.push(format!("{key}={value}"));
    }
    for key in &unsets {
        args.push("--ops-unset".to_owned());
        args.push(key.clone());
    }
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let (code, text) = run_capture(&exe, &refs);
    let mut lines = vec![format!("[exit {code}] 改配置")];
    for line in text.lines() {
        if !line.trim().is_empty() {
            lines.push(line.to_owned());
        }
    }
    if code != 0 {
        lines.push("没有写进文件。上面那句说了为什么（超范围、拼错、或者文件改坏了）".to_owned());
    }
    (code == 0, lines)
}

/// 重新读一遍配置并刷新表单。失败就弹框说明，不留一个空表单让人瞎猜。
fn reload_config_form(main: HWND) -> bool {
    let exe = run_root().join("stream-server.exe");
    match read_config_fields(&exe) {
        Ok(fields) => {
            {
                let mut slot = FIELDS.lock().unwrap_or_else(|e| e.into_inner());
                // 保留已经建好的控件句柄：重建窗口才能拿到新句柄，而窗口不该
                // 为了值变化而重建。
                let handles: Vec<(HWND, HWND)> =
                    slot.iter().map(|f| (f.hwnd, f.label)).collect();
                *slot = fields;
                for (field, (hwnd, label)) in slot.iter_mut().zip(handles) {
                    field.hwnd = hwnd;
                    field.label = label;
                }
            }
            fill_fields();
            true
        }
        Err(err) => {
            log(&format!("[fail] {err}"), main);
            notice(main, &err, "读配置");
            false
        }
    }
}

/// 打开（或叫到前面）配置表单窗口。
fn open_config_window(main: HWND) {
    let existing = *CONFIG_HWND.lock().unwrap_or_else(|e| e.into_inner());
    if existing != 0 {
        reload_config_form(main);
        unsafe {
            ShowWindow(existing as HWND, SW_SHOWNORMAL);
            SetForegroundWindow(existing as HWND);
        }
        return;
    }
    let exe = run_root().join("stream-server.exe");
    if !exe.exists() {
        notice(
            main,
            "找不到 stream-server.exe。\n\n这个窗口要和它放在同一目录：\
             读配置、校验、写文件都由它做。",
            "改配置",
        );
        return;
    }
    let fields = match read_config_fields(&exe) {
        Ok(fields) => fields,
        Err(err) => {
            log(&format!("[fail] {err}"), main);
            notice(main, &err, "读配置");
            return;
        }
    };
    let rows = ((fields.len() + 1) / 2) as i32;
    {
        let mut slot = FIELDS.lock().unwrap_or_else(|e| e.into_inner());
        *slot = fields;
    }
    // 窗口高度按行数算：加一个键不该把按钮挤出可视区。
    let client_h = 12 + rows * FIELD_PITCH + 8 + 32 + 12;
    let dpi = {
        let d = unsafe { GetDpiForSystem() };
        if d == 0 {
            96
        } else {
            d
        }
    };
    unsafe {
        let class = wide("CloudStreamConfig");
        let title = wide("配置（改完要重启服务才生效）");
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            scaled(COL_X[1] + FIELD_W + 32, dpi),
            scaled(client_h + 39, dpi),
            main,
            0,
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        );
        if hwnd == 0 {
            log("[fail] 建不出配置窗口", main);
            return;
        }
        *CONFIG_HWND.lock().unwrap_or_else(|e| e.into_inner()) = hwnd as isize;
        log("-- 配置表单已打开（每格的含义见 docs/configuration.md）", main);
    }
}

/// 表单窗口的消息循环。
unsafe extern "system" fn config_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            build_config_controls(hwnd);
            let instance = GetModuleHandleW(std::ptr::null());
            for &(text, id, _, _) in CFG_BUTTONS {
                let c = wide("BUTTON");
                let t = wide(text);
                CreateWindowExW(
                    0,
                    c.as_ptr(),
                    t.as_ptr(),
                    (BS_PUSHBUTTON as u32) | WS_CHILD | WS_VISIBLE,
                    0,
                    0,
                    0,
                    0,
                    hwnd,
                    id as isize,
                    instance,
                    std::ptr::null(),
                );
            }
            // 口令是打码显示的；给个复选框临时揭开，方便首次安装时抄到别处。
            let c = wide("BUTTON");
            let t = wide("显示口令（临时）");
            CreateWindowExW(
                0,
                c.as_ptr(),
                t.as_ptr(),
                (BS_AUTOCHECKBOX as u32) | WS_CHILD | WS_VISIBLE,
                0,
                0,
                0,
                0,
                hwnd,
                ID_SHOW_SECRET as isize,
                instance,
                std::ptr::null(),
            );
            // 位置与字体统一在这里定，按窗口所在屏的 DPI。
            apply_config_layout(hwnd);            0
        }
        WM_COMMAND => {
            let id = (wparam & 0xffff) as i32;
            let main = *MAIN_HWND.lock().unwrap_or_else(|e| e.into_inner()) as HWND;
            match id {
                ID_SHOW_SECRET => {
                    toggle_secret_fields(hwnd);
                }
                BTN_CFG_SAVE => {
                    // 写文件是毫秒级的（改一份小文本 + 复制一份留底），不复用
                    // spawn_job：那会把"保存成功没有"推到日志末尾，而人此刻
                    // 正盯着这个窗口。
                    let (ok, lines) = submit_config();
                    for line in &lines {
                        log(line, main);
                    }
                    if !ok {
                        // 失败时**不要**重新读文件：人刚打的字还在格子里，
                        // 覆掉之后既看不见自己写了什么、也不知道该改哪儿。
                        notice(
                            hwnd,
                            "没有存成功。\n\n原因在下面这块文字里，也可以看主窗口的操作记录。",
                            "改配置",
                        );
                        return 0;
                    }
                    // 成功才回读：把服务端规范化过的值（`VP9` → `'vp9'`）显示出来。
                    reload_config_form(main);
                    on_command(main, BTN_REFRESH);
                }
                BTN_CFG_RELOAD => {
                    if reload_config_form(main) {
                        log("-- 配置已重新读入（未保存的改动丢了）", main);
                    }
                }
                BTN_CFG_RAW => {
                    // 逃生口：表单只覆盖常见的那十几项，证书、鉴权这些还得开原文。
                    let cfg = run_root().join("stream-config.toml");
                    let path = if cfg.exists() {
                        cfg
                    } else {
                        run_root().join("config.toml")
                    };
                    let verb = wide("open");
                    let file = wide("notepad.exe");
                    let args = wide(&format!("\"{}\"", path.display()));
                    let dir = wide(&run_root().display().to_string());
                    ShellExecuteW(
                        hwnd,
                        verb.as_ptr(),
                        file.as_ptr(),
                        args.as_ptr(),
                        dir.as_ptr(),
                        SW_SHOWNORMAL as i32,
                    );
                    log(&format!("-- 已用记事本打开 {}（表单以外的键要在这里改）", path.display()), main);
                }
                _ => {}
            }
            0
        }
        WM_DPICHANGED => {
            // lparam 是系统建议的新矩形（已按新 DPI 算好）：照它摆，再重排。
            let rect = lparam as *const RECT;
            if !rect.is_null() {
                let r = *rect;
                unsafe {
                    SetWindowPos(
                        hwnd,
                        0,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
            apply_config_layout(hwnd);
            0
        }
        // X 只隐藏：窗口留着，人下次打开时改了一半的内容还在。
        WM_CLOSE => {
            ShowWindow(hwnd, 0); // SW_HIDE
            0
        }
        WM_DESTROY => 0,
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 命令分发。
fn on_command(hwnd: HWND, id: i32) {
    let root = run_root();
    let exe = root.join("stream-server.exe");
    match id {
        BTN_REFRESH => {
            let root = run_root();
            // 一次进程快照，两处用：面板上的"服务"那行，和那个按钮该写什么。
            let pids = service::pids(&root);
            let mut out = program_info(&root, &pids);
            if exe.exists() {
                let (_, text) = run_capture(&exe, &["--ops", "state"]);
                out.push_str("\n-- 服务端自述（--ops state）--\n");
                out.push_str(text.trim_end());
            } else {
                out.push_str("\n找不到 stream-server.exe（本窗口要和它放在同一目录）");
            }
            set_state(&out, hwnd);
            update_power_label(hwnd, !pids.is_empty());
            log("-- 状态已刷新", hwnd);
        }
        BTN_POWER => {
            // 按钮写的是"点下去会发生什么"，动作也照那个走 —— 判断只有一处。
            let stopping = service::running(&root);
            // 只有"停"要问：它会把所有已连上的远端断掉（这软件本身就是干这个
            // 的），顺便把默认录音设备还回去，而且连看门狗一起禁用。
            if stopping
                && !confirm(
                    hwnd,
                    "停止服务会断开所有已连上的远端，并把默认录音设备还回去。\n\n\
                     看门狗也会一起禁用 —— 不点「开始服务」，重启后它也不会自己起来。\n\n\
                     继续？",
                    "停止服务",
                )
            {
                log("-- 已取消停止", hwnd);
                return;
            }
            // 两条路都要等（停止等进程退、启动等端口应答），所以走后台线程：
            // 同步跑会让窗口"未响应"，而一个未响应的窗口看起来就是崩了。
            let root = root.clone();
            spawn_job(hwnd, move || {
                if stopping {
                    service::stop(&root).lines
                } else {
                    service::start(&root).lines
                }
            });
        }
        BTN_UPDATE => {
            if !exe.exists() {
                log("[fail] 找不到 stream-server.exe", hwnd);
                return;
            }
            // 整个"检查 → 问一句 → 覆盖"放在一个后台任务里，因为中间那一步
            // 要等人点确认；分两段写会让窗口在等待期间被当成卡死。确认框由
            // 后台线程弹：MessageBox 会在自己那条线程上跑一个模态循环，
            // 主线程照常泵消息，所以窗口不会假死。
            let target = hwnd as isize;
            spawn_job(hwnd, move || {
                let hwnd = target as HWND;
                let mut lines = Vec::new();

                let (code, text) = run_capture(&exe, &["--update", "check"]);
                lines.push(format!("[exit {code}] 检查更新"));
                lines.push(text.trim_end().to_owned());
                match code {
                    0 => {}
                    5 => {
                        lines.push(
                            "已经是最新版本（更新包里的版本不比当前新）".to_owned(),
                        );
                        return lines;
                    }
                    4 => {
                        lines.push(
                            "没找到更新包目录。把新的一包解到 \
                             %ProgramData%\\cloud-stream\\update 再试。"
                                .to_owned(),
                        );
                        return lines;
                    }
                    _ => {
                        lines.push("检查失败，详情见上".to_owned());
                        return lines;
                    }
                }

                let ok = confirm(
                    hwnd,
                    "用更新包里的文件覆盖当前程序？\n\n\
                     覆盖之前会自动留一份完整快照；新版本起不来或版本对不上，\
                     会自动退回原样。\n\n\
                     服务需要重启才会用上新版本。",
                    "更新",
                );
                if !ok {
                    lines.push("-- 已取消更新".to_owned());
                    return lines;
                }

                let (code, text) = run_capture(&exe, &["--update", "apply"]);
                lines.push(format!("[exit {code}] 更新"));
                lines.push(text.trim_end().to_owned());
                lines.push(if code == 0 {
                    // 原来写的是"重启服务（安装 / 修复）后生效" —— 那个入口已经
                    // 没了。现在重启服务的正路是旁边那个按钮：停止 → 开始
                    // （走看门狗，`/Run` 用的就是刚覆盖上去的那个 exe）。
                    "更新完成：点「停止服务」再「开始服务」即可用上新版本".to_owned()
                } else {
                    "更新没有完成；失败时会自动回滚，情况见上面的记录".to_owned()
                });
                lines
            });
        }
        BTN_UNINSTALL => {
            // 卸载归安装器。Inno 把整套卸载逻辑编进了运行根里的 `unins000.exe`
            // （`[Code]` 里逐条调后端能力，见 toolchain\inno\cloud-stream.iss），
            // 本窗口只负责把它叫起来。
            //
            // 这里**刻意不自己调 `--uninstall`**：那样一来"卸载要做哪几步"就有
            // 两份定义 —— 一份在 uninstaller，一份在这里，而两份迟早分叉
            // （最先分叉的会是"配置要不要先备份"这种判断）。
            let unins = run_root().join("unins000.exe");
            if !unins.exists() {
                notice(
                    hwnd,
                    "这个目录里没有 unins000.exe —— 说明它不是用向导版/MSI 装的（多半是\
                     便携包直接解出来的）。\n\n那种安装没有卸载器，删掉整个目录即可；\
                     服务和防火墙规则会由卸载前那一步静默处理。",
                    "卸载",
                );
                return;
            }
            if !confirm(
                hwnd,
                "卸载会停服务、删防火墙规则、删整个目录。\n\n\
                 机器身份（配置/证书/录音设备原值）会先备份到\n\
                 %ProgramData%\\cloud-stream\\uninstall-backup\n\n继续？",
                "卸载",
            ) {
                log("-- 已取消卸载", hwnd);
                return;
            }
            // 交给 unins000：它是 requireAdministrator 的，`runas` 会弹一次 UAC。
            // 不能用 `Command::spawn` —— 从非提权进程直接 CreateProcess 一个
            // 要求管理员的 exe，拿到的是 ERROR_ELEVATION_REQUIRED（740），
            // 而那个错误长得像"文件不存在"。
            match sys::launch_elevated(&unins, "", &run_root()) {
                Ok(()) => {
                    log("-- 已交给卸载器；本窗口这就关闭（它要删的就是这个 exe）", hwnd);
                    // 必须真的退出：目录里有正在执行的 exe，谁都删不掉它。
                    unsafe {
                        DestroyWindow(hwnd);
                    }
                }
                Err(sys::ElevateError::Denied) => log("[warn] UAC 被拒绝，什么都没动", hwnd),
                Err(sys::ElevateError::Failed(code)) => log(
                    &format!("[fail] 起不了卸载器（ShellExecute={code}）"),
                    hwnd,
                ),
            }
        }
        BTN_CONFIG => {
            // 表单取代记事本：值域校验、留底、原子写都在 `--ops config-set`
            // 里做完了，所以这里不再自己拍快照 —— 两处留底只会让"退回到哪
            // 一份"变成两个答案。
            open_config_window(hwnd);
        }
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let parent = hwnd;
            let instance = GetModuleHandleW(std::ptr::null());
            // ES_* / BS_* 在 windows-sys 里是 i32，而 CreateWindowExW 的 style 是
            // u32（WINDOW_STYLE），所以这里统一抬成 u32 再拼。
            const READONLY_EDIT: u32 =
                (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32 | WS_BORDER | WS_VSCROLL;
            const BUTTON: u32 = BS_PUSHBUTTON as u32;
            // 位置与字体不在这里定：`apply_main_layout` 按窗口所在屏的 DPI 统一
            // 摆一遍，`WM_DPICHANGED` 也用它。散着写迟早分叉成两套坐标。
            let make = |class: &str, text: &str, style: u32, id: i32| -> HWND {
                let c = wide(class);
                let t = wide(text);
                CreateWindowExW(
                    0,
                    c.as_ptr(),
                    t.as_ptr(),
                    style | WS_CHILD | WS_VISIBLE,
                    0,
                    0,
                    0,
                    0,
                    parent,
                    id as isize,
                    instance,
                    std::ptr::null(),
                )
            };

            make("STATIC", "程序信息", 0, ID_LABEL_STATE);
            make("EDIT", "", READONLY_EDIT, ID_STATE);

            let buttons: [(&str, i32); 5] = [
                ("刷新状态", BTN_REFRESH),
                // 初始文字按此刻的真实状态定：窗口一出现，按钮和下面面板说的
                // 就得是同一件事，不该先写"开始"再等刷新改回来。
                (service::power_label(service::running(&run_root())), BTN_POWER),
                ("更新", BTN_UPDATE),
                ("卸载", BTN_UNINSTALL),
                ("改配置", BTN_CONFIG),
            ];
            for (label, id) in buttons {
                make("BUTTON", label, BUTTON, id);
            }

            make("STATIC", "操作记录", 0, ID_LABEL_LOG);
            make("EDIT", "", READONLY_EDIT, ID_LOG);

            apply_main_layout(hwnd);
            SetWindowTextW(hwnd, wide("stream-setting · stream-server 的工具箱").as_ptr());
            0
        }
        WM_COMMAND => {
            let id = (wparam & 0xffff) as i32;
            on_command(hwnd, id);
            0
        }
        WM_APP_DONE => {
            // 动作结束后顺手刷一次状态：装完/回滚完，人第一眼看的就是版本。
            on_command(hwnd, BTN_REFRESH);
            0
        }
        WM_DPICHANGED => {
            // 窗口被拖到另一块不同缩放的屏上了。系统给的矩形是"按新 DPI 重新
            // 算过的窗口大小"，照着摆；再按新 DPI 重排控件与字体。
            let rect = lparam as *const RECT;
            if !rect.is_null() {
                let r = *rect;
                unsafe {
                    SetWindowPos(
                        hwnd,
                        0,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
            apply_main_layout(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// GUI 子系统下的标准句柄兜底。
///
/// 见模块头部：这个 exe 没有 `println!`，但"标准句柄无效"是**进程级**的隐患
/// —— 任何人将来在这里加一句调试输出，都会得到"窗口一闪就没了"。指到 `NUL`
/// 之后，那种写法至少是安全的空操作。
#[cfg(windows)]
fn stdio_guard() {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    fn usable(handle: isize) -> bool {
        handle != 0 && handle != INVALID_HANDLE_VALUE as isize
    }

    unsafe {
        // 被重定向（管道、文件）时句柄有效，一个字都别动。
        if usable(GetStdHandle(STD_OUTPUT_HANDLE)) {
            return;
        }
        // 从命令行跑的时候接上父控制台 —— 从这里调试的人不该看不到输出。
        if AttachConsole(ATTACH_PARENT_PROCESS) != 0
            && usable(GetStdHandle(STD_OUTPUT_HANDLE))
        {
            return;
        }
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
fn stdio_guard() {}

fn main() {
    // 必须在任何输出之前（虽然现在一行都没有，但缺陷是"将来加一行就炸"）。
    stdio_guard();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // 诊断：把要注册的任务 XML 原样打出来。字段里排查"任务建不起来"靠它 ——
    // 所以放在分流之前，任何一份 exe 都能问（不需要内置包）。参数照传：
    // `--dir` 能替另一台机器渲染，否则路径按"这个 exe 会装到哪儿"推。
    if args.iter().any(|a| a == "--task-xml") {
        std::process::exit(install::print_task_xml(&args));
    }
    // 「程序信息」面板那几行也能从命令行问。加这个开关不是"顺手"，而是让
    // 面板上那句话（服务/端口/看门狗）**可验证** —— 窗口在 session 0 里
    // 没有可截图的桌面，远端只能靠这条命令确认它到底算出什么。
    // 和 `--task-xml` 一样属于"结果本身就是答案"的那一类。
    if args.iter().any(|a| a == "--program-info") {
        let root = run_root();
        print!("{}", program_info(&root, &service::pids(&root)));
        std::process::exit(0);
    }
    // 「诊断」在这个 exe 上也接：用户不一定记得九节取证在服务端那一份 exe 上，
    // 而两个 exe 躺在同一个目录里，敲错是常态。转调过去，stdio 直接继承 ——
    // 九节正文不该被我们截一遍再原样打出来。
    //
    // 位置和 `--program-info` 一样在角色判断之前：诊断不需要提权、不需要包，
    // 落进 `classify` 就会被当成控制台开一个窗口（`--uninstall-step` 那次
    // 死锁就是这么来的）。
    if args.iter().any(|a| a == "--diagnose") {
        let server = run_root().join("stream-server.exe");
        let code = match std::process::Command::new(&server).arg("--diagnose").status() {
            Ok(status) => status.code().unwrap_or(1),
            // 拿不到那份 exe 时唯一有意义的说法是"它不在这儿"，
            // 而不是一句"诊断失败"。
            Err(err) => {
                eprintln!("--diagnose：跑不起来 {}（{err}）", server.display());
                1
            }
        };
        std::process::exit(code);
    }
    // 开关服务（`--service start|stop`）：窗口上那个按钮的命令行形态。
    //
    // 和 `--purge` / `--uninstall-step` 一样必须在角色判断**之前**截住：装好
    // 之后的 exe 旁边就有 `stream-server.exe`，落进 `classify` 会被当成控制台，
    // 于是开一个窗口进消息循环 —— 而敲命令的人等的是一个退出码。
    if args.iter().any(|a| a == "--service") {
        std::process::exit(service::run(&args));
    }
    // 替身（`--purge`）必须在角色判断**之前**截住：它跑在 `%TEMP%` 里，那儿
    // 没有 `stream-server.exe`，而它自带有内置包 —— 按下面的判据会被认成
    // "安装器"，于是它去解压一个包，而不是去删目录。
    if args.iter().any(|a| a == "--purge") {
        std::process::exit(uninstall::run(&args));
    }
    // `--uninstall-step` 必须在这里截住，**不能落进下面的角色判断**。
    //
    // 这是一条实机撞出来的死锁，代价是"整台机器看起来失联、卸载一行日志都没写"：
    //
    //   装好之后的 exe 旁边一定有 `stream-server.exe`，按 `classify` 的判据它是
    //   **控制台**。于是卸载器 `Exec(..., ewWaitUntilTerminated)` 起的那个
    //   `stream-setting.exe --uninstall-step task` 去建了一个窗口、进消息循环
    //   等着人点按钮；而窗口是 `SW_HIDE` 的，没有人能点。两个进程互相等：
    //   卸载器等子进程退出，子进程等一个永远不会来的消息。
    //
    // 更麻烦的是它的**症状**：`uninstall::run` 根本没被调用，report 文件一个
    // 字节都没写，Inno 的日志也停在这一步之前 —— 现场看起来完全像"卸载器自己
    // 卡住了"，而不是"我们开错了窗口"。
    //
    // 和 `--purge` 同一类：这类开关一律在角色判断之前分流，判据本身不动。
    if args.iter().any(|a| a == "--uninstall-step") {
        std::process::exit(uninstall::run(&args));
    }
    // `--inbound` 同理提前：它既可能在整包目录里跑（那里有 stream-server.exe，
    // 按判据是控制台），也可能在装好之后的运行根里跑。两种地方都要能补规则。
    if args.iter().any(|a| a == "--inbound") {
        std::process::exit(install::run(&args));
    }
    // 安装器这一支不建窗口、不注册窗口类：它要的是一个提权后的进程在跑，
    // 而不是一个窗口。所以分流放在这里，而不是窗口里加一个按钮分情况。
    if decide_mode(&args) == Mode::Installer {
        // 同一个 exe 两个方向：`--uninstall` 是卸，其余是装。
        if args.iter().any(|a| a == "--uninstall") {
            std::process::exit(uninstall::run(&args));
        }
        std::process::exit(install::run(&args));
    }
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide("StreamSetting");
        // 窗口标题栏、Alt+Tab、任务栏读的是"窗口类的图标"，而不是 exe 的。
        // 不填（`hIcon: 0`）时系统给一个通用图标 —— 右键属性里明明有图标，
        // 运行时却是白纸，这种不一致最容易被当成"图标没生效"。
        //
        // 编号 1 是硬约定：`build.rs` 里那句 `1 ICON` 就是它。改了任一边，
        // 这个窗口会安静地退回通用图标（LoadIconW 失败给 0，不报错）。
        let app_icon = LoadIconW(instance, 1 as *const u16);
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: app_icon,
            hCursor: LoadCursorW(0, 32512 as *const u16),
            // COLOR_BTNFACE + 1：用系统对话框底色，跟随用户的浅色/深色主题。
            hbrBackground: 16,
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);

        // 配置表单是第二个窗口：自己的控件、自己的窗口过程，但起的是同一个
        // 后端（`--ops config-set`）。窗口类在这里一起注册 —— 延迟注册只能
        // 在窗口过程里做，那里的 unsafe 只会更绕。
        let config_class = wide("CloudStreamConfig");
        let config_wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(config_wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: app_icon,
            hCursor: LoadCursorW(0, 32512 as *const u16),
            hbrBackground: 16,
            lpszMenuName: std::ptr::null(),
            lpszClassName: config_class.as_ptr(),
        };
        RegisterClassW(&config_wc);

        // 初始尺寸就按系统 DPI 算：窗口一出现就该是合适的大小，而不是先按
        // 96 DPI 建出来再等 `WM_DPICHANGED` 补救 —— 创建窗口时不会有那个消息。
        let dpi = {
            let d = GetDpiForSystem();
            if d == 0 {
                96
            } else {
                d
            }
        };
        let title = wide("stream-setting · stream-server 的工具箱");
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            scaled(MAIN_WINDOW.0, dpi),
            scaled(MAIN_WINDOW.1, dpi),
            0,
            0,
            instance,
            std::ptr::null(),
        );
        if hwnd == 0 {
            return;
        }
        // 表单窗口要往这里写日志，而窗口过程拿不到"我是谁打开的"。
        *MAIN_HWND.lock().unwrap_or_else(|e| e.into_inner()) = hwnd as isize;
        ShowWindow(hwnd, SW_SHOWNORMAL);
        // 先刷新一次：窗口打开时"这台机器现在是什么状态"就该在上面，
        // 而不是等用户去点刷新才出现。
        on_command(hwnd, BTN_REFRESH);
        // `--config`：直接把表单打开。给快捷方式用的 —— "改个帧率"不该要求
        // 先点开控制台、再找到按钮。
        if std::env::args().any(|a| a == "--config") {
            open_config_window(hwnd);
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, 0, 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 缩放要四舍五入，不能截断。
    ///
    /// 125% 下 100 该是 125；截断得 124，两个控件各差 1px 看不出来，但一列
    /// 排下来就是可见的错位，而且是"只有高分屏才复现"的那类问题。
    #[test]
    fn scaling_rounds_instead_of_truncating() {
        assert_eq!(scaled(16, 96), 16);
        assert_eq!(scaled(100, 120), 125); // 1.25
        assert_eq!(scaled(100, 144), 150); // 1.5
        assert_eq!(scaled(100, 168), 175); // 1.75
        assert_eq!(scaled(100, 192), 200); // 2.0
        assert_eq!(scaled(83, 120), 104); // 103.75 → 104
    }

    /// 出框的控件在窗口上根本看不见，而这种错只有肉眼能发现 —— 除非这里守着。
    #[test]
    fn every_control_fits_in_the_window() {
        for &(id, x, y, w, h) in MAIN_LAYOUT {
            assert!(x >= 0 && y >= 0, "控件 {id} 的坐标是负的");
            assert!(
                x + w <= MAIN_WINDOW.0,
                "控件 {id} 右边出框：{} > {}",
                x + w,
                MAIN_WINDOW.0
            );
            assert!(
                y + h <= MAIN_WINDOW.1,
                "控件 {id} 下边出框：{} > {}",
                y + h,
                MAIN_WINDOW.1
            );
        }
    }

    /// 按钮行：等宽、等距、正好铺满状态框。
    ///
    /// 加那个「开始/停止」按钮时把 4×178 改成了 5×140，这组数字之间是有关系的
    /// （5×140 + 4×12 = 748 = 状态框宽度）。少算一个间隔、或者只改宽度不改 x，
    /// 得到的是"最右边那个按钮压在边框上"这种只有实机才看得出来的错，所以在这里
    /// 把关系写死。
    #[test]
    fn the_button_row_is_evenly_spaced() {
        const BUTTONS: [i32; 5] = [
            BTN_REFRESH,
            BTN_POWER,
            BTN_UPDATE,
            BTN_UNINSTALL,
            BTN_CONFIG,
        ];
        let mut prev: Option<(i32, i32, i32)> = None; // (x, y, w)
        for id in BUTTONS {
            let &(_, x, y, w, _) = MAIN_LAYOUT
                .iter()
                .find(|(control, ..)| *control == id)
                .unwrap_or_else(|| panic!("按钮 {id} 不在 MAIN_LAYOUT 里"));
            match prev {
                None => assert_eq!(x, 16, "第一个按钮的左边距不是 16"),
                Some((px, py, pw)) => {
                    assert_eq!(y, py, "按钮 {id} 换行了");
                    assert_eq!(w, pw, "按钮 {id} 和左边那个不一样宽");
                    assert_eq!(x - px, pw + 12, "按钮 {id} 的间距不是 12");
                }
            }
            prev = Some((x, y, w));
        }
        let (last_x, _, last_w) = prev.unwrap();
        assert_eq!(
            last_x + last_w,
            16 + 748,
            "按钮行没有铺满状态框（右边会留一条缝，或者压出框）"
        );
    }

    /// 表单是两列，第二列必须从第一列右边之外开始，否则两列叠在一起。
    #[test]
    fn config_columns_do_not_overlap() {
        assert!(
            COL_X[1] >= COL_X[0] + FIELD_W,
            "两列挤在一起了：{} + {} > {}",
            COL_X[0],
            FIELD_W,
            COL_X[1]
        );
    }

    /// 窗口高度是按行数算出来的，算错了最后一个按钮就没了。
    #[test]
    fn config_buttons_fit_under_the_fields() {
        // 与 `build_config_controls` 同一套算法：18 个键 → 9 行
        let rows = (18 + 1) / 2;
        let buttons_y = 12 + rows * FIELD_PITCH + 8;
        let window_h = 12 + rows * FIELD_PITCH + 8 + 32 + 12;
        assert!(
            buttons_y + 30 <= window_h,
            "按钮被挤出窗口：{} > {window_h}",
            buttons_y + 30
        );
        // 按钮总宽不能超过内容宽度
        let last_x = CFG_BUTTONS.iter().map(|b| b.2 + b.3).max().unwrap();
        assert!(last_x <= COL_X[1] + FIELD_W, "按钮比内容还宽：{last_x}");
    }

    /// 角色矩阵。每条都对应一个真实场景，选错就是"双击装好的 exe 又装一遍"
    /// 或者"分发出去的 exe 打不开、只弹一个空窗口"。
    #[test]
    fn role_matrix() {
        // 分发出去的那一份：有包、旁边没有服务 → 安装器。
        assert_eq!(classify(false, true, false), Mode::Installer);
        // 装好之后的那一份：有包、旁边有服务 → 控制台。双击不再重装。
        assert_eq!(classify(false, true, true), Mode::Console);
        // 开发机构建：没有包 → 永远是控制台，哪怕旁边什么都没有（那时窗口
        // 会老老实实说"找不到 stream-server.exe"）。
        assert_eq!(classify(false, false, false), Mode::Console);
        assert_eq!(classify(false, false, true), Mode::Console);
        // 显式 --install 压过一切：修复安装靠它（旁边有服务也要重装）。
        assert_eq!(classify(true, true, true), Mode::Installer);
        assert_eq!(classify(true, false, false), Mode::Installer);
    }

    /// `CLI_SWITCHES` 里每一个都必须被判成"安装器"，一个都不能漏。
    ///
    /// 这条守的是一个**已经发生过**的故障：`--uninstall-step` 漏在列表外，
    /// 而装好之后的 exe 旁边就有 `stream-server.exe`（判据把它认成控制台），
    /// 于是卸载器起的子进程开了个 `SW_HIDE` 的窗口进消息循环，卸载器
    /// `ewWaitUntilTerminated` 等它退出 —— 互相等，卸载停在第 1 步，日志一片
    /// 空白。症状看起来像"卸载器卡住"，根因是"我们开错了窗口"。
    ///
    /// 加新开关时如果忘了进这个列表，这里会红。
    #[test]
    fn every_cli_switch_stays_out_of_the_window() {
        for sw in CLI_SWITCHES {
            assert_eq!(
                decide_mode(&[(*sw).to_owned()]),
                Mode::Installer,
                "{sw} 会被判成控制台，也就是开一个没人能关的窗口"
            );
        }
    }
}
