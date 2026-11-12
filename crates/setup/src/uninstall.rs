//! 卸载的后端：把这台机器还原成"什么都没装"。
//!
//! # 六步，次序不是装饰
//!
//! ```text
//! 1. 计划任务 MCStreamSrv   先停、再删 —— 看门狗每分钟一次，先杀进程
//!                           就会有一个"它又被拉起来"的窗口
//! 2. stream-server.exe      只杀 exe 路径在这个目录下的
//! 3. 默认录音设备           服务启动时把它抢走了，且从不自己还回来
//! 4. 四条入站规则           名字是机器级的，删掉顺手清干净
//! 5. 机器身份               配置/证书/录音原值存到 %ProgramData%
//! 6. 目录本身               交给替身进程（见下）
//! ```
//!
//! # 谁在调它
//!
//! 两个入口，共用同一批 `step_*` 函数：
//!
//! * `--uninstall` —— 完整一条链（便携版、MSI 的 `--keep`）；
//! * `--uninstall-step <名字>` —— 只做一步，给 Inno 的卸载器逐条调用。
//!
//! 换言之：**编排可以在外面，知识只在这里**。Inno 那边写的是"先 task 再
//! procs"，这里写的是"task 到底怎么删"。
//!
//! # 为什么第 6 步要一个"替身"
//!
//! 删不掉自己：`stream-setting.exe` 正在跑，Windows 不会让任何人删掉一个正在执行
//! 的 exe（可以重命名，不能删除/覆盖）。也没有"退出前把自己删了"这种操作 ——
//! 进程退出时那条路径已经没人持有了，但也没有代码在跑了。
//!
//! 所以：把自己复制到 `%TEMP%`，用 `--purge --dir <目标> --wait <pid>` 拉起它，
//! 然后本进程退出。替身等这几个 pid 都消失之后才开始删目录 —— 那时整棵树里
//! 没有任何人持有句柄。
//!
//! **对话必须在交棒之前弹完**。反过来的话，本进程会停在 `MessageBox` 上等用户
//! 点确定，而替身正等着本进程退出 —— 用户只要读得慢一点，删除就会开始在一个
//! "exe 还被占着"的目录上进行，然后失败。
//!
//! # 删不掉怎么办
//!
//! 用户拍过板：**删，但失败就退回提示**。所以替身删不动时不会假装成功 ——
//! 它把失败的文件路径写进结论文件，弹一个框把这个路径交出去，然后退出。
//! 目录留着，用户自己决定。
//!
//! # 和 `uninstall.cmd` 的差别
//!
//! 只有两处，都是"原生能做到、cmd 做不到"的：
//!
//! * 端口不再假设 8080。问 `stream-server.exe --print-config json`，所以
//!   `cloud-stream 9000` 这种改过端口的机器也能把规则清干净；
//! * 任务删不掉时先 `/DISABLE` 再判断。禁用之后看门狗已经不可能拉起服务，
//!   所以"任务残留 + 目录照删"是安全的 —— cmd 那一版只能停在"任务还在且
//!   启用着"就拒绝继续。

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING,
};

use crate::install::{self, TASK_NAME};
use crate::report::{stamp, Report};
use crate::sys::{self, command, confirm, dialog, ElevateError, CREATE_NO_WINDOW};

// ---------------------------------------------------------------------------
// 参数
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    /// 主体：任务 → 进程 → 录音设备 → 规则 → 备身份 → 交棒。
    Main,
    /// 替身：等父进程退出，然后删目录。**不在 `Main` 里做**，因为 `Main` 自己
    /// 就在要被删的那棵树里。
    Purge,
}

#[derive(Clone, Debug)]
struct Opts {
    stage: Stage,
    /// 要卸掉哪个目录。`None` = 本 exe 所在目录（整包自包含，卸的就是自己）。
    dir: Option<PathBuf>,
    task: String,
    yes: bool,
    /// 保留文件（`--keep`）：任务/进程/规则都走，只留目录。
    keep_files: bool,
    /// 任务指向别的目录时也删掉它。
    force: bool,
    /// 只打印清单，什么都不动。
    whatif: bool,
    /// 把清单打到 stdout 而不是弹框（`--plan`）。`--whatif` 的机器可读版本：
    /// 字段里问"这台机器上卸载会做什么"时，需要一段能贴进聊天窗口的文本。
    plan: bool,
    /// 删目录之前要等这些进程退出（含本进程，由调用方传入）。
    wait: Vec<u32>,
    /// 不弹任何框（给外面那层安装器的 `[UninstallRun]` 用）。
    ///
    /// `--keep` 与它常一起出现，但它们是两件事：`--keep` 说"别删文件"
    /// （文件归外面那层删），`--quiet` 说"别弹框"（外面那层的卸载页在负责
    /// 告诉用户发生了什么）。静默卸载里一个 MessageBox 会让进程一直等。
    quiet: bool,
    /// `--uninstall-step <名字>`：只做五步里的一步（`task` / `procs` / `mic` /
    /// `firewall` / `identity`）。
    ///
    /// 给 Inno 的卸载器用：它在 `[Code]` 里按顺序逐条调，于是卸载进度页能一条
    /// 一条报出来，而不是"停在那里十几秒"。**编排在卸载器，知识在这里** ——
    /// 这是刻意划的那条线（见 `step_*` 上面那段注释）。
    step: Option<String>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut opts = Opts {
        stage: Stage::Main,
        dir: None,
        task: TASK_NAME.to_owned(),
        yes: false,
        keep_files: false,
        force: false,
        whatif: false,
        plan: false,
        wait: Vec::new(),
        quiet: false,
        step: None,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--uninstall" => opts.stage = Stage::Main,
            "--purge" => opts.stage = Stage::Purge,
            "--dir" => {
                let value = it.next().ok_or("--dir 后面要跟一个路径")?;
                // 同 `install.rs`：MSI 传下来的目录带 `\.`（`[INSTALLDIR]` 自带
                // 结尾反斜杠，点是为了躲引号转义）。这里同样按"路径就是路径"
                // 归一化，删目录时才能和装的时候落在同一个字符串上。
                opts.dir = Some(sys::normalize_dir(value));
            }
            "--task" => {
                let value = it.next().ok_or("--task 后面要跟一个任务名")?;
                opts.task = value.clone();
            }
            "--wait" => {
                let value = it.next().ok_or("--wait 后面要跟一个 pid")?;
                let pid: u32 = value
                    .parse()
                    .map_err(|_| format!("--wait 要一个数字，拿到的是 {value:?}"))?;
                opts.wait.push(pid);
            }
            "--yes" | "-y" => opts.yes = true,
            "--keep" => opts.keep_files = true,
            "--force" => opts.force = true,
            "--whatif" => opts.whatif = true,
            "--plan" => opts.plan = true,
            "--quiet" | "--silent" => opts.quiet = true,
            "--uninstall-step" => {
                let value = it.next().ok_or("--uninstall-step 后面要跟一个步骤名")?;
                opts.step = Some(value.clone());
                // 单步永远不弹框：调用方是卸载器，它背后没有人点"确定"。
                opts.quiet = true;
            }
            // 其余忽略：窗口会把"我们这个 exe 的参数"整串传下来。
            _ => {}
        }
    }
    Ok(opts)
}

/// 入口。返回进程退出码。
pub fn run(args: &[String]) -> i32 {
    // 解析失败时还不知道 `--quiet`，先从原始参数里探一次 —— 外面那层
    // 安装器的卸载页正等着这个进程返回，弹框会把它卡住。
    if args.iter().any(|a| a == "--quiet" || a == "--silent") {
        sys::set_quiet(true);
    }
    let opts = match parse(args) {
        Ok(opts) => opts,
        Err(msg) => {
            dialog(&msg, "卸载参数", MB_ICONERROR);
            return 2;
        }
    };
    sys::set_quiet(opts.quiet);
    if let Some(step) = opts.step.clone() {
        return run_step(&opts, &step);
    }
    match opts.stage {
        Stage::Main => main_stage(&opts),
        Stage::Purge => purge_stage(&opts),
    }
}

