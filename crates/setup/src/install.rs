//! 原生安装流程：把一个 exe 变成一台能跑的机器。
//!
//! # 两段，因为管理员令牌只该用在需要它的地方
//!
//! ```text
//! 第一段（用户令牌）  解压到 %LOCALAPPDATA%\cloud-stream，复制自己进去
//! 第二段（管理员）    放行防火墙、注册看门狗任务、启动、等服务应答
//! ```
//!
//! 解压**必须**在用户令牌下做。`runas` 有可能切换到另一个账户（本机唯一的
//! 管理员不是当前用户时一定会），那时提权进程的 `%LOCALAPPDATA%` 是别人的目录
//! —— 装到"管理员的家"里，而用它的人找不到。所以第一段先在自己的令牌下把文件
//! 铺好，第二段只做那两件非管理员做不了的事，并且**用 `--dir` 把目标路径显式
//! 传过去**（提权进程的当前目录不可靠）。
//!
//! # 为什么不是四个脚本
//!
//! 这一层原来由 `install.cmd`(525) + `setup-watchdog.bat`(226) + `setup-task.ps1`
//! (175) + `open-inbound.cmd`(112) 拼起来。四层嵌套的失败方式各不相同：cmd 的
//! 引号、powershell 的 `-Command` 再一次引号、`schtasks /RP` 存密码、`netsh`
//! 重复建规则。原生版把四件事做在同一个进程里，**没有命令行可拼**，于是那四类
//! 失败方式一起消失。
//!
//! 只剩两个外部程序，都是系统自带的单条命令：
//!
//! * `schtasks.exe` —— 任务 XML 写在文件里传参，不拼命令行；
//! * `netsh.exe` —— 用 `raw_arg` 原样交出去，绕开"含空格的规则名被二次加引号"。
//!
//! # 端口不在这里猜
//!
//! 防火墙要放行哪个 TCP 端口、哪个 UDP 区间，问 `stream-server.exe
//! --print-config json`。它自己读过配置文件（`configfile::resolve` 那三层：
//! `--config` → `config.toml` → `stream-config.toml` → 默认值），在这里再抄一份
//! 解析就是第二套判断 —— 而 `open-inbound.cmd` 里"40000-40049 必须和配置默认值
//! 保持同步"那句注释，正是第二套判断迟早分叉的自白。
//!
//! # 结论写哪儿
//!
//! `report` 模块：和 `install.cmd` 同一个文件、同一套 `=== RESULT:` 标记。所以
//! 控制台窗口那边的 `watch_install_result` 一行都不用改。

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows_sys::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING,
};

use crate::pack;
use crate::report::{stamp, Report};
// 这几个是和卸载共用的（`sys` 模块）：同一个对话框、同一个"跑系统命令"、
// 同一套提权。放在一起是为了让"两个流程的行为一致"变成编译期的事实，
// 而不是两处各写一遍再靠人记得同步。
use crate::sys::{
    self, command, confirm, dialog, elevated, netsh, ElevateError, CREATE_NO_WINDOW,
};

/// 看门狗任务名。与 `install.cmd` / `uninstall.cmd` / 文档里的名字必须一致 ——
/// 三处都靠这个名字找到同一台机器上的同一个任务。
pub const TASK_NAME: &str = "MCStreamSrv";

/// 防火墙规则名。前三条沿用 `open-inbound.cmd` 的拼写：老机器上已经存在的规则
/// 会被先删后建，名字不同就会留下一堆"看起来有、实际重复"的残骸。
// 三条规则名沿用 `open-inbound.cmd` 的拼写：老机器上已经存在的规则会被先删
// 后建，名字不同就会留下一堆"看起来有、实际重复"的残骸。卸载要用同一批名字
// 把它删干净，所以是 `pub(crate)` —— 三处（建、删、文档）共用一份字符串。
pub(crate) const RULE_ICMP: &str = "cloud-stream ICMPv4";
pub(crate) const RULE_UDP: &str = "cloud-stream webrtc UDP";
pub(crate) const RULE_APP: &str = "cloud-stream app";

// ---------------------------------------------------------------------------
// 参数
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    /// 第一段：解压 + 复制自己。
    Unpack,
    /// 第二段：防火墙 + 任务 + 等应答（需要管理员）。
    Finish,
    /// 只补防火墙规则（`--inbound`，需要管理员）。
    ///
    /// 这一步单独存在，是因为另外两种做法都太重：重跑 `--install` 会顺带覆盖
    /// 程序文件（`setup-inbound.cmd` 被删掉之后，这儿是唯一的轻量入口）。
    Inbound,
}

