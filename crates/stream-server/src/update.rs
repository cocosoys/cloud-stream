//! 更新器：把"新的一包"换成"正在跑的这份"，换坏了能退回去。
//!
//! 四动作矩阵里「更新」这一行原本只有「失败可回滚」和「版本可判断」是通的，
//! 「可视化」和「程序可调用」两格空着 —— 更新这件事本身只能靠开发机上的
//! `deploy.ps1` 手工完成。这个模块是那两格。
//!
//! # 一次更新由四段组成
//!
//! ```text
//! 1. 问包      包里那个 stream-server.exe 自己说它是哪一版
//! 2. 比版本    比当前这一份新，才继续（--update-force 可以跳过这道门）
//! 3. 留底+覆盖 先拍一份完整快照，再逐个文件拷过去；锁着的改名让路
//! 4. 验版本    再问一次"你现在是哪一版"，答不上来或答错就自动回滚
//! ```
//!
//! 第 4 段是这个模块存在的理由。没有它，"更新"就只是"拷贝一组文件"——
//! 拷完之后机器处于什么状态，没有人知道。有了它，最坏结果是**退回原样**，
//! 而不是"起不来了，去翻文档吧"。
//!
//! # 更新包的形状
//!
//! 就是一个和运行目录同构的目录（`deploy.ps1` 打出来的那种）：
//!
//! ```text
//! update\
//!   stream-server.exe
//!   start-stream.cmd
//!   selkies-web\...
//! ```
//!
//! 默认位置是 exe 旁边的 `update\`，也可以用 `--update-from` 指到别处。
//! 刻意**不支持** zip 和 URL：那需要引入解压库和一个 HTTP 客户端，而当前的
//! 部署链（`deploy.ps1` 拷目录到目标机）本来就是目录形态。等真有"从网上下"
//! 的需求时，加的是"把 zip 解到 `update\`"这一步，而不是改这个模块的语义。
//!
//! # 两处刻意的不对称
//!
//! * **只增不删。** 包里有、目录里没有的文件会被加进去；目录里有、包里没有的
//!   文件**不动**。删文件的判断只能靠一份"应该有什么"的清单，而猜错一次就是
//!   删掉用户的存档。旧版本留下的多余 DLL 占几百 KB，代价远小于误删。
//! * **让路的文件不删，改名。** 锁着的文件（正在跑的就是 exe 自己）先改名成
//!   `.rollback-old` 再拷新的，和 `ops::rollback` 同一套手法：判错一次不该
//!   让人丢文件。
//!
//! # 谁不能覆盖
//!
//! 包里带的那几份"这台机器是谁"的文件 —— 配置、令牌、证书 —— **默认不覆盖**。
//! 它们和机器绑定（端口、口令、证书指纹），而包里的那一份是模板：一辆车的
//! 合格证不该在下一次保养时被换成新车的那张。要覆盖得显式 `--update-overwrite-config`。

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::ops;

/// 更新包的默认落地位置：`%ProgramData%\cloud-stream\update`。
///
/// 三条理由，按重要性排：
///
/// 1. **不在运行目录里。** 更新前要拍一份完整快照，而快照扫的是运行目录；
///    包要是住在里面，每更新一次就把几十兆的安装包也备一份，回滚时还会把包
///    一起"还原"（内容一样，但屏幕上看起来像回滚没生效）。把它放在外面，
///    快照的语义就是纯粹的"这台机器原来的样子"。
/// 2. **不随卸载消失。** 卸载会 `rmdir /s` 整个安装目录。包在外面留着，
///    "卸载 → 重装"可以直接用同一份，不用再传一次三十兆。
/// 3. **和卸载备份同一处**（`%ProgramData%\cloud-stream\`）。运维只需要记住
///    一个目录：这个产品的数据在 `ProgramData\cloud-stream` 下面。
///
/// 也可以用 `--update-from` 指到任何地方；这个默认值只负责"最不用解释的
/// 那种用法不需要敲参数"。
pub fn default_source_dir(root: &Path) -> PathBuf {
    if let Some(program_data) = std::env::var_os("ProgramData") {
        if !program_data.is_empty() {
            return PathBuf::from(program_data)
                .join("cloud-stream")
                .join("update");
        }
    }
    // 取不到环境变量（极少见：非 Windows 或环境被清过）：退到安装目录的
    // **外面**。宁可让人多找一个目录，也不要落进运行目录里 —— 那会把上面
    // 第 1 条的理由反过来。
    root.parent()
        .map(|p| p.join("cloud-stream-update"))
        .unwrap_or_else(|| root.join("update"))
}

/// 更新包里这两类目录永远不参与覆盖。
///
/// `state`：快照就住在里面，把包里的空目录盖上去等于抹掉全部回退历史。
/// `logs`：目标机的日志是这台机器的运行记录，不是包的内容。
const PROTECTED_DIRS: &[&str] = &["state", "logs"];

/// 只属于这台机器的文件。包里有同名的也不覆盖（除非显式要求）。
///
/// 判断只在运行根这一层做（`path` 里没有分隔符）：这几个名字出现在子目录里
/// 时不属于"这台机器的身份"，而属于前端资源的一部分。
const IDENTITY_FILES: &[&str] = &[
    "stream-config.toml",
    "config.toml",
    "auth-token.txt",
    "cert.pem",
    "cert.key",
];

/// 每次 apply 之前自动留的那一份的标签。不带 `pre-` 以外的含义，但默认的回滚
/// 目标会跳过 `pre-rollback`，这一份不会被跳过 —— 它正是"更新失败退回去"的
/// 那个落点。
pub const PRE_UPDATE_LABEL: &str = "preupdate";