/// 只做一步。给外面那层卸载器用（见 `Opts::step`）。
///
/// 三条约定，都是为了让"卸载器编排"这件事可预测：
///
/// * **不弹框、不等确认**：解析参数时就把 `quiet` 打开了；
/// * **不删文件**：删文件是卸载器的活（它按自己的安装日志删），这里只回收
///   系统集成；
/// * **退出码只说明这一步**：0 = 做完了（哪怕本来是"没有这个任务"），非 0 =
///   没做成。`task` 那一步"删不掉也禁不掉"仍然是失败 —— 看门狗会把服务拉起来，
///   而卸载器马上就要删它指向的 exe。
fn run_step(opts: &Opts, name: &str) -> i32 {
    let dir = opts.dir.clone().unwrap_or_else(crate::run_root);
    let report = Report::new();
    // `append` 而不是 `start`：六步是六个进程，共用 `last-install.txt`，
    // 截断的话只剩最后一步的记录（见 `Report::append` 的注释）。
    report.append(&dir, &format!("uninstall-step {name}"));
    report.note(&format!("elevated: {}", sys::elevated()));
    report.note(&format!("dir: {}", dir.display()));

    // 单步命令同样删任务、杀进程，所以照样过一遍"绝对不该碰的目录"。
    // 正常永远过（外面传的是它自己装的 {app}）；防的是手抖的 `--dir C:\`。
    if let Some(why) = absolutely_not(&dir) {
        report.note(&format!("  [skip] {why}"));
        report.end("SKIP");
        println!("skip: {why}");
        return 0;
    }

    let ok = match name {
        "task" => {
            let (ok, line) = task_outcome(&step_task(&dir, &opts.task, opts.force, &report));
            report.note(&line);
            ok
        }
        "procs" => {
            step_procs(&dir, &report);
            true
        }
        "mic" => {
            step_mic(&dir, &report);
            true
        }
        "firewall" => {
            step_firewall(&dir, &report);
            true
        }
        "identity" => {
            step_identity(&dir, &report);
            true
        }
        other => {
            report.note(&format!("  [fail] 不认识的步骤 {other}"));
            report.end("FAIL (unknown step)");
            eprintln!("不认识的步骤：{other}（可选 task/procs/mic/firewall/identity）");
            return 2;
        }
    };

    report.end(if ok { "OK" } else { "FAIL" });
    if ok {
        0
    } else {
        1
    }
}

// ---------------------------------------------------------------------------
// 主体
// ---------------------------------------------------------------------------

fn main_stage(opts: &Opts) -> i32 {
    let dir = opts.dir.clone().unwrap_or_else(crate::run_root);

    // `--plan`：把清单打到 stdout，不弹框、不动任何东西。走 stdout 是因为
    // 它和 `--print-config` / `--task-xml` 是一类东西 —— 结果本身就是答案，
    // 管道能接、文件能存、能贴进聊天窗口。
    if opts.plan {
        if let Some(reason) = refuse_reason(&dir, opts.force) {
            println!("拒绝卸载：{}\n{reason}", dir.display());
            return 0;
        }
        println!("{}", inventory(&dir, &opts.task));
        return 0;
    }

    // 目录不在的时候不早退：任务和规则可能还在（上一次半途而废）。一个还活着的
    // 看门狗会不停拉起一个指向空目录的服务，症状是"卸载完了服务又活了"。
    if dir.is_dir() {
        if let Some(reason) = refuse_reason(&dir, opts.force) {
            dialog(
                &format!(
                    "拒绝从这个目录卸载：\n{}\n\n{reason}\n\n\
                     如果你确定要删它，请手工删除。",
                    dir.display()
                ),
                "卸载",
                MB_ICONERROR,
            );
            return 2;
        }
    }

    let plan = inventory(&dir, &opts.task);
    if opts.whatif {
        dialog(&plan, "卸载（预演，什么都没动）", MB_ICONINFORMATION);
        return 0;
    }
    if !opts.yes
        && !confirm(
            &format!("{plan}\n\n继续卸载？"),
            if opts.keep_files {
                "卸载（保留文件）"
            } else {
                "卸载"
            },
        )
    {
        return 0;
    }

    // 任务与防火墙两项非管理员不可。解压那一段在安装时之所以要用户令牌，是因为
    // `%LOCALAPPDATA%` 会指错家；卸载时目标目录是显式给的，所以整段提权没问题。
    if !sys::elevated() {
        let mut params = format!("--uninstall --yes --dir \"{}\"", dir.display());
        if opts.task != TASK_NAME {
            params.push_str(&format!(" --task \"{}\"", opts.task));
        }
        if opts.keep_files {
            params.push_str(" --keep");
        }
        if opts.force {
            params.push_str(" --force");
        }
        for pid in &opts.wait {
            params.push_str(&format!(" --wait {pid}"));
        }
        return match sys::elevate_raw(&params, &dir) {
            Ok(()) => 0,
            Err(ElevateError::Denied) => {
                dialog(
                    "UAC 被拒绝了 —— 什么都没动。\n\n\
                     卸载要删计划任务和防火墙规则，这两件事非管理员不可。",
                    "卸载",
                    MB_ICONWARNING,
                );
                5
            }
            Err(ElevateError::Failed(code)) => {
                dialog(
                    &format!("起不了管理员窗口（ShellExecute={code}）。"),
                    "卸载",
                    MB_ICONERROR,
                );
                4
            }
        };
    }

    steps(opts, &dir)
}

/// 提权之后的五步。返回进程退出码。
// ---------------------------------------------------------------------------
// 五步：每一步一个函数
// ---------------------------------------------------------------------------
//
// 拆成函数而不是一段流水账，是因为每一步有**两个**调用方：
//
// * `steps()` —— 完整卸载（便携版、MSI 走这条：`--uninstall`）；
// * `run_step()` —— 只做一步（`--uninstall-step <名字>`），给 Inno 的卸载器用。
//   它自己在 `[Code]` 里按顺序调这五条，于是卸载进度页能逐条报出来。
//
// 这是"回收器并入卸载器"和"不要第二套实现"能同时成立的地方：卸载器拿走的是
// **编排**（先停什么、什么算致命），不是**知识**（任务怎么删、规则叫什么名字、
// 录音设备原值存在哪）。知识仍然只有这一份，而且还能被单测。

/// 第 1 步：停并删看门狗任务。返回的 `TaskEnd` 由调用方决定"算不算失败" ——
/// `steps()` 把 `Stuck` 当致命（看门狗会把服务拉起来），单步调用也一样。
fn step_task(dir: &Path, task: &str, force: bool, report: &Report) -> TaskEnd {
    stop_task(task, dir, force, report)
}

/// 第 2 步：停掉这个目录下的 `stream-server.exe`，以及开在这个目录里的控制台。
///
/// 顺序不能反：先收服务，再收控制台。控制台是 `{app}` 里正在执行的那个 exe，
/// 它不关，Inno 最后删目录那一步就会失败（见 `APP_EXES` 的注释）。
fn step_procs(dir: &Path, report: &Report) {
    let killed = kill_servers_under(dir, Some(report));
    if killed == 0 {
        report.note("  [ok] 这个目录下没有在跑的实例");
    }
    kill_consoles_under(dir, report);
    // 给了时间还不退，就别假装删得动。
    if !wait_no_server(dir, 30) {
        report.note("  [warn] 还有进程没退干净，后面删目录可能会失败");
    }
}

/// 第 3 步：把默认录音设备还回安装前的那个。
fn step_mic(dir: &Path, report: &Report) {
    restore_mic(dir, report);
}

/// 第 4 步：删四条入站规则。返回还剩几条（不致命，策略可能持有它们）。
fn step_firewall(dir: &Path, report: &Report) -> usize {
    let left = close_inbound(dir, report);
    if left == 0 {
        report.note("  [ok] 四条规则都清掉了");
    } else {
        // 不是致命错误：策略或第三方防火墙可能管着这些规则，而机器还是要卸。
        report.note(&format!(
            "  [warn] {left} 条规则还在（域策略或第三方防火墙持有），按名字在 Windows 防火墙里删"
        ));
    }
    left
}

