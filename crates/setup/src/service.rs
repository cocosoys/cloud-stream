//! 开 / 停服务：一个按钮，两条 `schtasks` 命令，外加一次进程收尾。
//!
//! # 为什么"停止"要动看门狗
//!
//! 看门狗就是那个每分钟触发一次的计划任务：`MultipleInstancesPolicy=IgnoreNew`
//! —— 活着的实例挡住新实例，死掉之后下一次触发把它拉回来。这意味着**单独结束
//! 进程不算停止**：最多一分钟它就回来了，而点按钮的人只会认为"这个按钮没用"。
//!
//! 所以停止的第一步是 `/Change /DISABLE` 把那只手按住，`/End` 只能排在它后面。
//! 反过来的话，`/End` 之后到 `/DISABLE` 之前有一个最长一分钟的窗口，看门狗正好
//! 会在那儿把服务拉回来。
//!
//! 开始则相反：先 `/ENABLE`（上一轮停止把它关过），再 `/Run`。
//!
//! # 为什么不需要管理员
//!
//! 因为任务是以**当前账户的交互式令牌**注册的（`LogonType=InteractiveToken` +
//! `RunLevel=LeastPrivilege`，见 `install::task_xml`）：动自己的任务不是提权
//! 操作。这三条是绑在一起的 —— 哪天有人把 `RunLevel` 改回 `Highest` 或把任务
//! 注册到 SYSTEM，`/Run` 就需要管理员，而这个窗口每次都弹 UAC 的话没人会用。
//!
//! # 端口不在这里猜
//!
//! 启动之后要等应答，等的是**配置里那个端口**（`--print-config json`，
//! `install::effective_config`）。写死 8080 在改过端口的机器上会得到"起来了
//! 但没应答"这种自相矛盾的结论。
//!
//! # 停止为什么顺手还录音设备
//!
//! 服务启动时把默认录音设备换成虚拟声卡，被结束的时候没人问它一句 —— 于是
//! "停一下服务"会连带让这台机器上的会议软件没声音。还原和卸载走同一个入口
//! （`--restore-mic`），只是这里没有后续的"删目录"，所以不必复制那份备份逻辑。
//!
//! # 和窗口的关系
//!
//! 模块头部那条"没有第二个实现"在这里同样成立：窗口上的按钮调的就是
//! `--service start|stop` 落到的那两个函数。加这个模块**不是**为了让界面能
//! 有一条命令行做不到的路。

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::install::{self, TASK_NAME};
use crate::sys;
use crate::uninstall::{self, TaskState};

/// 等服务应答的上限。和安装时的 60 秒不同：那里是"装完必须证明它能连"，
/// 这里是"点一下按钮，窗口别像个死程序"，慢的机器上刷新一下就是了。
const READY_SECONDS: u32 = 15;

/// 进程连影子都没出现多久就放弃。
///
/// `schtasks /Run` 是同步返回的（它只说"请求送到了"），所以四秒还没见到进程
/// 就意味着那边已经明确失败了。等满 15 秒只是让人多盯一会儿。
const GHOST_SECONDS: u32 = 4;

/// 停完之后等进程消失的上限。
const GONE_SECONDS: u32 = 10;

/// `--service` 的动词。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Start,
    Stop,
}

impl Action {
    /// 从原始命令行里取动词。没给、给了别的，都算用法错误 —— 静默地什么也
    /// 不做会让"我敲了命令，怎么没反应"变成一次排查。
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            if arg != "--service" {
                continue;
            }
            return match it.next().map(String::as_str) {
                Some("start") => Ok(Self::Start),
                Some("stop") => Ok(Self::Stop),
                Some(other) => Err(format!("--service 只认 start / stop，收到 {other:?}")),
                None => Err("--service 后面要跟 start 或 stop".to_owned()),
            };
        }
        Err("--service 后面要跟 start 或 stop".to_owned())
    }
}

/// 一次动作的结论：成没成，以及要贴进日志的行。
///
/// 成败用布尔而不是"看哪行以 `[fail]` 开头"：判成败看文本是这个仓里已经踩过
/// 的坑（`install::create_verdict` 的注释），而那几行是给人看的。
pub struct Done {
    pub ok: bool,
    pub lines: Vec<String>,
}

