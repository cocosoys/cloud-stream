//! 可视化控制台：四个动作的按钮版。
//!
//! 四动作差距矩阵里，「可视化界面」这一列原本四格全红 —— 安装和卸载只有 cmd
//! 黑框、更新要开发机、修改是文本编辑器。这个 exe 就是那一列。
//!
//! # 它是什么，不是什么
//!
//! 它**不是**第二个实现。窗口上的每个按钮最后都落到同一条命令上：
//!
//! ```text
//! 刷新状态 / 拍快照 / 回滚   ->  stream-server.exe --ops state|snapshot|rollback
//! 更新                      ->  stream-server.exe --update check|apply
//! 安装 / 卸载               ->  install.cmd / uninstall.cmd（提权窗口）
//! 改配置                    ->  stream-server.exe --ops config-get|config-set
//! ```
//!
//! 所以它和两个 cmd 脚本处在同一层，三个消费者一个后端。任何一个动作在窗口里
//! 能做的，在命令行里也能做，反之亦然 —— 不存在"界面上能点、脚本里做不到"这种
//! 只能靠人记住的差别。
//!
//! # 为什么不用 Tauri / egui
//!
//! Tauri 会拖进一个运行时和四百个 crate，而这个窗口要画的东西是六个按钮和两块
//! 文本。`windows-sys` 已经在依赖树里（服务端用它做采集与输入），所以这里的
//! 增量成本是零个新依赖、几秒构建。代价是界面语言是 Win32 而不是 CSS —— 对这
//! 个尺寸的界面，这笔账划算。
//!
//! # 提权的两个动作为什么在新窗口里跑
//!
//! `install.cmd` / `uninstall.cmd` 需要管理员。用 `runas` 拉起时输出在新开的
//! 控制台里，本窗口抓不到 —— 与其假装抓到了再显示一半，不如明说"已在新窗口
//! 运行"。
//!
//! 安装这一条已经接回来了：`install.cmd` 会把自己的结论写进
//! `%ProgramData%\cloud-stream\last-install.txt`（固定路径，提权换账户也读得
//! 到），本窗口在拉起之后盯着那个文件，出现 `=== RESULT:` 就把整段贴进日志。
//! 卸载还没有这一步 —— 它跑完就把目录删了，需要另找落点。
//!
//! # 改配置为什么是表单而不是记事本
//!
//! 记事本那条路的失败代价太高：拼错一个键名，服务下次启动才失败，而那一刻
//! 人已经离开了。表单把这一步挪到"保存"时：值域、枚举、布尔在
//! `--ops config-set` 里校验，写之前自动留底（`--ops rollback` 能退回），写
//! 完用启动时同一套解析器复查。界面自己不做判断，也不碰文件。
//!
//! 带 `--config` 启动会直接把表单打开（给快捷方式用）。

use std::sync::Mutex;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::CreateFontW;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetDlgItem, GetMessageW, GetWindowTextW,
    LoadCursorW, MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW,
    SetForegroundWindow, SetWindowTextW, ShowWindow, TranslateMessage, BS_PUSHBUTTON, CW_USEDEFAULT,
    ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_MULTILINE, ES_READONLY, MB_ICONINFORMATION, MB_ICONWARNING,
    MB_OK, MB_YESNO, MSG, SW_SHOWNORMAL, WM_APP, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY,
    WM_SETFONT, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_MINIMIZEBOX, WS_SYSMENU, WS_VISIBLE,
    WS_VSCROLL,
};

// 控件 id。数字直接写死：这个窗口的生命周期里只会有这一套控件。
const ID_STATE: i32 = 1100;
const ID_LOG: i32 = 1101;
const BTN_REFRESH: i32 = 1001;
const BTN_SNAPSHOT: i32 = 1002;
const BTN_ROLLBACK: i32 = 1003;
const BTN_INSTALL: i32 = 1004;
const BTN_UNINSTALL: i32 = 1005;
const BTN_CONFIG: i32 = 1006;
const BTN_UPDATE: i32 = 1007;

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