/// 第 5 步：把这台机器的身份存到目录外面。
///
/// 文件马上就要被删（自己删，或者 Inno 按安装日志删），而"配置/证书/录音
/// 设备原值"是重装时唯一想留下来的东西 —— 重装后 `--ops rollback` 会放回去。
///
/// 放在 `--keep` 分支之前是有意的：`--keep` 说的是"文件归别人删"，不该顺手
/// 把身份也丢掉。
fn step_identity(dir: &Path, report: &Report) {
    save_identity(dir, report);
}

/// `TaskEnd` → "继续不继续 + 一句人话"。`steps()` 与 `run_step()` 共用，
/// 免得两处对"任务指向别的目录算不算失败"给出不同答案。
fn task_outcome(end: &TaskEnd) -> (bool, String) {
    match end {
        TaskEnd::Absent => (true, "  [ok] 没有这个任务".to_owned()),
        TaskEnd::Removed => (true, "  [ok] 任务已删除".to_owned()),
        TaskEnd::Disabled(note) => (true, format!("  [warn] {note}")),
        TaskEnd::Foreign(what) => (
            true,
            format!(
                "  [skip] 任务指向另一个目录，留着不动：\n         {what}\n         在那边再卸载一次才能删掉它"
            ),
        ),
        TaskEnd::Stuck(msg) => (false, format!("  [fail] {msg}")),
    }
}

fn steps(opts: &Opts, dir: &Path) -> i32 {
    let report = Report::new();
    report.start(
        dir,
        if opts.keep_files {
            "uninstall-keep"
        } else {
            "uninstall"
        },
    );
    report.note(&format!("elevated: {}", sys::elevated()));

    // ---- 1. 计划任务 -------------------------------------------------------
    report.note(&format!("[1/6] 看门狗任务 {}", opts.task));
    let (ok, line) = task_outcome(&step_task(dir, &opts.task, opts.force, &report));
    report.note(&line);
    if !ok {
        report.end("FAIL (task)");
        // 到这儿就停：看门狗会每分钟把服务拉起来，而它指向的 exe 正在被删。
        dialog(
            &format!(
                "{line}\n\n\
                 它的看门狗每分钟拉起一次服务，所以后面的目录删不干净。\n\
                 这一步之前什么都没动。\n\n\
                 请在「任务计划程序」(taskschd.msc) 里删掉 {} 再试一次。",
                opts.task
            ),
            "卸载",
            MB_ICONERROR,
        );
        return 1;
    }

    // ---- 2. 进程 -----------------------------------------------------------
    report.note("[2/6] 停 stream-server.exe");
    step_procs(dir, &report);

    // ---- 3. 默认录音设备 ---------------------------------------------------
    report.note("[3/6] 还回默认录音设备");
    step_mic(dir, &report);

    // ---- 4. 入站规则 -------------------------------------------------------
    report.note("[4/6] 删入站防火墙规则");
    step_firewall(dir, &report);

    // ---- 5. 身份 -----------------------------------------------------------
    // 放在 keep 分支**之前**：`--keep` 是"文件归别人删"，而身份是重装时唯一
    // 想留住的东西 —— 谁删文件都不该顺手把它删掉。MSI 的卸载也走 --keep，
    // 所以这一步对它同样生效（此前那条路是漏的）。
    report.note("[5/6] 备份机器身份");
    step_identity(dir, &report);

    // ---- 6. 目录 -----------------------------------------------------------
    if opts.keep_files {
        report.note("[6/6] 保留文件（--keep）");
        report.end("OK (files kept)");
        dialog(
            &format!(
                "服务、任务、规则都处理完了，文件保留在：\n{}\n\n\
                 文件由安装器删（`--keep` 的语义就是这个）。",
                dir.display()
            ),
            "卸载完成（保留文件）",
            MB_ICONINFORMATION,
        );
        return 0;
    }

    report.note(&format!("[6/6] 删目录 {}", dir.display()));

    // 这一段必须写在**交棒之前**：交棒之后本进程只能退出，没有地方再弹框。
    let mut text = format!(
        "服务、任务、防火墙规则都处理完了。\n\n\
         正在删除目录（后台进行，几秒）：\n{}\n\n",
        dir.display()
    );
    if let Some(backup) = appdata_backup() {
        if backup.is_dir() {
            text.push_str(&format!(
                "机器身份已备份到：\n{}\n重装时会自动放回去。\n\n",
                backup.display()
            ));
        }
    }
    text.push_str(
        "如果几秒后目录还在，会有一个提示框告诉你删不掉的文件 —— \
         那种情况需要手工删掉它（多半是有程序还开着里面的文件）。",
    );
    dialog(&text, "卸载", MB_ICONINFORMATION);

    if let Err(err) = spawn_purge(dir, opts, &report) {
        report.note(&format!("  [fail] {err}"));
        report.end("FAIL (purge-spawn)");
        dialog(
            &format!(
                "起不了负责删目录的进程：\n\n{err}\n\n\
                 目录还在：{}\n手工删掉它即可（服务、任务、规则都已经清理）。",
                dir.display()
            ),
            "卸载",
            MB_ICONERROR,
        );
        return 6;
    }
    report.note("  [ok] 替身进程已启动，等本进程退出后删目录");
    report.end("PENDING (目录由替身删除)");
    0
}

/// 干活的清单。`/whatif` 与确认框共用同一段文本 —— 说了要做什么，就做什么。
fn inventory(dir: &Path, task: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("目录：{}\n任务：{task}\n\n", dir.display()));

    match lookup_task(task) {
        TaskLookup::Present(found) => match found.xml.as_deref().and_then(task_command) {
            Some(cmd) if path_under(Path::new(&cmd), dir) => {
                out.push_str(&format!("[x] 计划任务      {cmd}\n"))
            }
            Some(cmd) => out.push_str(&format!("[ ] 计划任务      指向别处，不动：{cmd}\n")),
            None => out.push_str("[x] 计划任务      在（读不出动作，按本目录处理）\n"),
        },
        TaskLookup::Absent => out.push_str("[ ] 计划任务      不存在\n"),
    }

    let running = server_pids_under(dir).len();
    out.push_str(&if running == 0 {
        "[ ] 服务进程      没在跑\n".to_owned()
    } else {
        format!("[x] 服务进程      {running} 个\n")
    });
    out.push_str("[x] 录音设备      还回安装前的那个\n");
    out.push_str(&format!("[x] 防火墙规则    4 条\n"));
    if dir.is_dir() {
        out.push_str(&format!("[x] 目录          {}\n", dir.display()));
    } else {
        out.push_str("[ ] 目录          已经不在\n");
    }
    out
}

// ---------------------------------------------------------------------------
// 1. 计划任务
// ---------------------------------------------------------------------------

enum TaskEnd {
    Absent,
    Removed,
    /// 还在，但已禁用 —— 看门狗拉不起服务，可以继续。
    Disabled(String),
    /// 指向另一个目录，按规矩不碰它。
    Foreign(String),
    /// 删不掉也禁不掉：必须停手。
    Stuck(String),
}