impl Done {
    fn good(lines: Vec<String>) -> Self {
        Self { ok: true, lines }
    }

    fn bad(lines: Vec<String>) -> Self {
        Self { ok: false, lines }
    }
}

/// 停止 / 开始时**要做哪几件事**：抽成表是为了让顺序能被测试钉住。
///
/// 顺序是这套逻辑里唯一"写反了也不报错"的东西：`/End` 与 `/DISABLE` 交换一下，
/// 两个命令都成功、退出码都是 0，只有服务在一分钟内悄悄回来。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    /// `schtasks /Change /ENABLE`：让看门狗重新接手。
    Enable,
    /// `schtasks /Change /DISABLE`：按住看门狗。
    Disable,
    /// `schtasks /Run`：立刻拉起一次。
    Run,
    /// `schtasks /End`：结束当前实例。
    End,
    /// 没有任务（便携包）时直接起进程 —— 这台机器没有看门狗。
    Spawn,
    /// 收掉任务没管到的残留进程。
    Kill,
    /// 把默认录音设备还回去。
    Mic,
}

fn start_plan(task: TaskState) -> &'static [Step] {
    match task {
        // 便携包：没有任务可 Run。直接起进程，并在日志里说清楚"这次起来的
        // 不会自己回来"—— 一个悄悄没有看门狗的服务比拒绝启动更坏。
        TaskState::Absent => &[Step::Spawn],
        // 被上一轮"停止"禁用过：不先启用，这次起的服务只活到下次重启。
        TaskState::Disabled => &[Step::Enable, Step::Run],
        TaskState::Enabled | TaskState::Unknown => &[Step::Run],
    }
}

fn stop_plan(task: TaskState) -> &'static [Step] {
    match task {
        TaskState::Absent => &[Step::Kill, Step::Mic],
        // Disabled 也照样发一次 /DISABLE：一条幂等命令，比在这里分叉便宜。
        TaskState::Enabled | TaskState::Disabled | TaskState::Unknown => {
            &[Step::Disable, Step::End, Step::Kill, Step::Mic]
        }
    }
}

/// 这个目录里在跑的 `stream-server.exe`。按完整路径数（见
/// `uninstall::server_pids_under`），同一台机器上另一个安装不算。
pub(crate) fn pids(root: &Path) -> Vec<u32> {
    uninstall::server_pids_under(root)
}

pub(crate) fn running(root: &Path) -> bool {
    !pids(root).is_empty()
}

/// 按钮该写什么。标签说的是**点下去会发生什么**，所以它跟着状态翻：
/// 在跑写成"停止服务"，没跑写成"开始服务"。
///
/// 四个汉字是上限（按钮宽 140，字号 16），改文案要连同布局一起看。
pub(crate) fn power_label(running: bool) -> &'static str {
    if running {
        "停止服务"
    } else {
        "开始服务"
    }
}