#[derive(Clone, Debug)]
struct Opts {
    stage: Stage,
    /// 装到哪儿。`None` = `%LOCALAPPDATA%\cloud-stream`。
    dir: Option<PathBuf>,
    /// 任务要注册给哪个账户。第一段从**自己的**环境变量取到，传给提权后的
    /// 第二段 —— 提权可能换账户，那时它的 `%USERNAME%` 是管理员，而桌面属于
    /// 别人。
    account: Option<String>,
    task: String,
    /// 不问确认（给控制台的按钮用）。
    yes: bool,
    /// 不弹任何框（给外面那层安装器用：Inno / MSI）。
    ///
    /// 语义是"结论只走退出码与结论文件"，不是"什么都不告诉你"——
    /// `sys::set_quiet` 会把原本该弹的文字改写到 stderr。
    quiet: bool,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut opts = Opts {
        stage: Stage::Unpack,
        dir: None,
        account: None,
        task: TASK_NAME.to_owned(),
        yes: false,
        quiet: false,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            // `--install` 是"我是安装器"这句话本身（`main` 用它判断角色），
            // 出现在这里就当它已经把 stage 定成 Unpack 了。
            "--install" => opts.stage = Stage::Unpack,
            "--finish" => opts.stage = Stage::Finish,
            "--inbound" => opts.stage = Stage::Inbound,
            "--dir" => {
                let value = it.next().ok_or("--dir 后面要跟一个路径")?;
                opts.dir = Some(PathBuf::from(value));
            }
            "--account" => opts.account = it.next().cloned(),
            "--task" => {
                let value = it.next().ok_or("--task 后面要跟一个任务名")?;
                opts.task = value.clone();
            }
            "--yes" | "-y" => opts.yes = true,
            // 外面那层安装器（Inno / MSI）用它：它的向导已经问过用户了，
            // 这里再弹一个框就是两个成功提示、两次确认，无人值守时还会卡死。
            // `--silent` 是同一个意思的别名（和 Inno 的 `/SILENT` 呼应）。
            "--quiet" | "--silent" => opts.quiet = true,
            // 其余一律忽略：控制台会把这个 exe 和它自己的参数一起传下来，
            // 为了一个不认识的开关让安装失败是没道理的。
            _ => {}
        }
    }
    Ok(opts)
}

/// 入口。返回进程退出码。
pub fn run(args: &[String]) -> i32 {
    // 参数解析失败时还不知道 `--quiet`，所以先从原始参数里探一次：
    // 外面那层安装器传给我们的参数如果写错了，弹框会把它的向导卡住。
    if args.iter().any(|a| a == "--quiet" || a == "--silent") {
        sys::set_quiet(true);
    }
    let opts = match parse(args) {
        Ok(opts) => opts,
        Err(msg) => {
            dialog(&msg, "安装参数", MB_ICONERROR);
            return 2;
        }
    };
    sys::set_quiet(opts.quiet);
    match opts.stage {
        Stage::Unpack => unpack(&opts),
        Stage::Finish => finish(&opts),
        Stage::Inbound => inbound_only(&opts),
    }
}

/// 诊断入口（`--task-xml`）：把要注册的任务原样打到 stdout。
///
/// 字段里"任务建不起来"时，`schtasks` 只会说"XML 无效" —— 而原因可能是一个
/// 没转义的 `&`、一个拼错的元素名、一个不接受的取值。把这段打出来，能直接和
/// 另一台机器上 `schtasks /Query /TN MCStreamSrv /XML` 的导出对比，也能交给
/// 任务计划程序自己的解析器试。
///
/// 走 stdout 而不是弹窗：它和 `--print-config` 是一类东西 —— 结果本身就是答案，
/// 管道能接、文件能存。
///
/// 里面的 `<Command>` 按 `task_root` 算：`--dir` → 自带 payload 时的默认安装目录
/// → 运行根。**不要**改成"exe 所在目录" —— 从 `target\output\` 跑这条命令时，
/// 那会指向安装器自己的位置，而它实际装到 `%LOCALAPPDATA%\cloud-stream`。
///
/// 实际路径打到 stderr，XML 只走 stdout：混进一行说明会让 `schtasks /XML`
/// 直接判"无效"。
pub fn print_task_xml(args: &[String]) -> i32 {
    let opts = match parse(args) {
        Ok(opts) => opts,
        Err(msg) => {
            // 这条命令天生是管道用法（`> task.xml`）。弹框会让脚本永远等在那儿，
            // 而调用方本来就在看输出 —— 同样的话走 stderr 就够。
            eprintln!("{msg}");
            return 2;
        }
    };
    let root = task_root(&opts);
    // XML 走 stdout（它就是 `> task.xml` 要存下来的东西）；这行说明走 stderr，
    // 混进 XML 会让 `schtasks /XML` 当场判"无效"。
    eprintln!(
        "# 任务里的路径按 {} 计算（要替别的机器或别的目录渲染，加 --dir）",
        root.display()
    );
    let account = sys::current_account().unwrap_or_else(|| r"DOMAIN\user".to_owned());
    print!(
        "{}",
        task_xml(&root.join("stream-server.exe"), &root, &account, &boundary())
    );
    0
}

/// `--task-xml` 该把路径算成哪个目录。
///
/// 这里必须和 `unpack` / `finish` 取 target 的**同一个顺序**，否则这条诊断会把
/// 人领向错误结论：从 `target\output\` 跑 `--task-xml`，按 exe 所在目录会得到
/// `target\output\stream-server.exe`，而 `--install` 实际铺到
/// `%LOCALAPPDATA%\cloud-stream` 并让任务指向那里 —— 一个"看着像 bug 的正确答案"。
///
/// 三档，从上往下：
/// 1. `--dir`：明确的意图，压过所有推断（替另一台机器渲染时用它）；
/// 2. 这个 exe 自带 payload（安装器角色）：目标是它**将要**解压到的地方；
/// 3. 其余（已经装在运行根里的那一份）：运行根就是目标。
///
/// 抽成纯函数是为了能钉住第 1、2 档的优先级 —— `pack::summary()` 依赖构建期
/// 有没有 embed，测试里造不出那个条件。
fn choose_task_root(
    explicit: Option<&Path>,
    has_payload: bool,
    default_target: Option<PathBuf>,
    run_root: &Path,
) -> PathBuf {
    if let Some(dir) = explicit {
        return dir.to_path_buf();
    }
    if has_payload {
        // 拿不到 `%LOCALAPPDATA%` 时退回运行根：这时给一个能看的路径，比不输出
        // XML 有用 —— 反正真装的时候 `unpack` 会因为同一个原因弹框报错。
        if let Some(dir) = default_target {
            return dir;
        }
    }
    run_root.to_path_buf()
}