fn stop_task(task: &str, dir: &Path, force: bool, report: &Report) -> TaskEnd {
    let lookup = lookup_task(task);
    let TaskLookup::Present(found) = lookup else {
        return TaskEnd::Absent;
    };
    match &found.xml {
        Some(xml) => {
            if let TaskOwner::Foreign(cmd) = task_owner(xml, dir) {
                if !force {
                    return TaskEnd::Foreign(cmd);
                }
                report.note(&format!("  [i] --force：任务的执行路径是 {cmd}，仍然删掉"));
            }
        }
        // 任务文件在，但内容读不出来（权限，或者哪天格式变了）。不猜"不是我的"
        // —— 那个方向的代价是留下一个看门狗，它在下一分钟把服务拉起来。
        None => report.note("  [warn] 读不出任务内容，按本目录的任务处理"),
    }

    // 先 `/End` 再 `/Delete`：`/Delete` 会拒绝一个还在运行的实例
    // （0x8004131F），而看门狗那一分钟内可能正好有一轮在跑。
    let _ = command("schtasks", &["/End", "/TN", task]);
    let kill = command("schtasks", &["/Delete", "/TN", task, "/F"]);
    if found.gone(task) {
        return TaskEnd::Removed;
    }
    // 删不掉。禁用之后它就再也拉不起服务了 —— 这一步让"任务残留 + 目录照删"
    // 变成安全的，而 cmd 那一版只能在这儿拒绝继续。
    let _ = command("schtasks", &["/Change", "/TN", task, "/DISABLE"]);
    if lookup_task(task)
        .xml()
        .map(|xml| settings_disabled(&xml))
        .unwrap_or(false)
    {
        return TaskEnd::Disabled(format!(
            "任务 {task} 删不掉，已禁用（看门狗不会再拉起服务）；方便时在 taskschd.msc 里删掉它"
        ));
    }
    // 禁用也没成功：看门狗还活着，而目录马上要被删。停手。
    let why = match kill {
        Ok(text) if !text.trim().is_empty() => text.trim().replace('\n', " "),
        Ok(_) => "schtasks 没有输出".to_owned(),
        Err(err) => err,
    };
    TaskEnd::Stuck(format!(
        "任务 {task} 既删不掉也禁不掉（{why}）。详细经过：{}",
        report
            .path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "结论文件".to_owned())
    ))
}

/// 任务在磁盘上的样子。
///
/// 任务计划程序把每个任务存成 `%SystemRoot%\System32\Tasks\<名字>`，内容是
/// UTF-16LE 的 XML。**这里优先读这个文件，而不是 `schtasks /Query /XML`**：
///
/// * 不要子进程（也就不受"schtasks 被策略拦着"影响 —— 这一条是真事，
///   开发机上 schtasks.exe 就在安全策略的程序黑名单里）；
/// * 不受控制台代码页影响。`schtasks` 的输出走 OEM 代码页（中文系统上是
///   GBK），转成 UTF-8 时非 ASCII 的路径会变成替换字符，于是"这条任务是我
///   的吗"没法回答；
/// * 存在性就是文件在不在，比解析一行本机化的英文/中文提示可靠。
///
/// `schtasks` 仍然是**动手**的那一个（`/Delete`、`/Change /DISABLE`）：写文件
/// 是它的活儿，而读文件是只读的、看得懂就够。
struct TaskFound {
    /// 任务文件。`None` = 连 `%SystemRoot%` 都拿不到（那就只能问 schtasks）。
    file: Option<PathBuf>,
    /// 解码后的 XML。`None` = 文件在但读不出来。
    xml: Option<String>,
}

impl TaskFound {
    /// 这个任务是不是已经没了。
    fn gone(&self, task: &str) -> bool {
        match &self.file {
            Some(file) => !file.exists(),
            None => lookup_task(task).xml().is_none(),
        }
    }
}

enum TaskLookup {
    Absent,
    Present(TaskFound),
}

impl TaskLookup {
    fn xml(&self) -> Option<String> {
        match self {
            TaskLookup::Present(found) => found.xml.clone(),
            TaskLookup::Absent => None,
        }
    }
}

/// 看门狗任务现在是什么状态。给「程序信息」面板用（`main.rs`）。
///
/// 和 `stop_task` 里那套判断同源（都是 `lookup_task` + `settings_disabled`），
/// 所以"面板说已禁用"和"卸载时走 `Disabled` 分支"一定是同一件事。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TaskState {
    /// 没有这个任务。
    Absent,
    /// 在，正常启用。
    Enabled,
    /// 在，但被 `schtasks /Change /DISABLE` 关过 —— 删不掉任务时的兜底，
    /// 看门狗已经拉不起服务。
    Disabled,
    /// 文件在，但内容读不出来（权限，或者哪天格式变了）。
    Unknown,
}

pub(crate) fn task_state(task: &str) -> TaskState {
    match lookup_task(task) {
        TaskLookup::Absent => TaskState::Absent,
        TaskLookup::Present(found) => match &found.xml {
            Some(xml) if settings_disabled(xml) => TaskState::Disabled,
            Some(_) => TaskState::Enabled,
            None => TaskState::Unknown,
        },
    }
}

fn lookup_task(task: &str) -> TaskLookup {    if let Some(file) = task_file(task) {
        if !file.exists() {
            return TaskLookup::Absent;
        }
        let xml = read_task_xml(&file);
        return TaskLookup::Present(TaskFound {
            file: Some(file),
            xml,
        });
    }
    // 拿不到 %SystemRoot%：退回 schtasks（此时编码问题无解，只能尽力）。
    match command("schtasks", &["/Query", "/TN", task, "/XML"]) {
        Ok(text) if text.contains("<Task") => TaskLookup::Present(TaskFound {
            file: None,
            xml: Some(text),
        }),
        _ => TaskLookup::Absent,
    }
}

/// `%SystemRoot%\System32\Tasks\<名字>`。
fn task_file(task: &str) -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot")?;
    let root = PathBuf::from(root);
    if root.as_os_str().is_empty() {
        return None;
    }
    Some(root.join("System32").join("Tasks").join(task))
}

/// 读任务文件。UTF-16LE（带 BOM）是实测的格式，也接受 UTF-8 以防哪天变了。
fn read_task_xml(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let units = |body: &[u8], le: bool| -> String {
        let units: Vec<u16> = body
            .chunks_exact(2)
            .map(|c| {
                if le {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(body) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Some(units(body, true));
    }
    if let Some(body) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return Some(units(body, false));
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

enum TaskOwner {
    Mine,
    /// 指向别处，带它读出来的那条命令。
    Foreign(String),
}

fn task_owner(xml: &str, dir: &Path) -> TaskOwner {
    let Some(cmd) = task_command(xml) else {
        // 读不出 `<Command>`：可能是编码问题，也可能这份 XML 结构不一样。
        // 当成自己的 —— 因为"不是自己的"这个判断的代价是留下一个看门狗，
        // 它在下一分钟把服务拉起来，然后在删目录时失败。
        return TaskOwner::Mine;
    };
    if path_under(Path::new(&cmd), dir) || !cmd.is_ascii() {
        return TaskOwner::Mine;
    }
    TaskOwner::Foreign(cmd)
}

/// 从任务 XML 里取 `<Command>` 的文本，并按 XML 规则还原实体。
fn task_command(xml: &str) -> Option<String> {
    let text = element_text(xml, "Command")?;
    Some(xml_unescape(&text))
}

/// 取第一个 `<tag>...</tag>` 的文本。找不到就是 `None`。
///
/// 只做这一件事的 XML 解析器：这份文档是我们自己写的（或系统导出的），结构
/// 固定，而引一个 XML 库只为了取一个元素并不划算。
fn element_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_owned())
}

fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        // `&amp;` 必须最后换：先换它会把 `&amp;lt;` 变成 `&lt;` 再变成 `<`。
        .replace("&amp;", "&")
}

/// 任务是不是被禁用了。
///
/// 实测（`Export-ScheduledTask` 对照）：**启用**的任务导出时**没有** `<Enabled>`
/// 元素；被 `Disable-ScheduledTask` 之后才会出现 `<Enabled>false</Enabled>`。
/// 所以判据不是"读到 false"，而是"设置块里有没有它"——
/// `<Enabled>true</Enabled>` 出现在 `<Triggers>` 里，不能整体搜。
fn settings_disabled(xml: &str) -> bool {
    let Some(settings) = element_text(xml, "Settings") else {
        return false;
    };
    settings.contains("<Enabled>false</Enabled>")
}

// ---------------------------------------------------------------------------
// 2. 进程
// ---------------------------------------------------------------------------

/// 这个目录下所有 `stream-server.exe` 的 pid。
///
/// 按**完整路径**过滤是全部要点：同一台机器上另一个目录里的安装不该被这一个
/// 卸载掉。任务管理器里两个同名 exe 长得一模一样，只有路径能分开。
pub(crate) fn server_pids_under(dir: &Path) -> Vec<u32> {
    pids_under(dir, &["stream-server.exe"])
}

/// 同上，但按调用方给的名字表收。名字为空时什么也不返回。
fn pids_under(dir: &Path, names: &[&str]) -> Vec<u32> {
    let mut out = Vec::new();
    if names.is_empty() {
        return out;
    }
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                let name = wide_z(&entry.szExeFile);
                if names.iter().any(|n| name.eq_ignore_ascii_case(n)) {
                    if let Some(path) = image_path(entry.th32ProcessID) {
                        if path_under(&path, dir) {
                            out.push(entry.th32ProcessID);
                        }
                    }
                }
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

/// 从 pid 拿完整的 exe 路径。
///
/// `PROCESS_QUERY_LIMITED_INFORMATION` 是能拿到路径的最小权限 —— 只要这个，
/// 不要 `PROCESS_QUERY_INFORMATION`（那个要的权限高得多，提权进程能过，
/// 普通用户对别的用户的进程过不了）。
unsafe fn image_path(pid: u32) -> Option<PathBuf> {
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if handle == 0 {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len);
    CloseHandle(handle);
    if ok == 0 || len == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(
        &buf[..len as usize],
    )))
}