/// 启动。
///
/// `ok` 的口径是"**看到应答 + 看门狗归位**"：进程在、端口也应答了才算成；只
/// 起来一半（进程在但端口不应答、或者任务没启用成功）也返回 false —— 那两种
/// 情况都要人看一眼，而一句标成「已启动」的日志不会有人回头看。
pub(crate) fn start(root: &Path) -> Done {
    let exe = root.join("stream-server.exe");
    if !exe.exists() {
        return Done::bad(vec![format!(
            "[fail] 这里没有 stream-server.exe，起不了：{}",
            root.display()
        )]);
    }
    let began = Instant::now();
    // 端口在**启动之前**问：`--print-config` 是一次短命调用（不绑端口、不多实例
    // 锁），但它没必要和正在起的那个进程抢同一段初始化。
    let port = install::effective_config(&exe).port;
    let before = pids(root);
    let already = !before.is_empty();
    // 这一轮真的发过 `/Run`（用来决定要不要补第二枪，见循环之后那段）。
    let mut asked = false;
    let mut lines = Vec::new();
    // 启用 / 禁用失败意味着"这次能起来，但看门狗不接手"，那是要说的，
    // 不该被后面那句"[ok] 已启动"盖过去。
    let mut clean = true;

    for step in start_plan(uninstall::task_state(TASK_NAME)) {
        match step {
            Step::Enable => match schtasks(&["/Change", "/TN", TASK_NAME, "/ENABLE"]) {
                Ok((0, _)) => lines.push("[ok] 看门狗已重新启用".to_owned()),
                Ok((code, text)) => {
                    clean = false;
                    lines.push(format!(
                        "[warn] 重新启用看门狗失败（退出码 {code}）：{}",
                        text.trim()
                    ));
                    lines.push("       这次起来的服务只活到下次重启，看门狗不会接手".to_owned());
                }
                Err(err) => {
                    clean = false;
                    lines.push(format!("[warn] 启用看门狗：{err}"));
                }
            },
            Step::Run => {
                // 已经在跑就别再 /Run 一次：`MultipleInstancesPolicy=IgnoreNew`
                // 会挡住它，而 schtasks 那边给的是"任务正在运行"—— 一句听起来
                // 像失败、实际什么都没发生的话。
                if already {
                    lines.push(format!("-- 已经在跑（pid {}），没有再起一个", join(&before)));
                } else {
                    asked = true;
                    match schtasks(&["/Run", "/TN", TASK_NAME]) {
                        Ok((0, _)) => lines.push("[ok] 已让看门狗拉起来".to_owned()),
                        Ok((code, text)) => lines.push(format!(
                            "[warn] schtasks /Run 退出码 {code}：{}（继续看它起没起来）",
                            text.trim()
                        )),
                        Err(err) => {
                            clean = false;
                            lines.push(format!("[fail] 起不了 schtasks：{err}"));
                        }
                    }
                }
            }
            Step::Spawn => {
                // 便携包里也一样：已经在跑就不再起第二个（服务自己有单实例
                // 互斥量，第二个会立刻 exit 0，日志里平白多一行"已有一个实例
                // 在跑"）。
                if already {
                    lines.push(format!("-- 已经在跑（pid {}），没有再起一个", join(&before)));
                } else {
                    match std::process::Command::new(&exe)
                        .arg("--run")
                        .current_dir(root)
                        .creation_flags(sys::CREATE_NO_WINDOW)
                        .spawn()
                    {
                        Ok(child) => {
                            lines.push(format!("[ok] 已直接启动（pid {}）", child.id()));
                            lines.push(
                                "       这台机器没有看门狗任务（多半是便携包直接解出来的）：\
                                 进程退掉或者重启之后不会自己回来"
                                    .to_owned(),
                            );
                        }
                        Err(err) => {
                            lines.push(format!("[fail] 起不了 {}：{err}", exe.display()));
                            return Done::bad(lines);
                        }
                    }
                }
            }
            Step::Disable | Step::End | Step::Kill | Step::Mic => {}
        }
    }

    // 补一枪：`/End` 之后任务计划程序有时还会认为上一个实例在跑，这时 `/Run`
    // 被 `IgnoreNew` 挡掉，而它给的话是「任务正在运行」—— 听起来像成功，实际
    // 什么都没起。等三秒再要一次，比让点按钮的人去猜"为什么没起来"便宜得多。
    if asked && !wait_process(root, 3) {
        lines.push("-- 三秒还没见到进程，再要一次（上一次可能撞上「任务正在运行」）".to_owned());
        match schtasks(&["/Run", "/TN", TASK_NAME]) {
            Ok((0, _)) => {}
            Ok((code, text)) => lines.push(format!(
                "[warn] 第二次 schtasks /Run 退出码 {code}：{}",
                text.trim()
            )),
            Err(err) => lines.push(format!("[fail] 起不了 schtasks：{err}")),
        }
    }

    let ready = wait_ready(root, port);
    // 计时要在**等完之后**取：把 15 秒的等待排除在外，会写出"用时 0.4 秒"
    // 这种和服务实际起来的时间无关的数。
    let elapsed = began.elapsed().as_secs_f32();
    match ready {
        Ready::Yes(pids) => {
            lines.push(format!(
                "[ok] 服务已启动（pid {}；{port} 正在监听；用时 {elapsed:.1} 秒）",
                join(&pids)
            ));
            if clean {
                Done::good(lines)
            } else {
                Done::bad(lines)
            }
        }
        Ready::NoProcess => {
            lines.push(format!(
                "[fail] 进程没起来（{GHOST_SECONDS} 秒内一次都没见到）。\
                 这不是「等一会儿就好」的那种失败：先看 stream-server.exe --diagnose 的结论"
            ));
            Done::bad(lines)
        }
        Ready::NoAnswer(pids) => {
            lines.push(format!(
                "[warn] 进程在（pid {}），但 {port} 在 {READY_SECONDS} 秒内没有应答。\n\
                 慢的机器上采集初始化会更久：过一会儿点「刷新状态」再看，\
                 一直不应答就 stream-server.exe --diagnose",
                join(&pids)
            ));
            Done::bad(lines)
        }
    }
}