fn task_root(opts: &Opts) -> PathBuf {
    choose_task_root(
        opts.dir.as_deref(),
        pack::summary().is_some(),
        pack::default_target(),
        &crate::run_root(),
    )
}

// ---------------------------------------------------------------------------
// 第一段：解压
// ---------------------------------------------------------------------------

fn unpack(opts: &Opts) -> i32 {
    let target = match opts.dir.clone().or_else(pack::default_target) {
        Some(dir) => dir,
        None => {
            dialog(
                "拿不到 %LOCALAPPDATA%，也没有给 --dir，不知道该装到哪儿。",
                "安装",
                MB_ICONERROR,
            );
            return 2;
        }
    };
    let Some((files, bytes)) = pack::summary() else {
        // 这是开发机构建的 exe（没有开 `embed`）。说出来，而不是装一个空目录
        // 再报成功 —— 后者会让人以为是别的地方坏了。
        dialog(
            "这个 exe 里没有内置安装包。\n\n\
             分发用的安装器是带 embed 构建的那一个；开发机构建的版本只有窗口。",
            "安装",
            MB_ICONERROR,
        );
        return 2;
    };

    if !opts.yes {
        let text = format!(
            "把 cloud-stream 装到：\n{}\n\n\
             {} 个文件，约 {:.0} MB。\n\n\
             目标目录里已有的配置、证书、令牌、日志会被保留 —— 只覆盖同名的程序文件。",
            target.display(),
            files,
            bytes as f64 / 1e6
        );
        if !confirm(&text, "安装") {
            return 0;
        }
    }

    let report = Report::new();
    report.start(&target, "install");
    report.note(&format!("archive : {files} files, {:.1} MB", bytes as f64 / 1e6));

    let unpacked = pack::unpack_to(&target, |done, total| {
        // 每 400 个记一行。两千个文件逐个记，结论文件就成了一份进度条，
        // 而人要看的是"停在哪一步"。
        if done % 400 == 0 || done == total {
            report.note(&format!("  [1/4] unpack {done}/{total}"));
        }
    });
    let unpacked = match unpacked {
        Ok(done) => done,
        Err(err) => {
            report.note(&format!("  [fail] {err}"));
            report.end("FAIL (unpack)");
            dialog(&format!("解开内置包失败：\n\n{err}"), "安装", MB_ICONERROR);
            return 3;
        }
    };
    report.note(&format!(
        "  [1/4] {} 个文件已就位（{:.1} MB）",
        unpacked.files,
        unpacked.bytes as f64 / 1e6
    ));

    // 归档里刻意没有安装器自己（否则是"exe 里嵌着包含自己的包"）。这一份复制
    // 过去之后就是控制台：它在 `stream-server.exe` 旁边，`main` 据此判断角色。
    if let Err(err) = pack::place_self(&target) {
        report.note(&format!("  [fail] {err}"));
        report.end("FAIL (place-self)");
        dialog(&format!("复制安装器失败：\n\n{err}"), "安装", MB_ICONERROR);
        return 3;
    }

    let exe = target.join("stream-server.exe");
    if !exe.exists() {
        report.end("FAIL (包不完整：没有 stream-server.exe)");
        dialog(
            &format!(
                "内置包里没有 stream-server.exe。\n\n目标目录：{}",
                target.display()
            ),
            "安装",
            MB_ICONERROR,
        );
        return 3;
    }

    // 卸载过一回的机器上有一份身份备份（配置/证书/录音设备原值）。装回来时
    // 把它放回去 —— 否则"卸载再装"会悄悄换掉令牌和证书，客户端那边全连不上。
    restore_identity(&exe, &report);

    // 第二段要管理员。已经提权就直接进去，不再弹一次 UAC。
    if elevated() {
        report.note("elevate: 当前已是管理员，直接继续");
        let next = Opts {
            stage: Stage::Finish,
            dir: Some(target),
            ..opts.clone()
        };
        return finish(&next);
    }

    let account = opts.account.clone().or_else(sys::current_account);
    match elevate(&target, account.as_deref(), &opts.task) {
        Ok(()) => {
            report.note("elevate: 已在管理员窗口继续（结论由那一侧写进本文件）");
            0
        }
        Err(ElevateError::Denied) => {
            report.end("CANCELLED (UAC 被拒绝)");
            dialog(
                "文件已经装好了，但注册服务和放行防火墙需要管理员，而 UAC 被拒绝了。\n\n\
                 现在这台机器上什么都还没跑起来。再点一次「安装」并允许提权即可。",
                "安装",
                MB_ICONWARNING,
            );
            5
        }
        Err(ElevateError::Failed(code)) => {
            report.end(&format!("FAIL (elevate {code})"));
            dialog(
                &format!("起不了管理员窗口（ShellExecute={code}）。"),
                "安装",
                MB_ICONERROR,
            );
            4
        }
    }
}

// ---------------------------------------------------------------------------
// 第二段：防火墙 + 任务 + 等应答
// ---------------------------------------------------------------------------

