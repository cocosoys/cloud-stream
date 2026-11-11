//! GUI 安装器怎么把"我做了什么"说出来。
//!
//! # 为什么不是 `println!`
//!
//! 这是个 GUI 子系统的 exe（`main.rs` 顶部 `windows_subsystem = "windows"`）：
//! 双击运行时根本没有控制台，`println!` 去的是一个没人看的 NUL。而安装这件事
//! 恰恰**必须**留下证据 —— 装到哪了、第几步、为什么失败，用户事后会来问。
//!
//! # 为什么是这一个文件
//!
//! `%ProgramData%\cloud-stream\last-install.txt` 是 `install.cmd` 一直在写的
//! 那个路径（`toolchain/install.cmd` 的 `:result_begin`），控制台窗口里的
//! `watch_install_result` 正盯着它。**格式刻意保持一致**：
//!
//! ```text
//! cloud-stream install
//! mode    : install
//! target  : C:\Users\x\AppData\Local\cloud-stream
//! started : 2026-11-12 10:31:02
//!  [1/4] ...
//! === RESULT: OK ===
//! finished: 2026-11-12 10:31:19
//! ```
//!
//! 于是控制台那一侧不需要改一行代码：它只认 `=== RESULT:` 这个标记，不关心
//! 结论是 cmd 写的还是这个 exe 写的。这也是"一个后端、多个入口"那条线在这里
//! 的延续 —— 换掉实现，不换掉契约。
//!
//! # 为什么固定路径而不是 `%TEMP%`
//!
//! 提权可能切换账户，那时提权进程的 `%TEMP%` 是调用方读不到的地方。
//! `ProgramData` 对两侧是同一个字符串，也不用在这条链上给路径加引号。
//!
//! # 写不进去怎么办
//!
//! 退回 `<目标目录>\logs\setup.log`。`ProgramData` 在受策略约束的机器上可能
//! 不可写，而"装完了但一个字都没留下"是最难排查的一种结果 —— 退到一个一定
//! 可写的地方（目标目录是这次安装自己建的），至少现场还在。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 结论文件。`None` 表示还没定落点（`start` / `resume` 时决定）。
pub struct Report {
    path: Mutex<Option<PathBuf>>,
}

impl Report {
    pub const fn new() -> Self {
        Report {
            path: Mutex::new(None),
        }
    }

    /// 定落点并写抬头（**截断**旧的）。
    ///
    /// 装一次是一次事件：上一次的 OK 留在文件开头，会让人以为这次也成功了。
    pub fn start(&self, target: &Path, mode: &str) -> bool {
        if !self.open(target, true) {
            return false;
        }
        self.note("cloud-stream install");
        self.note(&format!("mode    : {mode}"));
        self.note(&format!("target  : {}", target.display()));
        self.note(&format!("started : {}", stamp()));
        true
    }

    /// 续写同一次事件的下一段（提权之后的 `--finish` 阶段用）。
    ///
    /// 不截断：上一个阶段已经写了解压结果，抹掉等于抹掉"文件是哪来的"。
    pub fn resume(&self, target: &Path) -> bool {
        self.open(target, false)
    }

    fn open(&self, target: &Path, truncate: bool) -> bool {
        for candidate in candidates(target) {
            if let Some(parent) = candidate.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut opts = std::fs::OpenOptions::new();
            opts.create(true).write(true);
            if truncate {
                opts.truncate(true);
            } else {
                opts.append(true);
            }
            if opts.open(&candidate).is_ok() {
                *self.path.lock().unwrap_or_else(|e| e.into_inner()) = Some(candidate);
                return true;
            }
        }
        false
    }

    /// 追加一行。写不进去就静默 —— 安装器不该因为写日志失败而改变结论。
    pub fn note(&self, line: &str) {
        let path = match self.path() {
            Some(p) => p,
            None => return,
        };
        // 每次都重开：这样控制台那一侧随时能读到已经写下的部分（std 的
        // `File` 默认带 FILE_SHARE_READ，不会把读者挡在外面）。二十来次
        // 打开关掉的代价，比"结论只在最后一次性落地"小得多。
        let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
            return;
        };
        let _ = writeln!(file, "{line}");
    }

    /// 写终局标记。控制台认的就是这一行。
    pub fn end(&self, verdict: &str) {
        self.note("");
        self.note(&format!("=== RESULT: {verdict} ==="));
        self.note(&format!("finished: {}", stamp()));
    }

    pub fn path(&self) -> Option<PathBuf> {
        self.path.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// 候选落点，按优先级。
///
/// 第一项是 `install.cmd` 用了很久的那个固定路径；第二项是这次安装自己的
/// `logs\` —— 只有第一项写不进去时才会用到，而那时它是唯一一定可写的地方。
fn candidates(target: &Path) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(2);
    if let Some(program_data) = std::env::var_os("ProgramData") {
        let base = PathBuf::from(program_data);
        if !base.as_os_str().is_empty() {
            out.push(base.join("cloud-stream").join("last-install.txt"));
        }
    }
    out.push(target.join("logs").join("setup.log"));
    out
}

/// 本地时间戳，`%Y-%m-%d %H:%M:%S`。
///
/// 不用 `chrono`：整个 crate 为此引一个日期库不值当，而安装记录不需要时区
/// 处理 —— 它只回答"这是哪一次"。
pub fn stamp() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cs-report-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 抬头、经过、终局标记三段都得在，而且顺序不能乱 —— 控制台那一侧靠
    /// `=== RESULT:` **在最后**来判断"这一次说完了"。
    #[test]
    fn writes_header_body_and_verdict() {
        let dir = tmp_dir("ok");
        let report = Report::new();
        // 直接指定落点，绕开 %ProgramData%（测试不该往系统目录里写东西）。
        *report.path.lock().unwrap() = Some(dir.join("last-install.txt"));
        report.note("cloud-stream install");
        report.note(" [1/4] 解压");
        report.end("OK");

        let text = std::fs::read_to_string(dir.join("last-install.txt")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "cloud-stream install");
        assert!(lines.iter().any(|l| l.contains("[1/4]")));
        assert!(lines.iter().any(|l| l.trim() == "=== RESULT: OK ==="));
        assert!(lines.iter().any(|l| l.starts_with("finished:")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `resume` 不能截断：提权阶段续写的时候，解压那一半的经过必须还在。
    #[test]
    fn resume_appends_instead_of_truncating() {
        let dir = tmp_dir("resume");
        let path = dir.join("last-install.txt");
        std::fs::write(&path, "第一个阶段\n").unwrap();
        let report = Report::new();
        *report.path.lock().unwrap() = Some(path.clone());
        report.note("第二个阶段");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("第一个阶段"), "旧内容被截掉了：{text:?}");
        assert!(text.contains("第二个阶段"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 落点不确定时必须退到目标目录，而不是"什么都不写"。
    #[test]
    fn falls_back_to_target_logs() {
        let dir = tmp_dir("fallback");
        let candidates = candidates(&dir);
        assert_eq!(candidates.last().unwrap(), &dir.join("logs").join("setup.log"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