/// 停止：禁用看门狗 → 结束任务实例 → 收残留进程 → 还录音设备 → 等它真的没了。
pub(crate) fn stop(root: &Path) -> Done {
    let exe = root.join("stream-server.exe");
    // 端口要在杀进程**之前**问（`--print-config` 是一次短命调用，不受影响），
    // 杀完再问也读得出来，但那时已经没进程可问配置了。
    let port = install::effective_config(&exe).port;
    let mut lines = Vec::new();
    let mut clean = true;

    let before = uninstall::task_state(TASK_NAME);
    if !running(root) {
        match before {
            TaskState::Absent => lines
                .push("-- 服务没在跑，这台机器也没有看门狗任务（便携包），这次只补一遍收尾".to_owned()),
            TaskState::Disabled => {
                lines.push("-- 服务没在跑，看门狗已经是禁用状态，这次只补一遍收尾".to_owned())
            }
            _ => {}
        }
    }

    for step in stop_plan(before) {
        match step {
            Step::Disable => match schtasks(&["/Change", "/TN", TASK_NAME, "/DISABLE"]) {
                Ok((0, _)) => {
                    lines.push("[ok] 看门狗已禁用（它不会再每分钟把服务拉回来）".to_owned())
                }
                // 这一条失败会让"停止"变成"暂停一分钟"：进程杀了，看门狗下一次
                // 触发又把它拉起来。必须说清楚，而不是继续往下走然后报成功。
                Ok((code, text)) => {
                    clean = false;
                    lines.push(format!(
                        "[fail] 没能禁用看门狗（退出码 {code}）：{}",
                        text.trim()
                    ));
                    lines.push("       它每分钟检查一次：服务会在一分钟内被它拉回来".to_owned());
                }
                Err(err) => {
                    clean = false;
                    lines.push(format!("[fail] 起不了 schtasks：{err}"));
                }
            },
            Step::End => match schtasks(&["/End", "/TN", TASK_NAME]) {
                Ok((0, _)) => lines.push("[ok] 已结束任务实例".to_owned()),
                // 不致命：下面还要按路径收进程，那一种不依赖任务状态。
                Ok((code, text)) => lines.push(format!(
                    "[warn] schtasks /End 退出码 {code}：{}（继续按路径收进程）",
                    text.trim()
                )),
                Err(err) => lines.push(format!("[warn] schtasks /End：{err}")),
            },
            Step::Kill => {
                let killed = uninstall::kill_servers_under(root, None);
                if killed > 0 {
                    lines.push(format!("[ok] 结束了 {killed} 个还在跑的进程"));
                }
            }
            Step::Mic => restore_mic(&exe, root, &mut lines),
            Step::Enable | Step::Run | Step::Spawn => {}
        }
    }

    if !wait_gone(root) {
        clean = false;
        let left = pids(root);
        lines.push(format!(
            "[fail] {GONE_SECONDS} 秒后还有 {} 个进程没退（pid {}）",
            left.len(),
            join(&left)
        ));
    }

    // 进程没了但端口还有应答 —— 那不是"没停掉"，是**别的东西**占着这个端口
    // （另一个目录的安装，或者恰好同端口的别的程序）。这两件事的处置完全不同。
    if port_answers(port) {
        lines.push(format!(
            "[warn] {port} 还有应答：这个目录的进程已经没了，应答来自别处\
             （另一个目录的安装，或者别的程序占着这个端口）"
        ));
    }

    let still = running(root);
    // 收尾那句话按**这台机器实际有什么**来说：便携包没有看门狗，说"看门狗已
    // 禁用"是编的；有任务却没禁掉（上面 clean 已经是 false 的那条路）也不能说
    // "不会再起来"。所以标签和结论都从任务状态里取，而不是写死一个"已停止"。
    let (tag, watchable, watchdog) = match uninstall::task_state(TASK_NAME) {
        TaskState::Absent => ("ok", true, "这台机器没有看门狗任务，重启后也不会自己起来"),
        TaskState::Disabled => ("ok", true, "看门狗已禁用，重启后也不会自己起来"),
        TaskState::Enabled => (
            "warn",
            false,
            "看门狗还在启用 —— 它下一次触发会把服务拉回来",
        ),
        TaskState::Unknown => ("warn", false, "看门狗状态读不出来，别指望它不会回来"),
    };
    if clean && !still {
        let what = if watchable {
            "服务已停止"
        } else {
            "进程已结束，但这不算停下来"
        };
        lines.push(format!(
            "[{tag}] {what}（{port} 已关闭；{watchdog}）。点「开始服务」恢复"
        ));
    }
    if clean && !still && watchable {
        Done::good(lines)
    } else {
        Done::bad(lines)
    }
}