fn finish(opts: &Opts) -> i32 {
    let Some(target) = opts.dir.clone() else {
        // `--finish` 是提权进程在跑，它的当前目录是 System32，从那儿推不出目标。
        dialog("--finish 需要 --dir。", "安装", MB_ICONERROR);
        return 2;
    };
    let report = Report::new();
    report.resume(&target);
    report.note(&format!("resumed : {} (管理员阶段)", stamp()));

    let exe = target.join("stream-server.exe");
    if !exe.exists() {
        report.end("FAIL (没有 stream-server.exe)");
        dialog(
            &format!("{} 里没有 stream-server.exe。", target.display()),
            "安装",
            MB_ICONERROR,
        );
        return 3;
    }

    // 端口与 ICE 区间问服务端自己 —— 见模块头部。
    let eff = effective_config(&exe);
    report.note(&format!(
        "ports   : tcp {}{}",
        eff.port,
        if eff.udp_max >= eff.udp_min {
            format!(", udp {}-{}", eff.udp_min, eff.udp_max)
        } else {
            String::from("（未配置 ICE 区间）")
        }
    ));

    report.note("[2/4] 防火墙入站规则");
    if !open_inbound(&exe, &eff, &report) {
        // 不是致命错误：策略/N 方防火墙可能管着这些规则，而 TCP 8080 之外的
        // 三条只影响 WebRTC 与 ping。装完仍然可用，所以降级成警告。
        report.note("  [warn] 有几条规则没建上（策略或第三方防火墙？）");
    }

    report.note(&format!("[3/4] 看门狗任务 {}", opts.task));
    let account = opts.account.clone().or_else(sys::current_account);
    if let Err(err) = register_task(&target, &exe, account.as_deref(), &opts.task) {
        report.note(&format!("  [fail] {err}"));
        report.end("FAIL (task)");
        dialog(
            &format!(
                "注册计划任务失败：\n\n{err}\n\n\
                 程序文件已经铺到 {}，缺的只是那个看门狗任务，所以开机不会自己起来。\n\n\
                 常见原因是安全策略禁止创建计划任务，或者任务名被别的账户占着。\
                 处理完之后，双击上面那个目录里的 cloud-setup.exe，点一次\
                 「安装 / 修复」就会重试这一步。",
                target.display()
            ),
            "安装",
            MB_ICONERROR,
        );
        return 6;
    }
    report.note("  [ok] 任务已注册并回读校验（--run / InteractiveToken）");

    report.note(&format!("[4/4] 启动并等 127.0.0.1:{} 应答", eff.port));
    let _ = command("schtasks", &["/Run", "/TN", &opts.task]);
    if !wait_port(eff.port, &report, 90) {
        report.end("FAIL (no listener)");
        dialog(
            &format!(
                "任务已注册，但 90 秒内 {} 上没有应答。\n\n\
                 最可能的原因：这台机器缺 VC++ 运行库（vcruntime140.dll），\
                 那样 exe 在写第一条日志之前就退出了。\n\n\
                 详细经过：\n{}\n\
                 服务自己的日志：{}",
                eff.port,
                report
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "（没写下来）".to_owned()),
                target.join("logs").join("watchdog.log").display()
            ),
            "安装",
            MB_ICONERROR,
        );
        return 7;
    }
    report.note(&format!("  [ok] {}/api/status 所在端口已应答", eff.port));
    report.end("OK");

    let mut text = format!(
        "装好了。\n\n目录：{}\n服务：127.0.0.1:{} 已应答\n\n\
         打开 http://localhost:{}/ 就能看到画面。\n\
         计划任务 {} 每分钟检查一次，进程没了会被拉起来。",
        target.display(),
        eff.port,
        eff.port,
        opts.task
    );
    if let Some(ip) = local_ip() {
        text.push_str(&format!(
            "\n\n同一局域网里用 http://{ip}:{}/ 访问（别的机器上能开这个网址，说明防火墙是对的）。",
            eff.port
        ));
    }
    dialog(&text, "安装完成", MB_ICONINFORMATION);
    0
}

/// 只补入站规则：`--inbound`。
///
/// # 为什么值得单独一个开关
///
/// "能连上但没画面 / 外面 ping 不通 / 换了端口之后一切正常但别人打不开"这三
/// 件事的共同解法是补四条防火墙规则，而它们本来住在 `setup-inbound.cmd` 与
/// `open-inbound.cmd` 里。两个脚本删掉之后不能没有替代品：
///
/// * 重跑 `--install` 也能补，但它会顺带覆盖程序文件 —— 而"补一条规则"和
///   "重装一次"的代价差一个数量级；
/// * 让用户自己拼 `netsh` 就是在要求他记住端口号和 UDP 区间。
///
/// 端口与区间仍然问服务端自己（`--print-config json`），所以这个开关、安装
/// 流程、服务启动时的自检，三处用的是同一份数字。
fn inbound_only(opts: &Opts) -> i32 {
    // 从整包目录里运行（`--dir` 或本 exe 所在目录）—— 这两个位置都可能有
    // `stream-server.exe`，而"装到 %LOCALAPPDATA% 之后的控制台"就是后者。
    let target = opts.dir.clone().unwrap_or_else(crate::run_root);
    let exe = target.join("stream-server.exe");
    if !exe.exists() {
        dialog(
            &format!(
                "{} 里没有 stream-server.exe。\n\n\
                 用 --dir 指向运行根，或者在那个目录里运行这个 exe。",
                target.display()
            ),
            "放行防火墙",
            MB_ICONERROR,
        );
        return 3;
    }

    if !elevated() {
        let params = format!("--inbound --dir \"{}\"", target.display());
        return match sys::elevate_raw(&params, &target) {
            Ok(()) => 0,
            Err(ElevateError::Denied) => {
                dialog(
                    "UAC 被拒绝了 —— 防火墙规则没有改动。\n\n\
                     写防火墙规则必须管理员，这是 Windows 的限制。",
                    "放行防火墙",
                    MB_ICONWARNING,
                );
                5
            }
            Err(ElevateError::Failed(code)) => {
                dialog(
                    &format!("起不了管理员窗口（ShellExecute={code}）。"),
                    "放行防火墙",
                    MB_ICONERROR,
                );
                4
            }
        };
    }

    let eff = effective_config(&exe);
    let report = Report::new();
    report.resume(&target);
    report.note(&format!("--- inbound {} ---", stamp()));
    report.note(&format!("ports   : tcp {}", eff.port));
    let ok = open_inbound(&exe, &eff, &report);
    report.end(if ok { "OK (inbound)" } else { "WARN (inbound)" });

    if ok {
        let mut text = format!(
            "四条入站规则已放行：\n\
             TCP {}\nICMPv4 echo（让 ping 有应答）\n\
             UDP {}-{}（WebRTC 媒体；缺了是\"连上但黑屏\"）\n\
             程序本身（任何端口）",
            eff.port, eff.udp_min, eff.udp_max
        );
        if let Some(ip) = local_ip() {
            text.push_str(&format!(
                "\n\n同一局域网里访问 http://{ip}:{}/",
                eff.port
            ));
        }
        dialog(&text, "放行防火墙", MB_ICONINFORMATION);
        0
    } else {
        dialog(
            &format!(
                "有几条规则没建上 —— 多半是域策略或第三方防火墙接管了这一层。\n\n\
                 经过写在：{}",
                report
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "（没写下来）".to_owned())
            ),
            "放行防火墙",
            MB_ICONWARNING,
        );
        6
    }
}