/// 运行根：本 exe 所在目录。和 `ops::run_root` 同一口径 —— 不取当前工作目录，
/// 计划任务拉起进程时那是 `System32`。
fn run_root() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
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

/// 跑一个子进程并把它说的话原样拿回来（stdout + stderr）。
///
/// `CREATE_NO_WINDOW`（0x08000000）是必需的：不带它，每次刷新状态都会闪一下
/// 黑框，而"点一下按钮闪三个黑框"是这类工具最先被骂的地方。
fn run_capture(program: &std::path::Path, args: &[&str]) -> (i32, String) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
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

/// 提权运行一个 cmd 脚本。输出在新控制台里，抓不到 —— 这一点在界面上直说。
fn run_elevated(hwnd: HWND, script: &str, extra: &str) {
    let root = run_root();
    let path = root.join(script);
    if !path.exists() {
        log(&format!("[fail] 找不到 {}", path.display()), hwnd);
        return;
    }
    let params = format!("/c \"\"{}\" {}\"", path.display(), extra);
    unsafe {
        let verb = wide("runas");
        let file = wide("cmd.exe");
        let args = wide(&params);
        let dir = wide(&root.display().to_string());
        // 返回值 > 32 才算成功。<= 32 里 5 是"用户拒了 UAC"，其余是关联错误。
        let rc = ShellExecuteW(hwnd, verb.as_ptr(), file.as_ptr(), args.as_ptr(), dir.as_ptr(), SW_SHOWNORMAL as i32);
        if rc as isize > 32 {
            log(
                &format!("[ok] 已在提权窗口运行 {script} {extra}"),
                hwnd,
            );
            log("     那个窗口里有关键的 [ok]/[fail] 结论，请以它为准", hwnd);
        } else if rc as isize == 5 {
            log("[warn] UAC 被拒绝，什么都没做", hwnd);
        } else {
            log(&format!("[fail] 无法启动 {script}（ShellExecute={rc}）"), hwnd);
        }
    }
}

/// `%ProgramData%\cloud-stream\last-install.txt`。
///
/// 和 `install.cmd` 里 `RESULT` 的默认值是同一个路径，**两边必须一致**。
/// 刻意不是 `%TEMP%`：提权可能切换账户，那时提权进程的 `%TEMP%` 是本窗口
/// 读不到的地方；`ProgramData` 对两侧是同一个字符串，也不用在
/// cmd → PowerShell → CreateProcess 这条链上给路径加引号（引号就是在这条
/// 链上死的）。
fn install_result_path() -> Option<std::path::PathBuf> {
    let program_data = std::env::var_os("ProgramData")?;
    Some(
        std::path::PathBuf::from(program_data)
            .join("cloud-stream")
            .join("last-install.txt"),
    )
}