/// 这个目录下"开着的窗口"，它们会挡住删目录。
///
/// `stream-setting.exe` 是工具箱（装好之后那一份就在 `{app}` 里）；
/// `cloud-setup.exe` 是 2026-11 改名之前的名字，老机器上可能还开着一个。
///
/// 为什么控制台也要收：`{app}` 里**正在执行的文件删不掉**（Windows 的影像段
/// 不带 `FILE_SHARE_DELETE`）。用户从工具箱点"卸载"的那一刻，那个窗口就开在
/// `{app}\stream-setting.exe` 上，于是 Inno 走到删目录时拿到共享冲突，日志里
/// 只留一句 `Failed to delete directory (32). Will retry later.`，然后 ARP 清空
/// 了、任务没了、规则也清了，可目录还在 —— 实机撞到过一次，里面只剩一个
/// `unins000.exe` 和一个空目录，看起来像"卸载器不会删目录"，其实是"我们自己
/// 那个窗口没关"。
const CONSOLE_EXES: &[&str] = &["stream-setting.exe", "cloud-setup.exe"];

/// 收掉这个目录下的 `stream-server.exe`，返回真正结束掉的个数。
///
/// `pub(crate)`：卸载的第 2 步和「停止服务」用的是同一段代码（`service::stop`）。
/// 两处各写一遍"按路径枚举再 TerminateProcess"没有意义 —— 早晚有一处忘了
/// 跳过自己的 pid。
pub(crate) fn kill_servers_under(dir: &Path, report: Option<&Report>) -> usize {
    kill_pids_under(dir, &["stream-server.exe"], report)
}

/// 结束同目录下的控制台，**不包括自己**。
///
/// 不含自己这条是必须的：这一步本身就是
/// `{app}\stream-setting.exe --uninstall-step procs` 在跑，把自己杀了，
/// 后面三步（还录音设备 / 删规则 / 备份身份）就没人做了。
///
/// 副作用要提前说清楚：用户点"卸载"的那个窗口会在这里消失。卸载期间界面归
/// 卸载器，窗口留着只会挡住它要删的文件。
fn kill_consoles_under(dir: &Path, report: &Report) -> usize {
    let killed = kill_pids_under(dir, CONSOLE_EXES, Some(report));
    if killed > 0 {
        report.note("  [ok] 关掉了同目录下的控制台窗口（它正开着 {app} 里的 exe，会挡住删目录）");
    }
    killed
}

/// 按名字 + 完整路径收进程。`names` 之外的一律不动。
fn kill_pids_under(dir: &Path, names: &[&str], report: Option<&Report>) -> usize {
    let me = std::process::id();
    let mut killed = 0;
    for pid in pids_under(dir, names) {
        // 自己不能杀：那会让调用方（正是一次 `--uninstall-step`）当场消失。
        if pid == me {
            continue;
        }
        let ok = unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle == 0 {
                false
            } else {
                let rc = TerminateProcess(handle, 1);
                CloseHandle(handle);
                rc != 0
            }
        };
        match (ok, report) {
            (true, Some(report)) => {
                killed += 1;
                report.note(&format!("  [ok] 结束进程 {pid}"));
            }
            (false, Some(report)) => {
                report.note(&format!("  [warn] 结束不了进程 {pid}（可能已经退出）"))
            }
            (true, None) => killed += 1,
            (false, None) => {}
        }
    }
    killed
}