/// 服务端自己算出来的端口与 ICE 区间。
pub(crate) struct Effective {
    pub(crate) port: u16,
    udp_min: u16,
    udp_max: u16,
}

impl Default for Effective {
    fn default() -> Self {
        // 与服务端默认值一致。只在 `--print-config` 都问不出来时才用 —— 那时
        // 服务已经起了，至少别让防火墙规则全空着。
        Effective {
            port: 8080,
            udp_min: 40000,
            udp_max: 40049,
        }
    }
}

/// 问服务端自己：既有的配置解析在它那儿（`configfile::resolve` 那三层）。
///
/// 卸载也要用它 —— 改过端口的机器上，规则名是 `cloud-stream 9000`，而卸载
/// 那一侧如果假设 8080，就会留下一条指向旧端口的规则（不致命，但是"说好了
/// 清干净"没做到）。
pub(crate) fn effective_config(exe: &Path) -> Effective {
    let Ok(out) = std::process::Command::new(exe)
        .args(["--print-config", "json"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    else {
        return Effective::default();
    };
    if !out.status.success() {
        return Effective::default();
    }
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return Effective::default();
    };
    let num = |key: &str| doc.get(key).and_then(|v| v.as_u64());
    Effective {
        port: num("port").unwrap_or(8080) as u16,
        udp_min: num("webrtc_udp_port_min").unwrap_or(40000) as u16,
        udp_max: num("webrtc_udp_port_max").unwrap_or(40049) as u16,
    }
}

/// 上一台机器卸载时留下的身份备份，装回来时放回去。
///
/// 走 `--ops rollback`（`uninstall.cmd` 就是用它拍的快照），所以"备份里有什么"
/// 仍然只有一处定义 —— 服务端的 `--ops-paths` 列表。
fn restore_identity(exe: &Path, report: &Report) {
    let Some(program_data) = std::env::var_os("ProgramData") else {
        return;
    };
    let backup = PathBuf::from(program_data)
        .join("cloud-stream")
        .join("uninstall-backup");
    if !backup.is_dir() {
        return;
    }
    report.note(&format!("  [restore] 发现身份备份 {}", backup.display()));
    match command(
        exe.to_string_lossy().as_ref(),
        &[
            "--ops",
            "rollback",
            "--ops-dir",
            backup.to_string_lossy().as_ref(),
        ],
    ) {
        Ok(text) => {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                report.note(&format!("    {line}"));
            }
            report.note("  [restore] 已放回配置/证书/录音设备原值");
        }
        // 备份坏掉不该挡住安装：那是一次"重装"，而重装的主要目的就是让机器
        // 重新跑起来。
        Err(err) => report.note(&format!("  [warn] 身份没放回去：{err}")),
    }
}

// ---------------------------------------------------------------------------
// 防火墙
// ---------------------------------------------------------------------------

/// 建入站规则。返回 false = 至少有一条没建上。
///
/// 四条规则各自挡一类"服务在跑、外面连不上"：
///
/// * TCP `<port>` —— 页面、WS 信令、文件传输；
/// * ICMPv4 echo —— 让 `ping` 有应答。缺了它，一台完全健康的机器看起来像死的，
///   而这是所有人都会先试的那一条；
/// * UDP `<min>-<max>` —— WebRTC 的 ICE host candidate。信令走 TCP、媒体走 UDP，
///   只开一条的症状是"连上了但黑屏"；
/// * 程序规则 —— 按路径匹配所有端口，兜住改了端口的部署。
///
/// 先删后建：`netsh` 允许同名规则存在多条，只补缺失的那条会让另外三条每装一次
/// 多一份。删还顺手清掉"Windows 已阻止此应用的部分功能"留下的 BLOCK 规则 ——
/// BLOCK 优先于 allow，一条被驳回的阻止规则看起来像"规则都在，但一个包都不过"。
fn open_inbound(exe: &Path, eff: &Effective, report: &Report) -> bool {
    let exe_text = exe.to_string_lossy();
    let port = eff.port;

    // 端口规则的名字里带端口号（沿用 `cloud-stream 8080` 的拼法）。同时清掉
    // 8080 那个历史名字：改过端口的机器上会留着一条描述旧端口的规则。
    let mut names = vec![format!("cloud-stream {port}")];
    if port != 8080 {
        names.push("cloud-stream 8080".to_owned());
    }
    names.push(RULE_ICMP.to_owned());
    names.push(RULE_UDP.to_owned());
    names.push(RULE_APP.to_owned());

    for name in &names {
        let _ = netsh(&format!("advfirewall firewall delete rule name=\"{name}\""));
    }
    let _ = netsh(&format!(
        "advfirewall firewall delete rule name=all dir=in program=\"{exe_text}\""
    ));

    let mut ok = true;
    let adds = [
        format!(
            "advfirewall firewall add rule name=\"cloud-stream {port}\" dir=in action=allow protocol=TCP localport={port} profile=any"
        ),
        format!(
            "advfirewall firewall add rule name=\"{RULE_ICMP}\" dir=in action=allow protocol=icmpv4:8,any profile=any"
        ),
        format!(
            "advfirewall firewall add rule name=\"{RULE_UDP}\" dir=in action=allow protocol=UDP localport={} profile=any",
            if eff.udp_max > eff.udp_min {
                format!("{}-{}", eff.udp_min, eff.udp_max)
            } else {
                eff.udp_min.to_string()
            }
        ),
        format!(
            "advfirewall firewall add rule name=\"{RULE_APP}\" dir=in action=allow program=\"{exe_text}\" enable=yes profile=any"
        ),
    ];
    for raw in &adds {
        // 规则名里有空格，所以整条命令原样交出去（`raw_arg`）：让 Rust 去拼
        // 命令行，`name="cloud-stream 8080"` 会被二次加引号成
        // `name=""cloud-stream 8080""`，netsh 只认得前一种。
        if let Err(err) = netsh(raw) {
            ok = false;
            report.note(&format!("  [warn] netsh 拒绝了一条规则：{err}"));
        }
    }
    ok
}