/// 包里没有比当前更新的版本。
///
/// 单独一个退出码而不是 `0` + 一句"已是最新"：`check` 的调用方（脚本、界面）
/// 要能**不解析文本**就分流。`0` = 有新版可装，`5` = 没有。
pub const EXIT_UPTODATE: i32 = 5;

// ---------------------------------------------------------------------------
// 版本比较
// ---------------------------------------------------------------------------

/// 版本串 → 数字段。取每段开头的数字：`3rc1` → 3，`x` → 0。
fn numeric_parts(version: &str) -> Vec<u64> {
    version
        .trim()
        .split('.')
        .map(|part| {
            let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse::<u64>().unwrap_or(0)
        })
        .collect()
}

/// 比较两个版本号：`a` 比 `b` 新则返回 `Greater`。
///
/// 段数不齐时短的那边补 0（`0.4` == `0.4.0`），这是语义化版本的规定也是人的
/// 直觉。**不建模预发布序号**（`0.4.0-rc1` 会被读成 `0.4.0`）：这个项目的
/// 发布物只有正式版本，为一条不存在的路径写一套排序规则，读代码的人要先怀疑
/// 自己看错了。
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let (pa, pb) = (numeric_parts(a), numeric_parts(b));
    for i in 0..pa.len().max(pb.len()) {
        let x = pa.get(i).copied().unwrap_or(0);
        let y = pb.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

/// `available` 是否比 `current` 新。相等**不**算有新版：重装同一个版本是
/// `apply --update-force` 的事，"检查更新"不该在版本没动时天天报有新版。
pub fn is_newer(available: &str, current: &str) -> bool {
    compare_versions(available, current) == Ordering::Greater
}

// ---------------------------------------------------------------------------
// 清单与报告
// ---------------------------------------------------------------------------

/// 问出来的版本信息（来自 `--version json`）。
#[derive(Debug, Clone, Serialize)]
pub struct Probe {
    pub version: String,
    pub commit: String,
    /// 三态：`true` / `false` / `null`（未知）。
    pub dirty: Option<bool>,
    pub built_at: String,
}

/// 一个文件在本次更新里的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// 目录里没有，新增。
    Add,
    /// 内容不同，覆盖。
    Replace,
    /// 内容一样，不碰。
    Same,
    /// 内容不同，但这台机器的身份文件，跳过。
    Keep,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Action::Add => "add",
            Action::Replace => "replace",
            Action::Same => "same",
            Action::Keep => "keep",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanEntry {
    /// 相对运行根，`/` 分隔。
    pub path: String,
    pub action: Action,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    pub current: String,
    pub current_commit: String,
    pub source: String,
    /// 包里的 exe 报的版本；问不出来时是 `None`，原因在 `probe_error`。
    pub available: Option<Probe>,
    pub probe_error: Option<String>,
    /// 有新版可装。
    pub newer: bool,
    /// 包里**没有** exe（纯前端/DLL 更新）：无法比版本，只能靠 `--update-force`。
    pub exe_missing: bool,
    pub plan: Vec<PlanEntry>,
}

impl CheckReport {
    pub fn add(&self) -> usize {
        self.plan.iter().filter(|e| e.action == Action::Add).count()
    }
    pub fn replace(&self) -> usize {
        self.plan
            .iter()
            .filter(|e| e.action == Action::Replace)
            .count()
    }
    pub fn same(&self) -> usize {
        self.plan.iter().filter(|e| e.action == Action::Same).count()
    }
    pub fn keep(&self) -> usize {
        self.plan.iter().filter(|e| e.action == Action::Keep).count()
    }
    /// 需要动的文件数。0 表示"已经在目标版本上"。
    pub fn changes(&self) -> usize {
        self.add() + self.replace()
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ApplyReport {
    pub source: String,
    pub from_version: String,
    /// 换上去的版本；包里没有 exe 时是 `None`。
    pub to_version: Option<String>,
    /// 覆盖之前自动留的那一份。
    pub snapshot: String,
    pub written: Vec<String>,
    /// (路径, 说明)：被占用、改成 `.rollback-old` 之后才拷进去的。
    pub stashed: Vec<(String, String)>,
    pub unchanged: usize,
    pub kept: Vec<String>,
    /// 验版本通过了。（包里没有 exe 时为 `false`：没验过，不是验失败。）
    pub verified: bool,
    /// 本次的验版本口径，一句给人看的话。
    ///
    /// 报告里单独存一份，是为了让"没验过"和"验失败"在屏幕上分得开：只靠
    /// `to_version` 有没有值，会把"包里没有 exe"和"包里有 exe 但问不出版本"
    /// 混成同一句话，而这两种情况的下一步动作完全不同。
    pub expectation: String,
    /// 验版本失败之后自动回滚了。
    pub rolled_back: bool,
    /// 一句话结论；`Some` 即失败（退出码 3）。
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// 问版本
// ---------------------------------------------------------------------------

/// 跑 `<exe> --version json`，把它报的版本拿回来。
///
/// 为什么是"跑一下"而不是读 PE 的 VERSIONINFO：
///
/// * 要回答的问题不只是"这个文件标称几版"，还有"它跑得起来吗"。一个截断的
///   下载、一个缺 DLL 的包，VERSIONINFO 可能还在，但它起不来 —— 而那正是
///   最该在覆盖之前发现的情况。
/// * `--version` 早于配置解析执行，不依赖任何配置、不绑端口、不碰系统状态，
///   所以问一次的成本就是一次进程启动（几十毫秒）。
///
/// 代价是"检查更新"会执行包里那个 exe。对一条本来就要把它装上并运行的路径，
/// 这个代价是零：真正的问题是它跑不跑得起来，而不是问一句会不会太亲密。
pub fn probe(exe: &Path) -> Result<Probe, String> {
    if !exe.is_file() {
        return Err(format!("{}: 文件不存在", exe.display()));
    }
    let output = Command::new(exe).arg("--version").arg("json").output();
    let output = match output {
        Ok(out) => out,
        // 起不来通常是位数不对或缺 CRT：说清楚，别让人以为是版本号读错了。
        Err(err) => return Err(format!("{}: 起不来: {err}", exe.display())),
    };
    if !output.status.success() {
        return Err(format!(
            "{} --version 退出码 {:?}（stderr: {}）",
            exe.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|e| format!("{} --version 的输出不是 JSON: {e}（原文 {text:?}）", exe.display()))?;
    let version = value
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{} --version 的输出里没有 version 字段", exe.display()))?;
    Ok(Probe {
        version: version.to_owned(),
        commit: value
            .get("commit")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_owned(),
        dirty: value.get("dirty").and_then(|v| v.as_bool()),
        built_at: value
            .get("built_at")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_owned(),
    })
}

// ---------------------------------------------------------------------------
// 清单
// ---------------------------------------------------------------------------

/// 这个名字是不是"这台机器是谁"。
fn is_identity(rel: &str) -> bool {
    if rel.contains('/') {
        return false;
    }
    IDENTITY_FILES
        .iter()
        .any(|name| rel.eq_ignore_ascii_case(name))
}

/// 比对包与运行目录，得出每个文件该做什么。**只读**，不写任何东西。
pub fn plan(
    source: &Path,
    root: &Path,
    overwrite_identity: bool,
) -> Result<Vec<PlanEntry>, String> {
    let files = ops::collect_skipping(source, PROTECTED_DIRS)?;
    if files.is_empty() {
        return Err(format!("{}: 目录里没有任何文件", source.display()));
    }
    let mut entries = Vec::with_capacity(files.len());
    for rel in files {
        let rel_str = ops::rel_to_string(&rel);
        let src = source.join(&rel);
        let (size, hash) = ops::hash_file(&src)?;
        let dst = root.join(&rel);
        let action = if !dst.is_file() {
            Action::Add
        } else {
            let (dst_size, dst_hash) = ops::hash_file(&dst)?;
            if dst_size == size && dst_hash == hash {
                Action::Same
            } else if !overwrite_identity && is_identity(&rel_str) {
                Action::Keep
            } else {
                Action::Replace
            }
        };
        entries.push(PlanEntry {
            path: rel_str,
            action,
            size,
        });
    }
    // 覆盖顺序：exe 最后。半路失败时，正在运行的这份至少还是完整的 —— 一个
    // 停在中间状态的目录（新 DLL + 旧 exe）比"几乎没动"更难救。
    entries.sort_by(|a, b| {
        let a_exe = a.path.eq_ignore_ascii_case("stream-server.exe");
        let b_exe = b.path.eq_ignore_ascii_case("stream-server.exe");
        a_exe.cmp(&b_exe).then_with(|| a.path.cmp(&b.path))
    });
    Ok(entries)
}

// ---------------------------------------------------------------------------
// 检查
// ---------------------------------------------------------------------------

/// 只看不动：包里是哪一版、比现在新不新、会动哪些文件。
pub fn check(
    root: &Path,
    source: &Path,
    overwrite_identity: bool,
) -> Result<CheckReport, String> {
    ensure_distinct(root, source)?;
    let entries = plan(source, root, overwrite_identity)?;
    let candidate = source.join(crate::version::exe_name());
    let exe_missing = !candidate.is_file();
    let (available, probe_error) = if exe_missing {
        (None, Some("包里没有 stream-server.exe，无法判断版本".to_owned()))
    } else {
        match probe(&candidate) {
            Ok(p) => (Some(p), None),
            Err(err) => (None, Some(err)),
        }
    };
    let newer = available
        .as_ref()
        .map(|p| is_newer(&p.version, crate::version::VERSION))
        .unwrap_or(false);
    Ok(CheckReport {
        current: crate::version::VERSION.to_owned(),
        current_commit: crate::version::COMMIT.to_owned(),
        source: source.display().to_string(),
        available,
        probe_error,
        newer,
        exe_missing,
        plan: entries,
    })
}

/// 包目录和运行目录不能是同一个，也不能是运行目录的**上级**。
///
/// 前一种情况是"拿自己覆盖自己"：`plan` 会算出全部 `Same`、看起来一切正常，
/// 一种比报错更糟的成功。后一种情况更危险 —— 包目录是运行目录的上级时，
/// `plan` 会把**整个安装目录连同它自己**当成"新的一包"来枚举，然后把旧的
/// 盖到新的上面，转到几百个文件之后才发现自己在原地打转。
///
/// 反向的包含（包在运行目录**里面**）是允许的：`--update-from` 指到
/// `.\update` 是人的自然做法，而它没有上面那两个问题的任何一个 —— 只是快照
/// 会跟着把那几十兆也备进去。默认位置因此放在 `ProgramData`（见
/// [`default_source_dir`]），但别人这么用不该被拒绝。
fn ensure_distinct(root: &Path, source: &Path) -> Result<(), String> {
    let canon = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (r, s) = (canon(root), canon(source));
    if r == s {
        return Err(format!(
            "更新包目录不能是运行目录本身（{}）",
            r.display()
        ));
    }
    if r.starts_with(&s) {
        return Err(format!(
            "更新包目录不能是运行目录的上级：包 {} 包含着运行目录 {}",
            s.display(),
            r.display()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 执行
// ---------------------------------------------------------------------------

/// 拷一个文件进去，被占用就改名让路。
///
/// 让路逻辑借用 [`ops::stash`]，不在这里再写一遍：它已经处理了"目标名字被
/// 占、而且占着它的正是运行中的自己（删不掉）"这种情况 —— 更新器覆盖的
/// 往往就是自己，这条路径一定会走到。
fn copy_one(root: &Path, source: &Path, rel: &Path) -> Result<bool, String> {
    let src = source.join(rel);
    let dst = root.join(rel);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("{}: 建目录失败: {e}", parent.display()))?;
    }
    match fs::copy(&src, &dst) {
        Ok(_) => Ok(false),
        Err(err) if dst.exists() => {
            // 目标存在却拷不进去：几乎总是"正在被占用"（Windows 允许重命名
            // 运行中的 exe，不允许覆盖）。改名让路再拷一次。
            let stash = ops::stash(&dst).map_err(|e2| {
                format!(
                    "{}: 覆盖失败（{err}）；改名让路也失败（{e2}）",
                    dst.display()
                )
            })?;
            fs::copy(&src, &dst).map_err(|e3| {
                format!(
                    "{}: 让路之后仍然拷不进去（{e3}）；原文件在 {}",
                    dst.display(),
                    stash.display()
                )
            })?;
            Ok(true)
        }
        Err(err) => Err(format!("{}: 拷贝失败: {err}", dst.display())),
    }
}

/// 覆盖之后"应该验成什么样"。
///
/// 三态而不是 `Option<&str>`，因为这里有三种**不同**的情况，而把它们折成两态
/// 会造成一个真实的错误：一种是"包里没有 exe"（纯前端更新），合法地不验；
/// 另一种是"包里有 exe 但问不出它的版本"（`--update-force` 才允许走到这里），
/// 这一种**必须照验**，只是不知道期望的具体版本号 —— 至少得证明它能跑起来。
///
/// 折成两态的后果是实测撞到的：一个跑不起来的 exe 被当成"包里没有 exe"，
/// 于是跳过验证、装上、报成功。验证机制在最需要它的那一刻静默失效，比没有
/// 验证更危险，因为它给了一个假的"通过"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// 包里没有 exe：没有可问的对象，不假验。
    NotShipped,
    /// 知道期望版本：跑起来必须报这个版本。
    Version(String),
    /// 包里的 exe 问不出自己的版本（被 `--update-force` 放行）：至少要求它
    /// 跑得起来。跑不起来就回滚。
    Runnable,
}

/// 覆盖并验证。失败时自动回滚，返回的 `ApplyReport.error` 非空。
///
/// `expect` 是"换完之后应该验成什么样"，见 [`Expect`]。
pub fn apply(
    root: &Path,
    source: &Path,
    backups: &Path,
    expect: &Expect,
    overwrite_identity: bool,
) -> Result<ApplyReport, String> {
    ensure_distinct(root, source)?;
    let entries = plan(source, root, overwrite_identity)?;
    let mut report = ApplyReport {
        source: source.display().to_string(),
        from_version: crate::version::VERSION.to_owned(),
        to_version: match expect {
            Expect::Version(v) => Some(v.clone()),
            _ => None,
        },
        expectation: match expect {
            Expect::NotShipped => "包里没有 exe，不验版本".to_owned(),
            Expect::Version(v) => format!("期望 {v}"),
            Expect::Runnable => "包里问不出版本，只要求起得来".to_owned(),
        },
        ..Default::default()
    };

    // 先留底。顺序不能反：留底失败就该在动任何文件之前停下，此时目录还是
    // 原样，报错出去也不会让人怀疑"到底改到一半没有"。
    let snapshot = ops::snapshot(root, backups, PRE_UPDATE_LABEL)?;
    report.snapshot = snapshot.id.clone();

    for entry in &entries {
        match entry.action {
            Action::Same => {
                report.unchanged += 1;
                continue;
            }
            Action::Keep => {
                report.kept.push(entry.path.clone());
                continue;
            }
            Action::Add | Action::Replace => {}
        }
        let rel = ops::string_to_rel(&entry.path);
        match copy_one(root, source, &rel) {
            Ok(stashed) => {
                if stashed {
                    report
                        .stashed
                        .push((entry.path.clone(), "被占用，原文件已改名为留底".to_owned()));
                }
                report.written.push(entry.path.clone());
            }
            Err(err) => {
                // 半路失败：已经拷进去的那些必须退回去，否则目录停在
                // "新 DLL + 旧 exe"这种谁也不认识的状态。
                report.error = Some(format!(
                    "{} 拷不进去：{err}；已回滚到 {}",
                    entry.path, snapshot.id
                ));
                rollback_or_keep(root, backups, &snapshot.id, &mut report);
                return Ok(report);
            }
        }
    }

    match expect {
        // 没有期望版本号：包里根本没有 exe，没有可问的对象，不假验。
        Expect::NotShipped => {}
        // 包里有 exe 但问不出它的版本（--update-force 放行）：不知道"该是哪
        // 一版"，但仍然要证明它跑得起来。见 Expect::Runnable 的注释。
        Expect::Runnable => verify_runnable(root, backups, &snapshot.id, &mut report),
        // 验版本：问**运行目录里**那份，而不是包里那份。这一步回答的是
        // "装上去的那个东西跑起来说自己是几版"，也顺带证明它跑得起来。
        Expect::Version(expected) => {
            let installed = root.join(crate::version::exe_name());
            match probe(&installed) {
                Ok(actual) if actual.version == *expected => {
                    report.verified = true;
                }
                Ok(actual) => {
                    report.error = Some(format!(
                        "新版报的版本是 {}，期望 {expected}；已回滚到 {}",
                        actual.version, snapshot.id
                    ));
                    rollback_or_keep(root, backups, &snapshot.id, &mut report);
                }
                Err(err) => {
                    report.error =
                        Some(format!("新版起不来：{err}；已回滚到 {}", snapshot.id));
                    rollback_or_keep(root, backups, &snapshot.id, &mut report);
                }
            }
        }
    }
    Ok(report)
}

/// 不知道期望版本时的验证：只要求它跑得起来，并在报告里记下它自报的版本。
fn verify_runnable(root: &Path, backups: &Path, snapshot: &str, report: &mut ApplyReport) {
    let installed = root.join(crate::version::exe_name());
    match probe(&installed) {
        Ok(actual) => {
            report.verified = true;
            report.to_version = Some(actual.version);
        }
        Err(err) => {
            report.error = Some(format!("新版起不来：{err}；已回滚到 {snapshot}"));
            rollback_or_keep(root, backups, snapshot, report);
        }
    }
}

/// 回滚到指定快照，并把结果记进报告。
///
/// 回滚本身失败时**不**把它咽下去：此时机器处在"更新的文件已经写进去、旧版
/// 又没退回来"的状态，报告里必须同时出现两条错误 —— 只报第一条会让人以为
/// 还有救。而"回滚失败"最常见的成因就是留底名字被占（占着它的常常是正在
/// 运行的自己），所以 `ops::stash` 现在会换编号而不是直接报错。
fn rollback_or_keep(root: &Path, backups: &Path, snapshot: &str, report: &mut ApplyReport) {
    match ops::rollback(root, backups, Some(snapshot)) {
        Ok(done) => {
            report.rolled_back = true;
            if !done.failed.is_empty() {
                let detail: Vec<String> = done
                    .failed
                    .iter()
                    .map(|(path, why)| format!("{path}（{why}）"))
                    .collect();
                let tail = format!(
                    "；但回滚有 {} 个文件没成：{}",
                    detail.len(),
                    detail.join(", ")
                );
                report.error = Some(format!("{}{tail}", report.error.take().unwrap_or_default()));
            }
        }
        Err(err) => {
            let head = report.error.take().unwrap_or_default();
            report.error = Some(format!(
                "{head}；**自动回滚也失败了**：{err}。手动跑 `stream-server.exe --ops rollback`，备份在 {}",
                backups.display()
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// 命令行
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// 只看不动。
    Check,
    /// 覆盖 + 验证。
    Apply,
}

impl Verb {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "check" => Some(Self::Check),
            "apply" => Some(Self::Apply),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Request {
    pub verb: Option<Verb>,
    pub from: Option<PathBuf>,
    pub dir: Option<PathBuf>,
    pub json: bool,
    /// 不比版本，直接覆盖（重装同一版、或包里没有 exe 时用）。
    pub force: bool,
    /// 只打印计划，不写文件。
    pub dry_run: bool,
    /// 连"这台机器是谁"的文件也一起覆盖。
    pub overwrite_identity: bool,
}

impl Request {
    /// 从原始命令行（不含程序名）解析。
    ///
    /// 和 `--ops` 一样：返回 `None` = 命令行里没有 `--update`，继续正常启动；
    /// `Some(Err)` = 用法不对，**绝不能**回落到正常启动 —— 那会把一次
    /// "我想更新"变成"悄悄起了个服务"，而更新失败时机器正等着人处理。
    pub fn parse(args: &[String]) -> Option<Result<Self, String>> {
        if !args.iter().any(|a| a == "--update") {
            return None;
        }
        let mut req = Request::default();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--update" => match iter.next() {
                    Some(value) => match Verb::parse(value) {
                        Some(verb) => req.verb = Some(verb),
                        None => {
                            return Some(Err(format!(
                                "unknown --update verb {value:?} (check, apply)"
                            )))
                        }
                    },
                    None => return Some(Err("--update 后面要跟一个动词".to_owned())),
                },
                "--update-from" => {
                    if let Some(v) = iter.next() {
                        if !v.is_empty() {
                            req.from = Some(PathBuf::from(v));
                        }
                    }
                }
                "--update-dir" => {
                    if let Some(v) = iter.next() {
                        if !v.is_empty() {
                            req.dir = Some(PathBuf::from(v));
                        }
                    }
                }
                "--update-json" => req.json = true,
                "--update-force" => req.force = true,
                "--update-dry-run" => req.dry_run = true,
                "--update-overwrite-config" => req.overwrite_identity = true,
                _ => {}
            }
        }
        match req.verb {
            Some(_) => Some(Ok(req)),
            None => Some(Err("--update 后面要跟一个动词".to_owned())),
        }
    }

    /// 实际用的包目录：`--update-from` 优先，其次 exe 旁的 `update\`。
    fn source(&self, root: &Path) -> PathBuf {
        self.from
            .clone()
            .unwrap_or_else(|| default_source_dir(root))
    }
}

fn human_bytes(bytes: u64) -> String {
    ops::human_bytes(bytes)
}

/// 执行一次 `--update` 调用并打印结果，返回值即进程退出码。
///
/// 输出口径与 `--ops` / `--print-config` 一致：**stdout 只有结果**，解释走
/// stderr。`--update-json` 时 stdout 是一个 JSON 对象。
pub fn execute(request: &Request) -> i32 {
    let root = ops::run_root();
    let source = request.source(&root);
    let backups = request
        .dir
        .clone()
        .unwrap_or_else(|| ops::default_backup_dir(&root));
    let verb = match request.verb {
        Some(v) => v,
        None => return ops::EXIT_USAGE,
    };

    if !source.is_dir() {
        eprintln!(
            "update: 找不到更新包目录 {}（把新的一包解到那里，或用 --update-from 指定）",
            source.display()
        );
        return ops::EXIT_MISSING;
    }

    match verb {
        Verb::Check => {
            let report = match check(&root, &source, request.overwrite_identity) {
                Ok(r) => r,
                Err(err) => {
                    eprintln!("update: {err}");
                    return ops::EXIT_FAILED;
                }
            };
            if request.json {
                match serde_json::to_string_pretty(&report) {
                    Ok(text) => println!("{text}"),
                    Err(err) => {
                        eprintln!("update: 序列化失败: {err}");
                        return ops::EXIT_FAILED;
                    }
                }
            } else {
                println!("current:   {} ({})", report.current, report.current_commit);
                println!("source:    {}", report.source);
                match (&report.available, &report.probe_error) {
                    (Some(p), _) => println!(
                        "available: {} ({}){}",
                        p.version,
                        p.commit,
                        if report.newer { "  有新版" } else { "  不比当前新" }
                    ),
                    (None, Some(why)) => println!("available: 未知 —— {why}"),
                    (None, None) => println!("available: 未知"),
                }
                println!(
                    "plan:      {} 覆盖 / {} 新增 / {} 相同 / {} 保留",
                    report.replace(),
                    report.add(),
                    report.same(),
                    report.keep()
                );
                for entry in &report.plan {
                    if matches!(entry.action, Action::Add | Action::Replace | Action::Keep) {
                        println!(
                            "  {:<8} {:<40} {}",
                            entry.action.label(),
                            entry.path,
                            human_bytes(entry.size)
                        );
                    }
                }
            }
            if report.newer {
                ops::EXIT_OK
            } else {
                EXIT_UPTODATE
            }
        }
        Verb::Apply => {
            // 比版本这道门在 apply 里也要过一遍，不能只信 check 的结论：
            // check 和 apply 是两次独立调用，中间包可能被换掉。
            let candidate = source.join(crate::version::exe_name());
            let expect = if candidate.is_file() {
                match probe(&candidate) {
                    Ok(p) => Expect::Version(p.version),
                    Err(err) => {
                        eprintln!("update: 包里的 exe 问不出自己的版本：{err}");
                        if !request.force {
                            eprintln!("update: 加 --update-force 可以跳过这道门（风险自负）");
                            return ops::EXIT_FAILED;
                        }
                        // 放行不等于放弃验证：问不出"应该是哪一版"，但仍然
                        // 要求它跑得起来。一个跑不起来的 exe 被装进去，比
                        // 版本号不对严重得多。
                        eprintln!("update: 已按 --update-force 放行，但覆盖后仍会要求它起得来");
                        Expect::Runnable
                    }
                }
            } else {
                Expect::NotShipped
            };

            if let Expect::Version(ref version) = expect {
                if !is_newer(version, crate::version::VERSION) && !request.force {
                    eprintln!(
                        "update: 包里的版本 {version} 不比当前 {} 新；确实要重装同一版或降级，加 --update-force",
                        crate::version::VERSION
                    );
                    return EXIT_UPTODATE;
                }
            }

            if request.dry_run {
                return match check(&root, &source, request.overwrite_identity) {
                    Ok(report) => {
                        if request.json {
                            match serde_json::to_string_pretty(&report) {
                                Ok(text) => println!("{text}"),
                                Err(err) => {
                                    eprintln!("update: 序列化失败: {err}");
                                    return ops::EXIT_FAILED;
                                }
                            }
                        } else {
                            println!("dry-run: 不会写任何文件");
                            println!(
                                "plan:     {} 覆盖 / {} 新增 / {} 相同 / {} 保留",
                                report.replace(),
                                report.add(),
                                report.same(),
                                report.keep()
                            );
                            for entry in &report.plan {
                                if matches!(
                                    entry.action,
                                    Action::Add | Action::Replace | Action::Keep
                                ) {
                                    println!(
                                        "  {:<8} {:<40} {}",
                                        entry.action.label(),
                                        entry.path,
                                        human_bytes(entry.size)
                                    );
                                }
                            }
                        }
                        ops::EXIT_OK
                    }
                    Err(err) => {
                        eprintln!("update: {err}");
                        ops::EXIT_FAILED
                    }
                };
            }

            let report = match apply(
                &root,
                &source,
                &backups,
                &expect,
                request.overwrite_identity,
            ) {
                Ok(r) => r,
                Err(err) => {
                    eprintln!("update: {err}");
                    return ops::EXIT_FAILED;
                }
            };

            if request.json {
                match serde_json::to_string_pretty(&report) {
                    Ok(text) => println!("{text}"),
                    Err(err) => {
                        eprintln!("update: 序列化失败: {err}");
                        return ops::EXIT_FAILED;
                    }
                }
            } else {
                println!("snapshot:  {}", report.snapshot);
                println!("from:      {}", report.from_version);
                match &report.to_version {
                    Some(v) => println!("to:        {v}"),
                    None => println!("to:        （未改动版本；{}）", report.expectation),
                }
                println!(
                    "written:   {}（{} 相同，{} 保留）",
                    report.written.len(),
                    report.unchanged,
                    report.kept.len()
                );
                for (path, why) in &report.stashed {
                    println!("  stashed  {path}  —— {why}");
                }
                for path in &report.kept {
                    println!("  kept     {path}  —— 这台机器的身份，未覆盖");
                }
                if report.verified {
                    println!("verify:    通过");
                } else if report.error.is_some() {
                    println!("verify:    未通过（见下）");
                } else {
                    println!("verify:    跳过（{}）", report.expectation);
                }
                if report.rolled_back {
                    println!("rollback:  已退回 {}", report.snapshot);
                }
            }

            match &report.error {
                Some(err) => {
                    eprintln!("update: {err}");
                    ops::EXIT_FAILED
                }
                None => {
                    if !report.written.is_empty() {
                        // 更新生效需要重启：写文件的是这个进程，跑的是旧的
                        // 镜像。说清楚下一步，否则人会去点"刷新状态"看版本没变。
                        println!("hint:      重启服务后生效（任务计划 MCStreamSrv，或在控制台点安装/修复）");
                    }
                    ops::EXIT_OK
                }
            }
        }
    }
}

/// 从原始命令行进入，返回退出码。没有 `--update` 时返回 `None`。
pub fn try_run(args: &[String]) -> Option<i32> {
    match Request::parse(args)? {
        Ok(request) => Some(execute(&request)),
        Err(err) => {
            eprintln!("update: {err}");
            Some(ops::EXIT_USAGE)
        }
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    /// 临时目录。`ops` 的测试里有一份同样的：两边各自持有，省掉一个公共测试
    /// 模块（那会把测试辅助代码编进 release）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "cs-update-{}-{tag}-{n}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("建临时目录");
            TempDir(path)
        }

        fn write(&self, rel: &str, body: &[u8]) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("建父目录");
            }
            fs::write(&path, body).expect("写文件");
        }

        fn read(&self, rel: &str) -> Vec<u8> {
            fs::read(self.0.join(rel)).expect("读文件")
        }

        fn exists(&self, rel: &str) -> bool {
            self.0.join(rel).exists()
        }

        fn backups(&self) -> PathBuf {
            ops::default_backup_dir(&self.0)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn version_comparison_matches_human_intuition() {
        assert_eq!(compare_versions("0.4.0", "0.3.0"), Ordering::Greater);
        assert_eq!(compare_versions("0.3.0", "0.4.0"), Ordering::Less);
        assert_eq!(compare_versions("0.3.0", "0.3.0"), Ordering::Equal);
        // 段数不齐时短的一边补 0。
        assert_eq!(compare_versions("0.4", "0.3.9"), Ordering::Greater);
        assert_eq!(compare_versions("1.0.0", "0.99.99"), Ordering::Greater);
        // 0.10 比 0.9 新 —— 字符串比较会给出相反的答案，这是这个函数存在的理由。
        assert_eq!(compare_versions("0.10.0", "0.9.0"), Ordering::Greater);
    }

    #[test]
    fn same_version_is_not_an_update() {
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(is_newer("0.3.1", "0.3.0"));
        assert!(!is_newer("0.2.9", "0.3.0"));
    }

    #[test]
    fn plan_classifies_add_replace_and_same() {
        let root = TempDir::new("root");
        let pkg = TempDir::new("pkg");
        root.write("keep-same.txt", b"same");
        root.write("will-change.txt", b"old");
        pkg.write("keep-same.txt", b"same");
        pkg.write("will-change.txt", b"new");
        pkg.write("brand-new.dll", b"dll");

        let entries = plan(&pkg.0, &root.0, false).expect("比对应当成功");
        let by_path = |p: &str| {
            entries
                .iter()
                .find(|e| e.path == p)
                .unwrap_or_else(|| panic!("{p} 不在清单里"))
                .action
        };
        assert_eq!(by_path("keep-same.txt"), Action::Same);
        assert_eq!(by_path("will-change.txt"), Action::Replace);
        assert_eq!(by_path("brand-new.dll"), Action::Add);
    }

    /// 身份文件不该被包里的模板盖掉 —— 端口、口令、证书都和机器绑定。
    #[test]
    fn identity_files_are_kept_unless_asked() {
        let root = TempDir::new("root-id");
        let pkg = TempDir::new("pkg-id");
        root.write("stream-config.toml", b"fps = 15");
        pkg.write("stream-config.toml", b"fps = 60");
        pkg.write("cert.pem", b"template");

        let entries = plan(&pkg.0, &root.0, false).expect("比对应当成功");
        assert!(entries
            .iter()
            .all(|e| e.path != "stream-config.toml" || e.action == Action::Keep));

        let forced = plan(&pkg.0, &root.0, true).expect("比对应当成功");
        assert!(forced
            .iter()
            .all(|e| e.path != "stream-config.toml" || e.action == Action::Replace));
    }

    /// 子目录里的同名文件不是"这台机器的身份"，它属于前端资源。
    #[test]
    fn identity_detection_is_top_level_only() {
        assert!(is_identity("cert.pem"));
        assert!(is_identity("STREAM-CONFIG.TOML"));
        assert!(!is_identity("assets/cert.pem"));
    }

    #[test]
    fn state_and_logs_never_come_from_the_package() {
        let root = TempDir::new("root-skip");
        let pkg = TempDir::new("pkg-skip");
        pkg.write("state/backups/x/manifest.json", b"{}");
        pkg.write("logs/stream-server.log", b"noise");
        pkg.write("ok.txt", b"ok");
        let entries = plan(&pkg.0, &root.0, false).expect("比对应当成功");
        assert_eq!(entries.len(), 1, "只有 ok.txt 该进清单：{entries:?}");
        assert_eq!(entries[0].path, "ok.txt");
    }

    /// 一个空目录当更新包，是最容易犯的错（路径打错）。它必须报错，而不是
    /// "0 个文件要更新，成功"。
    #[test]
    fn empty_package_is_an_error() {
        let root = TempDir::new("root-empty");
        let pkg = TempDir::new("pkg-empty");
        assert!(plan(&pkg.0, &root.0, false).is_err());
    }

    /// 同一个目录既当包又当目标 = 拿自己覆盖自己。
    #[test]
    fn source_and_root_must_differ() {
        let dir = TempDir::new("same");
        dir.write("a.txt", b"x");
        assert!(ensure_distinct(&dir.0, &dir.0).is_err());
    }

    /// 包在运行目录里面可以（`--update-from .\update` 是人的自然做法）；
    /// 包是运行目录的上级不行 —— 那会让 `plan` 把安装目录自己也枚举进去。
    #[test]
    fn nested_package_is_allowed_but_container_is_not() {
        let root = TempDir::new("nest");
        let inner = root.0.join("update");
        fs::create_dir_all(&inner).expect("建 update 目录");
        assert!(ensure_distinct(&root.0, &inner).is_ok());
        assert!(ensure_distinct(&inner, &root.0).is_err());
    }

    /// 默认包位置必须在运行目录之外，否则每次更新都会把安装包备进快照。
    #[test]
    fn default_package_dir_is_outside_the_run_root() {
        let root = TempDir::new("default-dir");
        let source = default_source_dir(&root.0);
        let canon = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        assert!(
            !canon(&source).starts_with(canon(&root.0)),
            "默认包位置不该落在运行目录里：{}",
            source.display()
        );
    }

    /// 回滚落点：apply 之前那一份必须是一份完整的快照。
    #[test]
    fn apply_snapshots_before_touching_anything() {
        let root = TempDir::new("root-apply");
        let pkg = TempDir::new("pkg-apply");
        root.write("a.txt", b"old");
        pkg.write("a.txt", b"new");

        let report = apply(&root.0, &pkg.0, &root.backups(), &Expect::NotShipped, false)
            .expect("更新应当成功");
        assert_eq!(report.written, vec!["a.txt".to_owned()]);
        assert!(!report.snapshot.is_empty());
        assert_eq!(root.read("a.txt"), b"new");
        // 快照里存的是改之前的内容。
        let snap = root
            .backups()
            .join(&report.snapshot)
            .join("files")
            .join("a.txt");
        assert_eq!(fs::read(snap).expect("快照里应当有 a.txt"), b"old");
    }

    /// 包里没有 exe = 无法验版本。此时 `verified` 必须是 `false`，不能因为
    /// "没验"就报成"验过了"。
    #[test]
    fn apply_without_exe_does_not_claim_verification() {
        let root = TempDir::new("root-noverify");
        let pkg = TempDir::new("pkg-noverify");
        root.write("index.html", b"v1");
        pkg.write("index.html", b"v2");
        let report = apply(&root.0, &pkg.0, &root.backups(), &Expect::NotShipped, false)
            .expect("更新应当成功");
        assert!(!report.verified);
        assert!(report.error.is_none());
        assert_eq!(root.read("index.html"), b"v2");
    }

    /// 验版本失败必须自动回滚 —— 这是整个模块的存在理由。
    ///
    /// 包里没有 exe，所以"跑起来验版本"必然失败；用这个来模拟"新装的东西
    /// 起不来"。断言三件事：文件退回去了、报告说 rolled_back、错误里点了名。
    #[test]
    fn failed_verification_rolls_back_automatically() {
        let root = TempDir::new("root-rb");
        let pkg = TempDir::new("pkg-rb");
        root.write("index.html", b"v1");
        pkg.write("index.html", b"v2");

        // 期望版本给了值，但运行目录里没有 stream-server.exe 可问。
        let report = apply(
            &root.0,
            &pkg.0,
            &root.backups(),
            &Expect::Version("9.9.9".to_owned()),
            false,
        )
            .expect("回滚路径不该返回 Err");
        assert!(report.rolled_back, "验版本失败必须回滚");
        assert!(report.error.is_some(), "失败必须有话说");
        assert_eq!(root.read("index.html"), b"v1", "文件必须退回原样");
    }

    /// 实测撞到过的 bug，一字不差地钉在这里。
    ///
    /// 包里**有** exe、但问不出它的版本（`--update-force` 才放行到这一步）时，
    /// 曾经被当成"包里没有 exe"，于是跳过验证、把跑不起来的 exe 装进去、报成功。
    /// 验证在最需要它的那一刻静默失效，比没有验证更糟 —— 它给了一个假的"通过"。
    #[test]
    fn runnable_expectation_rejects_a_broken_binary() {
        let root = TempDir::new("root-runnable");
        let pkg = TempDir::new("pkg-runnable");
        root.write("stream-server.exe", b"old-binary-bytes");
        root.write("index.html", b"v1");
        pkg.write("stream-server.exe", b"definitely not a PE file");
        pkg.write("index.html", b"v2");

        let report = apply(&root.0, &pkg.0, &root.backups(), &Expect::Runnable, false)
            .expect("回滚路径不该返回 Err");
        assert!(report.rolled_back, "跑不起来的新版必须回滚");
        assert!(report.error.is_some(), "失败必须有话说");
        assert_eq!(
            root.read("stream-server.exe"),
            b"old-binary-bytes",
            "旧 exe 必须回来"
        );
        assert_eq!(root.read("index.html"), b"v1", "别的文件也要一起退回去");
    }

    #[test]
    fn stash_adds_a_suffix_instead_of_replacing_the_extension() {
        let dst = Path::new("D:\\app\\stream-server.exe");
        let stash = ops::stash_name(dst, 0);
        assert_eq!(
            stash.file_name().unwrap().to_string_lossy(),
            "stream-server.exe.rollback-old"
        );
    }

    #[test]
    fn request_parsing_needs_a_known_verb() {
        assert!(Request::parse(&["--ops".into(), "state".into()]).is_none());
        let ok = Request::parse(&[
            "--update".into(),
            "check".into(),
            "--update-from".into(),
            "D:\\pkg".into(),
        ])
        .expect("应当识别出 --update");
        let req = ok.expect("用法正确");
        assert_eq!(req.verb, Some(Verb::Check));
        assert_eq!(req.from, Some(PathBuf::from("D:\\pkg")));

        let bad = Request::parse(&["--update".into(), "appl".into()])
            .expect("应当识别出 --update")
            .expect_err("拼错的动词要报错");
        assert!(bad.contains("check, apply"), "报错要列出可用动词：{bad}");
    }

    /// `--update` 不带动词时不能回落到正常启动。
    #[test]
    fn bare_update_flag_is_usage_error_not_startup() {
        let parsed = Request::parse(&["--update".into()]).expect("应当识别出 --update");
        assert!(parsed.is_err());
    }
}