/// 等这个目录下再没有 `stream-server.exe`。返回 false = 超时。
fn wait_no_server(dir: &Path, seconds: u32) -> bool {
    for _ in 0..seconds * 4 {
        if server_pids_under(dir).is_empty() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

// ---------------------------------------------------------------------------
// 3. 默认录音设备
// ---------------------------------------------------------------------------

/// 还给用户"安装前那个"录音设备。
///
/// 服务启动时把默认录音设备换成虚拟声卡，而且**被结束的时候从来不会被问一句**
/// —— 所以原值是这个卸载流程唯一能还回去的地方。原值记在
/// `logs\mic-default.json`，那个文件马上要跟着目录一起消失。
fn restore_mic(dir: &Path, report: &Report) {
    let exe = dir.join("stream-server.exe");
    if !exe.exists() {
        report.note("  [skip] 没有 stream-server.exe，没什么可还");
        return;
    }
    // 能力探测在**执行之前**。旧版本 exe 不认识 `--restore-mic`，
    // `Config::parse` 会忽略不认识的参数，于是它起的是一个完整的服务：永不返回、
    // 占住端口、顺手把录音设备再抢一遍。那正是"卡在第三步"的成因。
    if !sys::exe_has_marker(&exe, "MICRESTORE") {
        report.note(
            "  [skip] 这个 exe 是加 --restore-mic 之前的版本；\
             运行它只会起一个完整服务，所以不运行",
        );
        keep_mic_record(dir, report);
        return;
    }

    // 20 秒：exe 自己保证 5 秒内退（见服务端 main.rs），这里只是不让"它没退"
    // 变成一个永远等下去的卸载。
    match sys::run_bounded(&exe, &["--restore-mic"], dir, 20) {
        Ok((code, text)) => {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                report.note(&format!("    {line}"));
            }
            // 成功必须被正面观测到。"不是超时、也不是没还原"这句话对"打了个
            // 照面就死掉的助手"同样成立，而机器会留在虚拟声卡上，日志看着却
            // 很干净。
            let done = text.contains("MICRESTORE restored")
                || text.contains("MICRESTORE nothing-to-do");
            if done {
                report.note("  [ok] 默认录音设备已还原");
            } else {
                report.note(&format!("  [warn] 没还原（退出码 {code}）"));
                keep_mic_record(dir, report);
            }
        }
        Err(err) => {
            report.note(&format!("  [warn] 起不了 --restore-mic：{err}"));
            keep_mic_record(dir, report);
        }
    }
}

/// 目录和记录文件都要没了，把记录复制到 `%TEMP%` —— 那是"原本是哪台设备"
/// 的最后一个证据。
fn keep_mic_record(dir: &Path, report: &Report) {
    let src = dir.join("logs").join("mic-default.json");
    if !src.exists() {
        report.note("      没有 logs\\mic-default.json，可能从来没被接管过");
        return;
    }
    let dst = std::env::temp_dir().join("cloud-stream-mic-default.json");
    match std::fs::copy(&src, &dst) {
        Ok(_) => report.note(&format!(
            "      原本是哪台设备记在这儿：{}\n\
             在「设置 → 系统 → 声音」里手工选回去",
            dst.display()
        )),
        Err(err) => report.note(&format!("      复制录音设备记录失败：{err}")),
    }
}

// ---------------------------------------------------------------------------
// 4. 防火墙
// ---------------------------------------------------------------------------

/// 删入站规则，返回还剩几条。
///
/// 端口问服务端自己（`--print-config json`），不在这里假设 8080 ——
/// `open-inbound.cmd` 里"40000-40049 必须和默认值同步"那句注释，就是第二套
/// 判断迟早分叉的自白。8080 那个历史名字顺手也删（改过端口的机器会留着它）。
fn close_inbound(dir: &Path, report: &Report) -> usize {
    let exe = dir.join("stream-server.exe");
    let port = if exe.exists() {
        install::effective_config(&exe).port
    } else {
        8080
    };

    let mut names = vec![format!("cloud-stream {port}")];
    if port != 8080 {
        names.push("cloud-stream 8080".to_owned());
    }
    names.extend(
        [
            install::RULE_ICMP,
            install::RULE_UDP,
            install::RULE_APP,
        ]
        .map(str::to_owned),
    );

    for name in &names {
        let _ = sys::netsh(&format!("advfirewall firewall delete rule name=\"{name}\""));
    }
    // "Windows 已阻止此应用的部分功能"留下的 BLOCK 规则：BLOCK 优先于 allow，
    // 留着就是"规则看着都在、一个包都过不去"。
    let _ = sys::netsh(&format!(
        "advfirewall firewall delete rule name=all dir=in program=\"{}\"",
        exe.display()
    ));

    // 回读核对。`netsh show rule` 在规则不存在时退出码非 0 —— 只读它，
    // 不解析本机化过的输出文本。
    let mut left = 0;
    for name in &names {
        if sys::netsh(&format!("advfirewall firewall show rule name=\"{name}\"")).is_ok() {
            left += 1;
            report.note(&format!("  [warn] 规则还在：{name}"));
        }
    }
    left
}

// ---------------------------------------------------------------------------
// 5. 身份备份 + 交棒 + 替身
// ---------------------------------------------------------------------------

fn appdata_backup() -> Option<PathBuf> {
    let base = std::env::var_os("ProgramData")?;
    let base = PathBuf::from(base);
    if base.as_os_str().is_empty() {
        return None;
    }
    Some(base.join("cloud-stream").join("uninstall-backup"))
}

/// 把这台机器的身份存到目录外面：端口、令牌、证书、录音设备原值。
///
/// 走 `--ops snapshot --ops-paths`（不是整包快照）：要活下来的是"这台机器原来
/// 是谁"，不是 30 MB 前端 —— 那个新装一次就回来了。同一个理由让 `install` 那边
/// 用 `--ops rollback` 放回去，于是"备份里有什么"只有一处定义。
///
/// 失败是 `[warn]` 而不是 `[fail]`：因为备份写不出来就拒绝卸载，会让用户卡在
/// 一个他明确要求清干净的机器上，而那比丢配置更糟。
fn save_identity(dir: &Path, report: &Report) {
    let exe = dir.join("stream-server.exe");
    if !exe.exists() {
        return;
    }
    if !sys::exe_has_marker(&exe, "ops snapshot failed") {
        report.note("  [skip] 这个 exe 早于 --ops，身份没法备份");
        report.note("         （该保的是 stream-config.toml / config.toml / cert.pem / cert.key）");
        return;
    }
    let Some(backup) = appdata_backup() else {
        report.note("  [warn] 拿不到 %ProgramData%，身份不备份");
        return;
    };
    match sys::run_bounded(
        &exe,
        &[
            "--ops",
            "snapshot",
            "--ops-dir",
            backup.to_string_lossy().as_ref(),
            "--ops-label",
            "preuninstall",
            "--ops-paths",
            r"stream-config.toml;config.toml;cert.pem;cert.key;logs\mic-default.json",
        ],
        dir,
        60,
    ) {
        Ok((0, text)) => {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                report.note(&format!("    {line}"));
            }
            report.note(&format!(
                "  [ok] 身份已存到目录外：{}\n重装时会自动放回去",
                backup.display()
            ));
        }
        Ok((code, text)) => {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                report.note(&format!("    {line}"));
            }
            report.note(&format!(
                "  [warn] 身份备份没做完（退出码 {code}）—— 下一次安装会用自己的默认值"
            ));
        }
        Err(err) => report.note(&format!("  [warn] 备份失败：{err}")),
    }
}

/// 把自己复制到 `%TEMP%` 并以替身身份启动，然后调用方就可以退出了。
fn spawn_purge(dir: &Path, opts: &Opts, report: &Report) -> Result<(), String> {
    use std::process::Stdio;

    let me = std::env::current_exe().map_err(|e| format!("拿不到自己的路径：{e}"))?;
    let temp = std::env::temp_dir();
    // 文件名带 pid：同上一次卸载留下的替身不会互相覆盖（那个可能还在等）。
    let helper = temp.join(format!("cloud-stream-purge-{}.exe", std::process::id()));
    std::fs::copy(&me, &helper)
        .map_err(|e| format!("复制到 {} 失败：{e}", helper.display()))?;

    let mut cmd = std::process::Command::new(&helper);
    cmd.arg("--purge").arg("--dir").arg(dir);
    for pid in &opts.wait {
        cmd.arg("--wait").arg(pid.to_string());
    }
    // 本进程（以及调用方的窗口）必须先退：它们都持有这棵树里的 exe。
    cmd.arg("--wait").arg(std::process::id().to_string());
    cmd.current_dir(&temp)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    let child = cmd
        .spawn()
        .map_err(|e| format!("起不了 {}：{e}", helper.display()))?;
    report.note(&format!("  [i] 替身 {}（pid {}）", helper.display(), child.id()));
    Ok(())
}

/// 替身：等父进程退出，删目录，然后把自己安排到下次开机时删除。
fn purge_stage(opts: &Opts) -> i32 {
    let Some(dir) = opts.dir.clone() else {
        dialog("--purge 需要 --dir。", "卸载", MB_ICONERROR);
        return 2;
    };

    // 等所有被点名的进程退出。60 秒是一个"人已经走开"的长度，不是
    // "还要读一个对话框"的长度 —— 对话框在交棒之前就弹完了。
    for pid in &opts.wait {
        wait_pid(*pid, 60);
    }

    let report = Report::new();
    report.resume(&dir);
    report.note("");
    report.note(&format!("--- 替身：删目录（{}）---", stamp()));

    match remove_tree(&dir) {
        Ok(()) => {
            report.note(&format!("  [ok] 目录已删除：{}", dir.display()));
            report.end("UNINSTALLED");
            // 成功不弹框：上一个对话框已经说过"正在删除"，
            // 目录自己消失就是答案。失败才需要打断人。
            cleanup_self(&report);
            0
        }
        Err(err) => {
            report.note(&format!("  [fail] {err}"));
            report.end("UNINSTALL-FAIL (files)");
            dialog(
                &format!(
                    "目录没删干净：\n{}\n\n{}\n\n\
                     服务、任务、防火墙规则都已经处理过了，只差这几项文件。\n\
                     关掉可能还在用这些文件的程序，然后手工删除这个目录。",
                    dir.display(),
                    err
                ),
                "卸载没有完全完成",
                MB_ICONERROR,
            );
            cleanup_self(&report);
            8
        }
    }
}

/// 等一个 pid 消失。拿不到句柄就当它已经退了。
///
/// `SYNCHRONIZE` 是能等待的最小权限；`WAIT_OBJECT_0` 表示"进程已经结束"。
fn wait_pid(pid: u32, seconds: u64) {
    unsafe {
        // `PROCESS_SYNCHRONIZE` 是"只等待、不读不写"的最小权限。
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle == 0 {
            return;
        }
        WaitForSingleObject(handle, (seconds * 1000) as u32);
        CloseHandle(handle);
    }
}

/// 删整棵树，失败重试。返回最后一次的失败原因。
///
/// 为什么要重试而不是一次 `remove_dir_all`：刚被 `TerminateProcess` 结束的
/// 进程，句柄是内核慢慢回收的，几毫秒内文件仍然报"正在被使用"。一次就放弃
/// 会让卸载在正确的操作上失败。
fn remove_tree(root: &Path) -> Result<(), String> {
    let mut last = String::new();
    for round in 1..=8 {
        match attempt_remove(root) {
            Ok(()) => return Ok(()),
            Err(err) => last = err,
        }
        if round == 8 {
            break;
        }
        std::thread::sleep(Duration::from_millis(400));
        // 每轮都再扫一次：父进程刚退出时看门狗可能又拉起过一个。
        kill_servers_under(root, None);
    }
    Err(last)
}