/// 盯着结论文件，出现终局标记就把整段读回日志面板。
///
/// 为什么是"盯文件"而不是抓子进程输出：`runas` 拉起来的是另一个进程（往往
/// 还是另一个账户），它的控制台本窗口看不到。文件是两边都能看见的唯一通道。
///
/// 三条防错：只认 `started` 之后写出来的那份（上一次的 OK 不能冒充这一次）、
/// 只认带 `=== RESULT:` 的（半截文件读下去只会看到一半）、等不到就说清楚
/// 去哪儿找 —— 沉默的等待比报错更让人不知所措。
fn watch_install_result(hwnd: HWND, path: std::path::PathBuf, started: std::time::SystemTime) {
    let target = hwnd as isize;
    std::thread::spawn(move || {
        let hwnd = target as HWND;
        let grace = started
            .checked_sub(std::time::Duration::from_secs(3))
            .unwrap_or(started);
        // 240 秒：install.cmd 自己最多等 60 秒探活，加上提权、防火墙、任务注册，
        // 留足余量。超时就明说，而不是永远转下去。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
        loop {
            if std::time::Instant::now() > deadline {
                log(
                    &format!(
                        "[warn] 240 秒内没等到结论文件 {}",
                        path.display()
                    ),
                    hwnd,
                );
                log("     提权那个窗口里有关键的 [ok]/[fail] 结论，请以它为准", hwnd);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(700));
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let fresh = meta.modified().map(|m| m >= grace).unwrap_or(true);
            if !fresh {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if !text.contains("=== RESULT:") {
                continue;
            }
            log("-- 提权窗口的结论（读自 last-install.txt）--", hwnd);
            for line in text.lines() {
                log(line, hwnd);
            }
            return;
        }
    });
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
    hwnd: HWND,
}

/// 每格的节拍与宽度：标签 16 + 间隔 2 + 控件 24 + 行距 4。
const FIELD_PITCH: i32 = 46;
const FIELD_W: i32 = 320;
/// 两列的左边界。
const COL_X: [i32; 2] = [16, 360];

static FIELDS: Mutex<Vec<Field>> = Mutex::new(Vec::new());

/// 表单窗口。0 = 还没建过。关闭时只隐藏不销毁：重建十八个控件比 `ShowWindow`
/// 慢得多，而且人下次打开时改了一半的内容还在，不会白改。
static CONFIG_HWND: Mutex<isize> = Mutex::new(0);

/// 主窗口句柄。表单要把结果写进主窗口的日志面板，而 Win32 的窗口过程拿不到
/// "我是谁打开的"。存一份比 `GetWindow(hwnd, GW_OWNER)` 稳妥。
static MAIN_HWND: Mutex<isize> = Mutex::new(0);

/// 去掉一层外层引号：`'vp9'` → `vp9`，`'D:\logs\x.log'` → `D:\logs\x.log`。
///
/// 表单里显示的是"值"，不是"TOML 语法"。引号由 `configedit::format_value`
/// 在写回时按类型重新加，所以这里剥掉不会丢信息，也不怕路径里的反斜杠。
fn plain(raw: &str) -> &str {
    let text = raw.trim();
    if text.len() >= 2 {
        let bytes = text.as_bytes();
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            return &text[1..text.len() - 1];
        }
    }
    text
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
            hwnd: 0,
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
    let font = CreateFontW(
        -16,
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
    let mut fields = FIELDS.lock().unwrap_or_else(|e| e.into_inner());
    let rows = ((fields.len() + 1) / 2) as i32;
    for (i, field) in fields.iter_mut().enumerate() {
        let column = (i as i32) / rows;
        let row = (i as i32) % rows;
        let x = COL_X[column as usize];
        let y = 12 + row * FIELD_PITCH;
        // 标签
        let label = wide(&field.brief);
        let c = wide("STATIC");
        let child = CreateWindowExW(
            0,
            c.as_ptr(),
            label.as_ptr(),
            0 | WS_CHILD | WS_VISIBLE,
            x,
            y,
            FIELD_W,
            16,
            parent,
            0,
            instance,
            std::ptr::null(),
        );
        SendMessageW(child, WM_SETFONT, font as usize, 1);
        // 控件：枚举/布尔是下拉框，其余是文本框
        let (class, style) = if field.options.is_empty() {
            // ES_* 在 windows-sys 里是 i32，style 是 u32（WINDOW_STYLE）。
            ("EDIT", WS_BORDER | ES_AUTOHSCROLL as u32)
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
            x,
            y + 18,
            FIELD_W,
            if field.options.is_empty() { 24 } else { 200 },
            parent,
            (FIELD_BASE + i as i32) as isize,
            instance,
            std::ptr::null(),
        );
        SendMessageW(input, WM_SETFONT, font as usize, 1);
        let selected = field.original.as_deref().map(plain);
        if field.options.is_empty() {
            let w = wide(selected.unwrap_or(""));
            SetWindowTextW(input, w.as_ptr());
        } else {
            fill_combo(input, &field.options, selected);
        }
        field.hwnd = input;
    }
    rows
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
                let w = wide(selected.unwrap_or(""));
                SetWindowTextW(field.hwnd, w.as_ptr());
            } else {
                // 重新填一遍选项会把已有的删掉重来，这里只挪选中项就够了。
                let index = selected
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
            let was = field.original.as_deref().map(plain).unwrap_or("");
            if now == was {
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
                let handles: Vec<HWND> = slot.iter().map(|f| f.hwnd).collect();
                *slot = fields;
                for (field, hwnd) in slot.iter_mut().zip(handles) {
                    field.hwnd = hwnd;
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
            COL_X[1] + FIELD_W + 32,
            client_h + 39,
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
            let rows = build_config_controls(hwnd);
            let y = 12 + rows * FIELD_PITCH + 8;
            let instance = GetModuleHandleW(std::ptr::null());
            let font = CreateFontW(
                -16,
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
            let buttons: [(&str, i32, i32); 3] = [
                ("保存", BTN_CFG_SAVE, 100),
                ("重新读", BTN_CFG_RELOAD, 100),
                ("打开原文", BTN_CFG_RAW, 100),
            ];
            let mut x = COL_X[0];
            for (label, id, w) in buttons {
                let c = wide("BUTTON");
                let t = wide(label);
                let child = CreateWindowExW(
                    0,
                    c.as_ptr(),
                    t.as_ptr(),
                    (BS_PUSHBUTTON as u32) | WS_CHILD | WS_VISIBLE,
                    x,
                    y,
                    w,
                    30,
                    hwnd,
                    id as isize,
                    instance,
                    std::ptr::null(),
                );
                SendMessageW(child, WM_SETFONT, font as usize, 1);
                x += w + 8;
            }
            0
        }
        WM_COMMAND => {
            let id = (wparam & 0xffff) as i32;
            let main = *MAIN_HWND.lock().unwrap_or_else(|e| e.into_inner()) as HWND;
            match id {
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
            if !exe.exists() {
                set_state("找不到 stream-server.exe（本窗口要和它放在同一目录）", hwnd);
                return;
            }
            let (_, text) = run_capture(&exe, &["--ops", "state"]);
            set_state(&text, hwnd);
            log("-- 状态已刷新", hwnd);
        }
        BTN_SNAPSHOT => {
            if !exe.exists() {
                log("[fail] 找不到 stream-server.exe", hwnd);
                return;
            }
            spawn_job(hwnd, move || {
                let (code, text) = run_capture(&exe, &["--ops", "snapshot", "--ops-label", "manual"]);
                vec![format!("[exit {code}] 拍快照"), text.trim_end().to_owned()]
            });
        }
        BTN_ROLLBACK => {
            if !exe.exists() {
                log("[fail] 找不到 stream-server.exe", hwnd);
                return;
            }
            // 回滚会替换文件，必须问一句。文案里带"回滚本身也能退回去"是因为
            // 这决定了人敢不敢点。
            if !confirm(
                hwnd,
                "把程序文件退回到最近一次快照？\n\n当前状态会先自动留底，随时可以再退回来。",
                "回滚",
            ) {
                log("-- 已取消回滚", hwnd);
                return;
            }
            spawn_job(hwnd, move || {
                let (code, text) = run_capture(&exe, &["--ops", "rollback"]);
                vec![format!("[exit {code}] 回滚"), text.trim_end().to_owned()]
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
                    "更新完成：重启服务（安装 / 修复）后生效".to_owned()
                } else {
                    "更新没有完成；失败时会自动回滚，情况见上面的记录".to_owned()
                });
                lines
            });
        }
        BTN_INSTALL => {
            // 先记下时间点再拉起提权窗口：上一次安装留下的 OK 不能冒充这一次。
            let started = std::time::SystemTime::now();
            let path = install_result_path();
            if let Some(p) = &path {
                // 能删就删（提权写的文件普通用户删不掉，所以上面那个时间戳是
                // 真正的保险，这一行只是让文件干净一点）。
                let _ = std::fs::remove_file(p);
            }
            run_elevated(hwnd, "install.cmd", "/y");
            if let Some(p) = path {
                watch_install_result(hwnd, p, started);
            }
        }
        BTN_UNINSTALL => {
            if confirm(
                hwnd,
                "卸载会停服务、删防火墙规则、删整个目录。\n\n\
                 机器身份（配置/证书/录音设备原值）会先备份到\n\
                 %ProgramData%\\cloud-stream\\uninstall-backup\n\n继续？",
                "卸载",
            ) {
                run_elevated(hwnd, "uninstall.cmd", "/y");
            } else {
                log("-- 已取消卸载", hwnd);
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
            let font = CreateFontW(
                -16, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 0, 0,
                wide("Segoe UI").as_ptr(),
            );
            let parent = hwnd;
            let instance = GetModuleHandleW(std::ptr::null());
            // ES_* / BS_* 在 windows-sys 里是 i32，而 CreateWindowExW 的 style 是
            // u32（WINDOW_STYLE），所以这里统一抬成 u32 再拼。
            const READONLY_EDIT: u32 =
                (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32 | WS_BORDER | WS_VSCROLL;
            const BUTTON: u32 = BS_PUSHBUTTON as u32;
            let make = |class: &str, text: &str, style: u32, x: i32, y: i32, w: i32, h: i32, id: i32| -> HWND {
                let c = wide(class);
                let t = wide(text);
                let child = CreateWindowExW(
                    0,
                    c.as_ptr(),
                    t.as_ptr(),
                    style | WS_CHILD | WS_VISIBLE,
                    x,
                    y,
                    w,
                    h,
                    parent,
                    id as isize,
                    instance,
                    std::ptr::null(),
                );
                SendMessageW(child, WM_SETFONT, font as usize, 1);
                child
            };

            make(
                "STATIC",
                "程序文件",
                0,
                16,
                12,
                200,
                20,
                0,
            );
            make(
                "EDIT",
                "",
                READONLY_EDIT,
                16,
                34,
                748,
                130,
                ID_STATE,
            );

            // 七个按钮等宽排一行：中文字宽差不多的前提下，等宽比"按字数
            // 伸缩"更好看，也不会因为某个标签多了两个字就挤歪整排。
            // 顺序即操作顺序：看状态 → 留底 → 退回 → 更新 → 装 / 卸 / 改。
            let buttons: [(&str, i32); 7] = [
                ("刷新状态", BTN_REFRESH),
                ("拍快照", BTN_SNAPSHOT),
                ("回滚", BTN_ROLLBACK),
                ("更新", BTN_UPDATE),
                ("安装 / 修复", BTN_INSTALL),
                ("卸载", BTN_UNINSTALL),
                ("改配置", BTN_CONFIG),
            ];
            let mut x = 16;
            for (label, id) in buttons {
                make("BUTTON", label, BUTTON, x, 176, 100, 30, id);
                x += 108;
            }

            make("STATIC", "操作记录", 0, 16, 220, 200, 20, 0);
            make(
                "EDIT",
                "",
                READONLY_EDIT,
                16,
                242,
                748,
                210,
                ID_LOG,
            );

            SetWindowTextW(hwnd, wide("cloud-stream 控制台").as_ptr());
            let _ = font;
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
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn main() {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide("CloudStreamSetup");
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: 0,
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
            hIcon: 0,
            hCursor: LoadCursorW(0, 32512 as *const u16),
            hbrBackground: 16,
            lpszMenuName: std::ptr::null(),
            lpszClassName: config_class.as_ptr(),
        };
        RegisterClassW(&config_wc);

        let title = wide("cloud-stream 控制台");
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            796,
            484,
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