/// `--service start|stop` 的命令行入口。
///
/// 存在理由和窗口上那个按钮是同一个：两边落到同一对函数，于是"界面上能点、
/// 命令行里做不到"这种事不会出现。退出码：0 成功，1 没做成，2 参数不认识。
pub fn run(args: &[String]) -> i32 {
    let action = match Action::parse(args) {
        Ok(action) => action,
        Err(msg) => {
            // 和 `--task-xml` 一样走 stderr 而不是弹框：这条命令天生是给终端和
            // 脚本用的，弹框会让调用方永远等在那儿。
            eprintln!("{msg}");
            return 2;
        }
    };
    let root = crate::run_root();
    let done = match action {
        Action::Start => start(&root),
        Action::Stop => stop(&root),
    };
    for line in &done.lines {
        println!("{line}");
    }
    if done.ok {
        0
    } else {
        1
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 只差 `/TN` 之前那一个动词的几条命令。抽出来是为了让四处的写法一致 ——
/// 少写一个 `/TN` 时 `schtasks` 会把它当成"列出全部任务"，而那个错误很难看出来。
fn schtasks(args: &[&str]) -> Result<(i32, String), String> {
    sys::run("schtasks", args)
}

fn join(pids: &[u32]) -> String {
    pids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn port_answers(port: u16) -> bool {
    let Ok(addr) = format!("127.0.0.1:{port}").parse() else {
        return false;
    };
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}

/// 等服务"进程在 + 端口应答"。见 `start` 里对三种结局的处理。
enum Ready {
    Yes(Vec<u32>),
    /// 一次都没见到进程。
    NoProcess,
    /// 进程在，但端口一直不应答。
    NoAnswer(Vec<u32>),
}

fn wait_ready(root: &Path, port: u16) -> Ready {
    let mut seen = false;
    for tick in 0..READY_SECONDS * 4 {
        let live = pids(root);
        if live.is_empty() {
            if !seen && tick >= GHOST_SECONDS * 4 {
                return Ready::NoProcess;
            }
        } else {
            seen = true;
            if port_answers(port) {
                return Ready::Yes(live);
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let live = pids(root);
    if live.is_empty() {
        Ready::NoProcess
    } else {
        Ready::NoAnswer(live)
    }
}

/// 等目录里出现 `stream-server.exe`（只要进程，不管端口）。
fn wait_process(root: &Path, seconds: u32) -> bool {
    for _ in 0..seconds * 4 {
        if !pids(root).is_empty() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    !pids(root).is_empty()
}

fn wait_gone(root: &Path) -> bool {
    for _ in 0..GONE_SECONDS * 4 {
        if pids(root).is_empty() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    pids(root).is_empty()
}

/// 把默认录音设备还回去。和卸载里那一段同源，只是没有后续的"删目录"，
/// 所以不复制那份"记录留到 %TEMP%"的兜底。
///
/// 能力探测必须在执行之前：旧版本 exe 不认识 `--restore-mic`，
/// `Config::parse` 会忽略不认识的参数，于是它起的是一个完整的服务（永不返回、
/// 占住端口）。那正是"停止服务"变成"又起了一个"的成因。
fn restore_mic(exe: &Path, root: &Path, lines: &mut Vec<String>) {
    if !exe.exists() {
        return;
    }
    if !sys::exe_has_marker(exe, "MICRESTORE") {
        lines.push("-- 这个版本没有 --restore-mic，录音设备只能手工还回去（设置 → 系统 → 声音）".to_owned());
        return;
    }
    // 20 秒：exe 自己保证 5 秒内退（见服务端 app.rs），这里只是不让"它没退"
    // 变成一次永远等下去的停止。
    match sys::run_bounded(exe, &["--restore-mic"], root, 20) {
        Ok((code, text)) => {
            let done = text.contains("MICRESTORE restored")
                || text.contains("MICRESTORE nothing-to-do");
            if done {
                lines.push("[ok] 默认录音设备已还原".to_owned());
            } else {
                lines.push(format!(
                    "[warn] 没能确认录音设备还原（退出码 {code}）：{}",
                    text.trim()
                ));
            }
        }
        Err(err) => lines.push(format!("[warn] 起不了 --restore-mic：{err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_takes_start_and_stop() {
        let args = |verb: &str| vec!["--service".to_owned(), verb.to_owned()];
        assert_eq!(Action::parse(&args("start")), Ok(Action::Start));
        assert_eq!(Action::parse(&args("stop")), Ok(Action::Stop));
        // 和 `--ops` 一样：前后夹着别的开关也能认出来。
        assert_eq!(
            Action::parse(&["--dir".to_owned(), "stop".to_owned(), "--service".to_owned(), "stop".to_owned()]),
            Ok(Action::Stop)
        );
    }

    #[test]
    fn parse_rejects_junk() {
        assert!(Action::parse(&["--service".to_owned()]).is_err());
        assert!(Action::parse(&["--service".to_owned(), "halt".to_owned()]).is_err());
        // 大小写不宽容：`--ops` 那边是宽容的，这里刻意不 —— 只认两条命令，
        // 敲错了要当场报出来，而不是猜一个。
        assert!(Action::parse(&["--service".to_owned(), "Start".to_owned()]).is_err());
    }

    /// 停止的顺序：**先禁看门狗，再结束实例**。
    ///
    /// 写反了不会报错：两条命令都成功，只是服务在一分钟内被看门狗拉回来 ——
    /// 现场看起来像"停止按钮没用"。
    #[test]
    fn stop_disables_the_watchdog_before_ending_it() {
        for task in [TaskState::Enabled, TaskState::Disabled, TaskState::Unknown] {
            let plan = stop_plan(task);
            let disable = plan.iter().position(|s| *s == Step::Disable);
            let end = plan.iter().position(|s| *s == Step::End);
            assert!(disable.is_some(), "{task:?} 的停止计划里没有禁用看门狗");
            assert!(
                disable < end,
                "{task:?} 的计划先结束实例再禁用看门狗：中间那一分钟它会自己回来"
            );
        }
    }

    /// 开始必须先启用再 Run：被"停止"关过的任务，`/Run` 起起来的那一次
    /// 只活到下次重启。
    #[test]
    fn start_enables_before_running() {
        assert_eq!(start_plan(TaskState::Disabled), &[Step::Enable, Step::Run]);
        assert_eq!(start_plan(TaskState::Enabled), &[Step::Run]);
    }

    /// 便携包（没有计划任务）不该发出任何 schtasks 命令 —— 那些命令会全部失败，
    /// 而失败信息会盖住真正的情况："这台机器没有看门狗"。
    #[test]
    fn portable_install_never_touches_the_scheduler() {
        assert_eq!(start_plan(TaskState::Absent), &[Step::Spawn]);
        let plan = stop_plan(TaskState::Absent);
        assert!(!plan.contains(&Step::Disable));
        assert!(!plan.contains(&Step::End));
        // 仍然要收进程、还录音设备。
        assert!(plan.contains(&Step::Kill) && plan.contains(&Step::Mic));
    }

    /// 标签说的是"点下去会发生什么"，而不是"现在是什么状态"。
    #[test]
    fn power_label_says_what_the_click_does() {
        assert_eq!(power_label(true), "停止服务");
        assert_eq!(power_label(false), "开始服务");
        // 四个汉字是按钮的上限（宽 140，见 `MAIN_LAYOUT`）。
        assert!(power_label(true).chars().count() <= 4);
        assert_eq!(power_label(true).chars().count(), power_label(false).chars().count());
    }
}