/// 一次尝试：先清只读位，再删文件，最后自底向上删目录。
///
/// 不用 `std::fs::remove_dir_all`：它失败时只说"拒绝访问"，不说**是哪个文件**。
/// 而"删不掉"这个结论的全部用处就是告诉用户该关掉什么。
fn attempt_remove(root: &Path) -> Result<(), String> {
    if !root.exists() {
        return Ok(());
    }
    let mut stack = vec![root.to_path_buf()];
    let mut dirs: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => return Err(format!("读不了 {}：{err}", dir.display())),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            // 目录联接/符号链接只删链接本身，不跟进去 —— 跟进去会删到树外面。
            if kind.is_symlink() {
                let _ = std::fs::remove_dir(&path).or_else(|_| std::fs::remove_file(&path));
                continue;
            }
            if kind.is_dir() {
                dirs.push(path.clone());
                stack.push(path);
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if meta.permissions().readonly() {
                    let _ = clear_readonly(&path);
                }
            }
            if let Err(err) = std::fs::remove_file(&path) {
                return Err(format!("删不掉 {}：{err}", path.display()));
            }
        }
    }
    // 子目录的路径一定比父目录长，所以按长度倒序 = 先子后父。
    dirs.sort_by_key(|p| std::cmp::Reverse(p.as_os_str().len()));
    for dir in dirs {
        if let Err(err) = std::fs::remove_dir(&dir) {
            return Err(format!("删不掉 {}：{err}", dir.display()));
        }
    }
    Ok(())
}

fn clear_readonly(path: &Path) -> std::io::Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms)
}