// ---------------------------------------------------------------------------
// 计划任务
// ---------------------------------------------------------------------------

/// 任务 XML（Task Scheduler 1.2）。
///
/// 三个值必须是现在这样，改任何一个都会毁掉一条已经验证过的性质：
///
/// * `<LogonType>InteractiveToken</LogonType>` —— 复用控制台会话已有的令牌。
///   这一条替代了 `schtasks /RU /RP`：不需要密码、不读也不写 SAM、空密码账户
///   也能用。**采集和 SendInput 是按会话隔离的**，跑到 session 0 的进程看到的
///   是一张空桌面。
/// * `<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>` —— 看门狗的
///   全部机制就在这一行：活着的实例挡住新实例，死掉之后下一次触发把它拉回来。
/// * `<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>` —— 不限时。`schtasks`
///   对重复任务的默认上限是 72 小时，会悄悄杀掉一个已经跑了三天的服务。
fn task_xml(exe: &Path, root: &Path, account: &str, boundary: &str) -> String {
    let exe = xml_escape(&exe.to_string_lossy());
    let root = xml_escape(&root.to_string_lossy());
    let account = xml_escape(account);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>cloud-stream 推流服务（每分钟检查一次，进程没了就拉起来）</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{account}</UserId>
    </LogonTrigger>
    <TimeTrigger>
      <StartBoundary>{boundary}</StartBoundary>
      <Enabled>true</Enabled>
      <Repetition>
        <Interval>PT1M</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
    </TimeTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{account}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <AllowHardTerminate>true</AllowHardTerminate>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <StartWhenAvailable>true</StartWhenAvailable>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>--run</Arguments>
      <WorkingDirectory>{root}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// XML 文本转义。路径里的 `&` 是真实存在的（`D:\A&B\tools`），不转义任务就
/// 建不起来，而 `schtasks` 的报错只会说"XML 无效"。
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// `/Create` 到底成没成。返回 `Some(原因)` = 失败。
///
/// **只看退出码。** 原来还看了一眼输出里有没有 `success` / `成功`，那个判据在
/// .150 上把"创建成功"判成了失败：schtasks 的中文输出是 GBK，被当成 UTF-8 解码
/// 之后那两个汉字根本不存在，`contains("成功")` 永远为假。结果任务建好了、服务
/// 监听上 8080 了，界面弹的却是"注册计划任务失败"。
///
/// 换掉它不只是绕开编码：文案本来就不可依赖 —— 英文机说 `SUCCESS`、中文机说
/// `成功`、下个版本还可能换个说法。退出码只有 0 与非 0 两种含义，不翻译。
fn create_verdict(code: i32, output: &str) -> Option<String> {
    if code == 0 {
        return None;
    }
    let text = output.trim();
    if text.is_empty() {
        Some(format!("schtasks 退出码 {code}（没有任何输出）"))
    } else {
        Some(format!("schtasks 退出码 {code}：\n{text}"))
    }
}

/// 注册 + 回读校验。
///
/// 回读是必须的：任务可以创建成功却指向一个被截断的路径，或者带着没人想要的
/// 登录类型。两种症状都长得像"任务跑了，但什么都没监听"，而且是几天后才发现。
fn register_task(
    root: &Path,
    exe: &Path,
    account: Option<&str>,
    task: &str,
) -> Result<(), String> {
    let account = account.ok_or(
        "拿不到要注册给哪个账户（%USERNAME% 是空的），没法保证任务落在桌面会话里",
    )?;
    let xml = task_xml(exe, root, account, &boundary());
    let file = std::env::temp_dir().join(format!("cloud-stream-task-{}.xml", std::process::id()));
    // schtasks 的导出就是 UTF-16，导入同样接受它。写成 UTF-8 而声明写 UTF-16
    // 会被拒（"XML 无效"），而声明写成 UTF-8 又得赌 schtasks 有没有按声明读 ——
    // 直接照它自己的格式写，没有可赌的地方。
    let mut bytes = vec![0xFF, 0xFE];
    for unit in xml.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&file, &bytes).map_err(|e| format!("写不进 {}：{e}", file.display()))?;

    let xml_path = file.to_string_lossy().into_owned();
    let args = ["/Create", "/TN", task, "/XML", xml_path.as_str(), "/F"];
    let mut outcome = sys::run("schtasks", &args)
        .map_err(|e| format!("schtasks 起不来：{e}"))?;
    if outcome.0 != 0 {
        // 覆盖一个**正在运行**的任务会被拒（0x8004131F），而「安装 / 修复」用在
        // 已经装过的机器上时任务恰恰在跑。先 `/End` 再试一次。
        //
        // 只在这时才停：无条件 `/End` 会让"因为别的原因建不起来"变成"顺手把
        // 本来能跑的服务停了"—— 那是比装不上更糟的结果（.150 上真发生过一次）。
        let _ = command("schtasks", &["/End", "/TN", task]);
        outcome = sys::run("schtasks", &args)
            .map_err(|e| format!("schtasks 起不来：{e}"))?;
    }
    if let Some(why) = create_verdict(outcome.0, &outcome.1) {
        // 失败时**留下** XML 文件：`schtasks` 的报错只到"XML 无效"这一层，
        // 真正要看的是里面那个元素写错了，而文件已经删掉就没得看了。
        return Err(format!("{why}\n\n任务 XML 留在：{}", file.display()));
    }
    // 成功时 schtasks 会打印一句"成功: 成功创建计划任务…"，那句话没有信息量，
    // 所以不往结论文件里搬。**只有成功才删 XML** —— 失败那条路要把它留下来。
    let _ = std::fs::remove_file(&file);

    // 回读。退出码非 0 时不要继续按"内容里没有 --run"报错 —— 那等于把
    // "查询本身失败了"说成"任务建错了"，而这两件事要查的地方完全不同。
    let dump = match sys::run("schtasks", &["/Query", "/TN", task, "/XML"]) {
        Ok((0, text)) => text.to_lowercase(),
        Ok((code, text)) => {
            return Err(format!(
                "任务已经创建，但回读时 schtasks 退出码 {code}：\n{}",
                text.trim()
            ))
        }
        Err(err) => return Err(format!("任务已经创建，但回读时出错：{err}")),
    };
    if !dump.contains("--run") {
        return Err(format!(
            "任务建好了，但动作里没有 --run。用这条命令看一眼：\n\
             schtasks /Query /TN {task} /XML"
        ));
    }
    if !dump.contains("interactivetoken") {
        return Err(format!(
            "任务的登录类型不是 Interactive（进程会跑到桌面会话之外）。\n\
             schtasks /Query /TN {task} /XML"
        ));
    }
    Ok(())
}