/// 把自己安排到下次开机时删除。
///
/// 替身在 `%TEMP%` 里，属于"不删也不会有人发现"的一类；但一个 9 MB 的残骸
/// 每次卸载留一个，攒起来就是垃圾。`MOVEFILE_DELAY_UNTIL_REBOOT` 是这件事的
/// 标准做法：注册表里登记一条 PendingFileRenameOperations，重启时由内核执行。
///
/// 失败就算了：临时目录里多一个文件，比在这儿弹一个框打断"卸载完成了"要好。
fn cleanup_self(report: &Report) {
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    let wide: Vec<u16> = me
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let ok = unsafe { MoveFileExW(wide.as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
    if ok == 0 {
        report.note(&format!(
            "  [i] 替身自己没排上删除队列，重启后可以手工删：{}",
            me.display()
        ));
    }
}

// ---------------------------------------------------------------------------
// 路径与护栏
// ---------------------------------------------------------------------------

/// 归一化：小写、`/` 变 `\`、去掉 `\\?\` 前缀与尾部分隔符。
///
/// `QueryFullProcessImageNameW` 一般给的是普通盘符路径，但任务 XML 里可能是
/// `\\?\C:\...` 这种扩展长度写法 —— 两边都归一化才能比。
fn norm_path(text: &str) -> String {
    let mut out = text.replace('/', "\\").to_lowercase();
    if let Some(rest) = out.strip_prefix(r"\\?\") {
        out = rest.to_owned();
    }
    while out.ends_with('\\') {
        out.pop();
    }
    out
}

/// `path` 是不是就在 `dir` 里面（含 `dir` 本身）。
///
/// 比字符串前缀多一步：`C:\cs` 不能匹配 `C:\cs2\x` —— 少这一步就会卸掉隔壁
/// 那个安装。
fn path_under(path: &Path, dir: &Path) -> bool {
    let a = norm_path(&path.to_string_lossy());
    let b = norm_path(&dir.to_string_lossy());
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || a.starts_with(&format!("{b}\\"))
}

/// 这个目录能不能删。返回 `Some(理由)` 表示拒绝。
///
/// 每一条都是廉价的，而且没有一条能被"我是管理员"说服 —— 一次误删一个盘根或
/// 用户主目录，代价和"卸载失败"不是一个量级。
fn refuse_reason(dir: &Path, force: bool) -> Option<String> {
    // 前两条是绝对的，`--force` 也越不过去（见 `absolutely_not`）。
    if let Some(why) = absolutely_not(dir) {
        return Some(why);
    }
    // 目录里连一个"我装过东西"的痕迹都没有：多半是打错了路径。
    //
    // `--force` 能越过这一条（"我知道我在干什么"）：一个人可能手工删掉了
    // 服务端，只剩下一堆前端文件，那时他仍然需要一个能清干净的任务和规则。
    if force {
        return None;
    }
    let looks_installed = dir.join("stream-server.exe").exists() || dir.join("manifest.json").exists();
    if !looks_installed {
        return Some(
            "这个目录里没有 stream-server.exe / manifest.json / stream-setting.exe，\
             看起来不是一个 cloud-stream 目录。"
                .to_owned(),
        );
    }
    None
}

/// 无论谁、带什么开关都不该碰的目录：盘根，和那几个系统目录。
///
/// 从 `refuse_reason` 里拆出来的，因为 `--uninstall-step` 那五条单步命令也要
/// 这道护栏 —— 它们同样会删任务、杀进程，而调用方（卸载器）传的路径一旦有
/// 毛病，后果和完整卸载一样重。
fn absolutely_not(dir: &Path) -> Option<String> {
    // 盘根（`C:\` 的 parent 是 None；`C:\x` 的 parent 是 `C:\`）。
    if dir.parent().is_none() {
        return Some("这是一个盘根。".to_owned());
    }
    for var in [
        "SystemRoot",
        "SystemDrive",
        "USERPROFILE",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramData",
    ] {
        let Some(value) = std::env::var_os(var) else {
            continue;
        };
        if !value.is_empty()
            && norm_path(dir.to_string_lossy().as_ref()) == norm_path(value.to_string_lossy().as_ref())
        {
            return Some(format!("这是 %{var}%。"));
        }
    }
    None
}

/// UTF-16 定长数组转字符串（`PROCESSENTRY32W::szExeFile`）。
fn wide_z(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_stage_dir_and_wait_list() {
        let opts = parse(&args(&[
            "--uninstall",
            "--dir",
            r"D:\tools\cloud stream",
            "--wait",
            "1234",
            "--wait",
            "5678",
            "--yes",
            "--keep",
        ]))
        .unwrap();
        assert_eq!(opts.stage, Stage::Main);
        assert_eq!(opts.dir.unwrap(), PathBuf::from(r"D:\tools\cloud stream"));
        assert_eq!(opts.wait, vec![1234, 5678]);
        assert!(opts.yes && opts.keep_files && !opts.force);
        assert_eq!(opts.task, TASK_NAME, "没给 --task 时必须用默认任务名");
    }

    #[test]
    fn purge_is_its_own_stage() {
        let opts = parse(&args(&["--purge", "--dir", "C:\\x"])).unwrap();
        assert_eq!(opts.stage, Stage::Purge);
    }

    #[test]
    fn uninstall_step_is_single_and_silent() {
        // 单步命令由卸载器拉起，背后没有人点"确定" —— 解析参数时就必须把
        // quiet 打开，不能指望调用方记得传 --quiet。
        let opts = parse(&args(&[
            "--uninstall-step",
            "firewall",
            "--dir",
            "C:\\Program Files\\cloud-stream",
        ]))
        .unwrap();
        assert_eq!(opts.step.as_deref(), Some("firewall"));
        assert!(opts.quiet, "单步必须是静默的");
        assert_eq!(opts.task, TASK_NAME, "没给 --task 时必须用默认任务名");
    }

    #[test]
    fn wait_needs_a_number() {
        assert!(parse(&args(&["--uninstall", "--wait", "abc"])).is_err());
        assert!(parse(&args(&["--uninstall", "--wait"])).is_err());
    }

    /// 隔壁目录不能被认成自己人 —— 少这一步就会卸掉另一个安装。
    #[test]
    fn sibling_dir_is_not_under() {
        assert!(path_under(
            Path::new(r"C:\cs\stream-server.exe"),
            Path::new(r"C:\cs")
        ));
        assert!(!path_under(
            Path::new(r"C:\cs2\stream-server.exe"),
            Path::new(r"C:\cs")
        ));
        assert!(!path_under(Path::new(r"C:\cs"), Path::new(r"D:\cs")));
        // 大小写、斜杠、尾分隔符、扩展长度前缀都不该影响判断。
        assert!(path_under(
            Path::new(r"\\?\c:/CS/sub/stream-server.exe"),
            Path::new(r"C:\cs\")
        ));
    }

    /// 真实导出的形状：`<Command>` 在 `<Exec>` 里，路径里的 `&` 是转义的。
    #[test]
    fn reads_command_and_unescapes() {
        let xml = r#"<Task><Actions Context="Author"><Exec>
  <Command>C:\A&amp;B\cloud-stream\stream-server.exe</Command>
  <Arguments>--run</Arguments>
  <WorkingDirectory>C:\A&amp;B\cloud-stream</WorkingDirectory>
</Exec></Actions></Task>"#;
        assert_eq!(
            task_command(xml).unwrap(),
            r"C:\A&B\cloud-stream\stream-server.exe"
        );
        // `&amp;lt;` 只能变成 `&lt;`，不能一路解成 `<`。
        assert_eq!(xml_unescape("a&amp;lt;b"), "a&lt;b");
    }

    /// 启用 vs 禁用：禁用的才有 `<Enabled>false</Enabled>`，而且它在 `<Settings>`
    /// 里；`<Triggers>` 里那个 `<Enabled>true</Enabled>` 不能拿来当判据。
    #[test]
    fn disabled_task_is_detected_from_settings_only() {
        let enabled = r#"<Task><Triggers><TimeTrigger><Enabled>true</Enabled></TimeTrigger></Triggers>
<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings></Task>"#;
        assert!(!settings_disabled(enabled));

        let disabled = r#"<Task><Triggers><TimeTrigger><Enabled>true</Enabled></TimeTrigger></Triggers>
<Settings><Enabled>false</Enabled><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings></Task>"#;
        assert!(settings_disabled(disabled));
    }

    #[test]
    fn owner_is_mine_foreign_or_unknown() {
        let ours = r#"<Task><Actions><Exec><Command>C:\cs\stream-server.exe</Command></Exec></Actions></Task>"#;
        assert!(matches!(
            task_owner(ours, Path::new(r"C:\cs")),
            TaskOwner::Mine
        ));

        let theirs = r#"<Task><Actions><Exec><Command>D:\other\stream-server.exe</Command></Exec></Actions></Task>"#;
        assert!(matches!(
            task_owner(theirs, Path::new(r"C:\cs")),
            TaskOwner::Foreign(_)
        ));

        // 读不出来时不猜"不是我的"：那个方向的代价是留下一个看门狗。
        assert!(matches!(
            task_owner("<Task></Task>", Path::new(r"C:\cs")),
            TaskOwner::Mine
        ));
    }

    /// 任务文件是 UTF-16LE。**中文路径必须能读对** —— 这是放弃
    /// `schtasks /Query` 的主要理由（它的输出走 OEM 代码页，中文会变问号，
    /// 于是"这条任务是不是我的"没法回答）。
    #[test]
    fn task_file_is_read_as_utf16_with_non_ascii_path() {
        let path = std::env::temp_dir().join(format!("cs-task-{}.xml", std::process::id()));
        let xml = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task><Settings><Enabled>false</Enabled></Settings>
<Actions><Exec><Command>D:\工具\cloud-stream\stream-server.exe</Command></Exec></Actions></Task>"#;
        let mut bytes = vec![0xFF, 0xFE];
        for unit in xml.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&path, &bytes).unwrap();

        let text = read_task_xml(&path).unwrap();
        assert_eq!(
            task_command(&text).unwrap(),
            r"D:\工具\cloud-stream\stream-server.exe",
            "非 ASCII 路径被读坏了"
        );
        assert!(settings_disabled(&text), "禁用状态没读出来");
        // 中文路径也要能被认成"我的"，而不是被当成替换字符糊掉。
        assert!(matches!(
            task_owner(&text, Path::new(r"D:\工具\cloud-stream")),
            TaskOwner::Mine
        ));
        let _ = std::fs::remove_file(&path);
    }

    /// 没 BOM 的 UTF-8 也得读（防着微软哪天换格式）。
    #[test]
    fn task_file_also_accepts_utf8() {
        let path = std::env::temp_dir().join(format!("cs-task8-{}.xml", std::process::id()));
        std::fs::write(&path, b"<Task><Actions><Exec><Command>C:\\cs\\x.exe</Command></Exec></Actions></Task>").unwrap();
        let text = read_task_xml(&path).unwrap();
        assert_eq!(task_command(&text).unwrap(), r"C:\cs\x.exe");
        let _ = std::fs::remove_file(&path);
    }

    /// 护栏：盘根、%SystemRoot%、没有安装痕迹的目录，一个都不能放过。
    #[test]
    fn refuses_dangerous_roots() {
        assert!(refuse_reason(Path::new(r"C:\"), false).is_some());
        if let Some(sysroot) = std::env::var_os("SystemRoot") {
            assert!(
                refuse_reason(Path::new(&sysroot), false).is_some(),
                "SystemRoot 没拦住"
            );
        }
        // 存在的目录但没有安装痕迹。
        let empty = std::env::temp_dir().join(format!("cs-refuse-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&empty);
        assert!(refuse_reason(&empty, false).is_some());
        // `--force` 越过"没有安装痕迹"，但**越不过**盘根与系统目录 ——
        // "我知道我在干什么"不能变成"我可以删掉 C:\"。
        assert!(refuse_reason(&empty, true).is_none());
        assert!(refuse_reason(Path::new(r"C:\"), true).is_some());
        let _ = std::fs::remove_dir_all(&empty);
    }

    /// 一个带 stream-server.exe 的临时目录必须放过（否则护栏会把正常路径也拦了）。
    #[test]
    fn allows_a_real_bundle_dir() {
        let dir = std::env::temp_dir().join(format!("cs-bundle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stream-server.exe"), b"x").unwrap();
        assert_eq!(refuse_reason(&dir, false), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 删整棵树：文件、嵌套目录、只读文件都要走干净，且不留空壳。
    #[test]
    fn remove_tree_takes_nested_dirs_and_readonly_files() {
        let root = std::env::temp_dir().join(format!("cs-rm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a").join("b")).unwrap();
        std::fs::write(root.join("a").join("b").join("x.txt"), b"1").unwrap();
        std::fs::write(root.join("root.txt"), b"2").unwrap();
        let ro = root.join("a").join("ro.txt");
        std::fs::write(&ro, b"3").unwrap();
        let mut perms = std::fs::metadata(&ro).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&ro, perms).unwrap();

        remove_tree(&root).unwrap();
        assert!(!root.exists(), "目录还在：{}", root.display());
    }

    /// 目录本来就不在，算成功（重跑一次卸载不该报错）。
    #[test]
    fn remove_tree_on_missing_dir_is_ok() {
        let root = std::env::temp_dir().join(format!("cs-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert!(remove_tree(&root).is_ok());
    }

    /// `--quiet` 与 `--keep` 常一起出现，但语义无关：前者"别弹框"，
    /// 后者"别删文件"，两个都得各自认得出来。
    #[test]
    fn quiet_and_keep_are_independent_switches() {
        let a = parse(&args(&["--uninstall", "--keep", "--quiet", "--yes"]));
        let a = a.unwrap();
        assert!(a.quiet && a.keep_files && a.yes);
        assert!(!a.force && !a.plan);

        let b = parse(&args(&["--silent"])).unwrap();
        assert!(b.quiet);
        assert!(!b.keep_files, "--quiet 不该顺带改 --keep");
    }
}