/// 重复触发需要一个基准时间。给"现在"就够了：Task Scheduler 会从这个基准
/// 往后推算下一个间隔，往过去写一个时间反而可能让它立刻触发（那会和紧接着的
/// `/Run` 撞成一次"任务已在运行"）。
///
/// 借 `report::stamp()` 再换个分隔符，而不是再调一次 `GetLocalTime`：结论文件
/// 和任务 XML 上的时间必须是同一只钟，否则排查时会看到"装完三秒才注册任务"。
fn boundary() -> String {
    stamp().replace(' ', "T")
}

// ---------------------------------------------------------------------------
// 等应答 / 提权 / 对话框
// ---------------------------------------------------------------------------

/// 等端口开始应答。返回 false = 等满了还是没人听。
fn wait_port(port: u16, report: &Report, tries: u32) -> bool {
    let addr = match format!("127.0.0.1:{port}").parse() {
        Ok(addr) => addr,
        Err(_) => return false,
    };
    for attempt in 1..=tries {
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok() {
            report.note(&format!("  [ok] {attempt} 秒后有应答"));
            return true;
        }
        // 每 15 秒说一句：等 90 秒什么都不说是最像卡死的一种表现。
        if attempt % 15 == 0 {
            report.note(&format!("  ... 已等 {attempt} 秒，{port} 还没有应答"));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    false
}

/// 本机在默认路由上的那个 IPv4。拿不到就 None（离线、只有回环）。
fn local_ip() -> Option<std::net::IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // 不真发包：connect 一个公网地址只是为了让路由表挑一个出口地址。
    sock.connect("8.8.8.8:80").ok()?;
    let addr = sock.local_addr().ok()?;
    let ip = addr.ip();
    if ip.is_loopback() {
        return None;
    }
    Some(ip)
}

/// 提权重新跑自己（`--finish`）。
///
/// 不提权是不行的：`netsh advfirewall` 与在根任务文件夹里注册任务都要管理员。
/// 但**只提权第二段** —— 解压已经在用户令牌下做完了（见模块头部）。
///
/// 参数的拼法在这里，而不是在 `sys` 里：什么阶段要带什么参数是这个流程自己的
/// 知识（`--finish` / `--account` / `--task`），`sys` 只负责"帮我把它交出去"。
fn elevate(target: &Path, account: Option<&str>, task: &str) -> Result<(), sys::ElevateError> {
    let mut params = format!("--install --finish --yes --dir \"{}\"", target.display());
    if let Some(account) = account {
        params.push_str(&format!(" --account \"{account}\""));
    }
    if task != TASK_NAME {
        params.push_str(&format!(" --task \"{task}\""));
    }
    // 失败值原样交给调用方：这里只关心"是不是用户拒了"，其余都是 ShellExecute
    // 的错误码，而那个码比任何转述都有用。
    sys::elevate_raw(&params, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dir_account_and_stage() {
        let args: Vec<String> = [
            "--install",
            "--finish",
            "--dir",
            r"D:\tools\cloud stream",
            "--account",
            r"DESKTOP\me",
            "--yes",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let opts = parse(&args).unwrap();
        assert_eq!(opts.stage, Stage::Finish);
        assert_eq!(opts.dir.unwrap(), PathBuf::from(r"D:\tools\cloud stream"));
        assert_eq!(opts.account.as_deref(), Some(r"DESKTOP\me"));
        assert!(opts.yes);
        assert_eq!(opts.task, TASK_NAME, "没给 --task 时必须用默认任务名");
    }

    /// 双引号里的空格是路径的一部分：`--dir` 必须整段吃掉，不能把
    /// "D:\tools\cloud" 当成路径、把 "stream" 当成下一个参数。
    #[test]
    fn unknown_switches_are_ignored() {
        let args: Vec<String> = ["--install", "--whatever", "--dir", "C:\\x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let opts = parse(&args).unwrap();
        assert_eq!(opts.dir.unwrap(), PathBuf::from("C:\\x"));
    }

    #[test]
    fn missing_value_is_an_error() {
        let args: Vec<String> = ["--dir"].iter().map(|s| s.to_string()).collect();
        assert!(parse(&args).is_err());
    }

    /// 路径里的 `&` 是真实存在的。不转义 → 任务建不起来，而报错只说"XML 无效"。
    #[test]
    fn xml_specials_are_escaped() {
        assert_eq!(xml_escape(r"D:\A&B\t"), r"D:\A&amp;B\t");
        assert_eq!(xml_escape("a<b>c"), "a&lt;b&gt;c");
    }

    /// 三条"改了就毁"的设置必须真的在这份 XML 里。
    #[test]
    fn task_xml_keeps_the_three_decisions() {
        let xml = task_xml(
            Path::new(r"C:\cs\stream-server.exe"),
            Path::new(r"C:\cs"),
            r"PC\me",
            "2026-11-12T10:00:00",
        );
        assert!(xml.contains("<Arguments>--run</Arguments>"));
        assert!(xml.contains("<WorkingDirectory>C:\\cs</WorkingDirectory>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        // 两个触发器：登录时 + 每分钟。
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("<Interval>PT1M</Interval>"));
        // 不允许把密码写进任务里 —— 这是这套东西从来不要密码的原因。
        assert!(!xml.to_lowercase().contains("<password"));
    }

    /// `--quiet` 要能被认出来，且 `--silent` 是它的别名。
    ///
    /// 这条开关的意义不在"少一个框"，而在于**外面那层安装器能不能活着等到
    /// 我们返回**：无人值守安装里一个 MessageBox 会让进程一直等下去，
    /// 外面那层就永远收不到退出码。
    #[test]
    fn quiet_is_parsed_and_has_an_alias() {
        let a = parse(&[
            "--finish".into(),
            "--dir".into(),
            r"D:\s".into(),
            "--quiet".into(),
        ])
        .unwrap();
        assert!(a.quiet);
        assert_eq!(a.stage, Stage::Finish);

        let b = parse(&["--silent".into()]).unwrap();
        assert!(b.quiet);

        // 不给就不静默 —— 双击场景还是要弹框的。
        assert!(!parse(&["--install".into()]).unwrap().quiet);
    }

    /// `--task-xml` 的路径口径：三档优先级。
    ///
    /// 第二条断言是这次修的 bug：安装器从 `target\output\` 这种"不是运行根"的
    /// 地方被跑起来时，任务指向的必须是它**将要解压到**的目录，而不是它自己
    /// 站的那个目录 —— 后者会让人以为任务指向了一个不存在的路径。
    #[test]
    fn task_root_prefers_explicit_then_payload_then_run_root() {
        let here = Path::new(r"C:\dev\cloud-stream\target\output");
        let install_to = PathBuf::from(r"C:\Users\x\AppData\Local\cloud-stream");
        let asked = Path::new(r"D:\stream");

        // 1. `--dir` 压过一切推断：替另一台机器渲染靠它。
        assert_eq!(
            choose_task_root(Some(asked), true, Some(install_to.clone()), here),
            PathBuf::from(r"D:\stream")
        );
        assert_eq!(
            choose_task_root(Some(asked), false, None, here),
            PathBuf::from(r"D:\stream")
        );

        // 2. 自带 payload（安装器角色）：指向将要解压的地方。
        assert_eq!(
            choose_task_root(None, true, Some(install_to.clone()), here),
            install_to
        );

        // 3. 拿不到 `%LOCALAPPDATA%`：退回运行根。真装时 `unpack` 会在同一处
        //    弹框报错，所以这里给一个能看的路径，而不是拒绝输出 XML。
        assert_eq!(choose_task_root(None, true, None, here), here);

        // 4. 已经装在运行根里的那一份：运行根就是目标（payload 无从谈起）。
        assert_eq!(
            choose_task_root(None, false, Some(PathBuf::from(r"C:\elsewhere")), here),
            here
        );
    }

    /// 成败只看退出码，不看文案。
    ///
    /// 第一条用例是 .150 上真实发生的事：`/Create` 退出码 0、任务创建成功，
    /// 而那段输出在 Rust 里（GBK 被当 UTF-8）根本不含"成功"两个字 ——
    /// 旧代码因此报了一个不存在的失败。这条测试锁死"退出码 0 就是成功"。
    #[test]
    fn create_verdict_trusts_the_exit_code() {
        assert!(create_verdict(0, "成功: 成功创建计划任务 \"MCStreamSrv\"。").is_none());
        assert!(create_verdict(0, "").is_none());
        assert!(create_verdict(0, "WARNING: something benign").is_none());

        let refused = create_verdict(1, "错误: 拒绝访问。").unwrap();
        assert!(refused.contains("退出码 1"), "{refused}");
        assert!(refused.contains("拒绝访问"), "{refused}");

        // 非 0 且没有输出时也要把退出码说出来，而不是给一个空字符串。
        let silent = create_verdict(0x8007_0005u32 as i32, "").unwrap();
        assert!(silent.contains("没有任何输出"), "{silent}");
    }
}
