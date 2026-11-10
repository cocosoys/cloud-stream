//! 动作底座：改之前先留底，改坏了能退回。
//!
//! 四动作（安装 / 卸载 / 更新 / 修改）在「失败可回滚」这一列上原本全是空的，
//! 而这一列恰恰是可视化界面敢把按钮交出去的前提 —— 一个点了就不可逆的按钮，
//! 没人敢放在首页。这里补的是那层公共底座：
//!
//! | 动作 | 它需要什么 | 由谁调用 |
//! |---|---|---|
//! | 安装 | 动手之前留一份，失败时退回 | `install.cmd` |
//! | 卸载 | 删目录之前把配置和状态搬到目录外 | `uninstall.cmd` |
//! | 更新 | 覆盖 exe/DLL/前端之前留一份 | 更新器（后续） |
//! | 修改 | 改 `stream-config.toml` 之前留一份 | 配置界面（后续） |
//!
//! # 为什么在 exe 里而不是一个 PowerShell 脚本
//!
//! 目标机上只有 exe 是确定存在的，而且未来那个可视化界面本身是 Rust 进程 ——
//! 让它去 `powershell -File ...` 只为拿一份清单，等于把唯一的可测层交给一串
//! 引号拼接。放进 exe 之后这一层能被单元测试覆盖（见文末），也能被 cmd 用
//! `--ops` 直接调。
//!
//! # 为什么绕过 `Config::parse`
//!
//! `--ops` 在 **解析配置之前**执行，直接读原始命令行。理由不是偷懒：最需要
//! 回滚的时刻，往往是配置文件刚好被改坏了 —— 那种情况下 `Config::parse` 会
//! 以退出码 2 结束进程，于是"用来救命的那条命令"和"把机器搞坏的那份配置"
//! 死在一起。恢复入口不能依赖它要恢复的东西。
//!
//! # 快照里有什么
//!
//! 运行根下除 `logs/` 和 `state/` 之外的全部文件（递归）。排除 `logs/` 是体积
//! 和意义都不对：日志记录的是过去发生的事，回滚它不会让机器回到过去的状态；
//! 排除 `state/` 是因为那里面装的就是快照自己。
//!
//! # 回滚怎么处理"正在运行的文件"
//!
//! Windows 不允许覆盖一个正在运行的 exe，但**允许重命名**它。所以恢复的写法
//! 是"先改名让路，再拷入"，而不是直接覆盖：
//!
//! ```text
//! stream-server.exe            ->  stream-server.exe.rollback-old
//! <备份>/files/stream-server.exe -> stream-server.exe
//! ```
//!
//! 这样一来，回滚可以在服务运行中执行，残留的 `.rollback-old` 是最坏情况下的
//! 第二道保险（真出问题时它还在原地）。被回滚"多出来"的文件（更新引入的新
//! DLL）同样改名而不是删除 —— 判错一次不该让人丢文件。

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 备份目录，相对运行根。跟着目录走，所以"整包搬走"仍然带着历史。
pub const BACKUP_SUBDIR: &str = "state/backups";

/// 备份目录的绝对路径。
///
/// 不走 `root.join(BACKUP_SUBDIR)`：那样在 Windows 上拼出来的显示形态是
/// `D:\stream\state/backups`（两种分隔符混用），能跑但每次打印到屏幕上都像
/// 出了什么错。路径最终是要给人看的。
pub fn default_backup_dir(root: &Path) -> PathBuf {
    root.join("state").join("backups")
}

/// 快照不进入的顶层目录名。
///
/// `logs`：日志是"过去发生过什么"的记录，回滚它不会让机器回到过去。
/// `state`：快照自己就住在这里，递归进去等于自己拷自己。
const SKIP_DIRS: &[&str] = &["logs", "state"];

/// 调用方显式点名要哪些路径时，只保留这一条排除。
///
/// `state/` 永远不能进快照（里面装的就是快照自己），但 `logs/` 必须放开：
/// 默认排除它是为全量快照省事，而卸载恰恰只要 `logs\mic-default.json` ——
/// 默认录音设备原值就在那儿，不备这一份，"卸载后再装回来"就丢了机器状态。
const EXPLICIT_SKIP: &[&str] = &["state"];

/// 恢复时让路用的后缀。
pub const STASH_SUFFIX: &str = ".rollback-old";

/// 回滚前自动留底用的标签。
///
/// 它有两个作用，第二个是**默认目标要排除它**：回滚一次之后，最新那份快照
/// 就变成了"回滚之前的状态"，于是"再回滚一次"会把刚退回去的东西又推回来 ——
/// 一个连点两次就等于没点的按钮。所以默认目标跳过自动快照，只认人主动拍的
/// 那几份；确实要退回自动那份（回滚错了），用 `--ops-to <id>` 指定即可。
pub const PRE_ROLLBACK_LABEL: &str = "pre-rollback";

/// 成功。
pub const EXIT_OK: i32 = 0;
/// 用法错误：动词不认识、缺参数。与 `Config::parse` 的退出码保持一致。
pub const EXIT_USAGE: i32 = 2;
/// 执行失败：拷贝失败、校验不一致、部分文件没能恢复。
pub const EXIT_FAILED: i32 = 3;
/// 找不到要回滚的目标（没有快照，或指定 id 不存在）。
pub const EXIT_MISSING: i32 = 4;

// ---------------------------------------------------------------------------
// 清单
// ---------------------------------------------------------------------------

/// 一个被快照的文件。`hash` 是 FNV-1a 64 的十六进制，用途是**变更检测**，
/// 不是完整性证明 —— 我们只需要回答"这个文件还是不是原来那个"，不需要抵抗
/// 有人恶意构造碰撞。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// 相对运行根的路径，用 `/` 分隔（跨平台可读，且 Windows 两种都吃）。
    pub path: String,
    pub size: u64,
    pub hash: String,
}

/// 一份快照的清单。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// `20261110T075512Z-preinstall`，字典序即时间序。
    pub id: String,
    pub created_unix: u64,
    /// `2026-11-10T07:55:12Z`。
    pub created_at: String,
    pub label: String,
    /// 拍这份快照时 exe 报的版本与提交。回滚之后"退回到哪一版"要能查。
    pub version: String,
    pub commit: String,
    /// 拍快照时运行根的绝对路径，仅用于展示与核对。
    pub root: String,
    pub files: Vec<FileEntry>,
}

impl Snapshot {
    /// 快照目录（`<备份目录>/<id>`）。
    pub fn dir(&self, backups: &Path) -> PathBuf {
        backups.join(&self.id)
    }

    /// 文件内容目录（`<备份目录>/<id>/files`）。
    pub fn files_dir(&self, backups: &Path) -> PathBuf {
        self.dir(backups).join("files")
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

/// 回滚结果。
#[derive(Debug, Default, Serialize)]
pub struct RollbackReport {
    /// 退回到的那一份。
    pub to: String,
    /// 回滚之前自动留的那一份（回滚本身也可回滚）。
    pub pre: String,
    /// 被替换掉的文件。
    pub restored: Vec<String>,
    /// 内容本来就一样的文件数量。
    pub unchanged: usize,
    /// 目标快照里没有、被让路成 `.rollback-old` 的文件。
    pub stashed: Vec<String>,
    /// (路径, 原因)。非空即 `EXIT_FAILED`。
    pub failed: Vec<(String, String)>,
}

/// 校验结果。
#[derive(Debug, Default, Serialize)]
pub struct VerifyReport {
    pub against: String,
    pub ok: usize,
    /// 存在但内容变了。
    pub changed: Vec<String>,
    /// 清单里有、盘上没有。
    pub missing: Vec<String>,
    /// 盘上有、清单里没有。
    pub extra: Vec<String>,
}

impl VerifyReport {
    pub fn is_clean(&self) -> bool {
        self.changed.is_empty() && self.missing.is_empty()
    }
}

// ---------------------------------------------------------------------------
// 基础件
// ---------------------------------------------------------------------------

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `20261110T075512Z`：定宽、无分隔符、字典序 = 时间序。
fn compact_utc(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = crate::version::civil_from_unix(secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// 标签只留 `[A-Za-z0-9._-]`。标签会变成目录名，而它的来源可能是用户在界面上
/// 随手敲的一句话 —— 一个 `\` 或 `:` 就能让它跑到别的盘上去。
fn sanitize_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('_').to_owned();
    // 全是点的标签（`..`、`...`）单独处理：它们不是文件名而是相对路径，
    // 拼在 `state/backups/` 后面就是"上一级目录"。
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '.') {
        return "snapshot".to_owned();
    }
    trimmed
}

/// 相对路径转成清单里的形态：`/` 分隔。
fn rel_to_string(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn string_to_rel(s: &str) -> PathBuf {
    s.split('/').fold(PathBuf::new(), |acc, part| acc.join(part))
}

/// FNV-1a 64。选它而不是 sha2 的理由：没有依赖、源码十二行、对"文件变了没变"
/// 这个用途足够；哈希值只在本项目内部比较，不进任何协议。
fn hash_file(path: &Path) -> Result<(u64, String), String> {
    let mut file =
        fs::File::open(path).map_err(|e| format!("{}: 打不开: {e}", path.display()))?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut size: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("{}: 读失败: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        size += n as u64;
        for byte in &buf[..n] {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    }
    Ok((size, format!("{hash:016x}")))
}

/// 收集运行根下需要纳入快照的文件（相对路径，已排序）。
///
/// `skip` 是顶层要跳过的目录名。默认全量快照传 [`SKIP_DIRS`]；调用方显式
/// 点名要哪些路径时传 `["state"]` —— 那一条永远不能放开，否则快照会把备份
/// 目录自己拷进备份目录。
fn collect(root: &Path) -> Result<Vec<PathBuf>, String> {
    collect_skipping(root, SKIP_DIRS)
}

fn collect_skipping(root: &Path, skip: &[&str]) -> Result<Vec<PathBuf>, String> {
    fn walk(
        dir: &Path,
        rel: &Path,
        out: &mut Vec<PathBuf>,
        depth: usize,
        skip: &[&str],
    ) -> Result<(), String> {
        // 深度上限不是防御性编程洁癖：目录里可能出现符号链接环，
        // 而 `read_dir` 会跟着它一直走下去。
        if depth > 16 {
            return Ok(());
        }
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            // 一个读不动的子目录不该让整次快照失败：跳过并继续，
            // 因为"少备一个目录"远好于"备份功能整个不可用"。
            Err(_) => return Ok(()),
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let child_rel = rel.join(&name);
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                if depth == 0 && skip.iter().any(|d| name_str.eq_ignore_ascii_case(d)) {
                    continue;
                }
                walk(&path, &child_rel, out, depth + 1, skip)?;
            } else if meta.is_file() {
                // 上一轮回滚让路留下的残骸不该进入新的快照，否则每回滚一次，
                // 备份就胖一圈，而且永远删不掉。
                if name_str.ends_with(STASH_SUFFIX) {
                    continue;
                }
                out.push(child_rel);
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    walk(root, Path::new(""), &mut files, 0, skip)?;
    files.sort();
    Ok(files)
}

/// 写清单。先写 `.tmp` 再改名：中途断电时宁可没有这份清单，也不要留一份
/// 一半的 JSON —— 读清单的地方会把它当成"快照坏了"，而坏快照比没快照更误导。
fn write_manifest(dir: &Path, snapshot: &Snapshot) -> Result<(), String> {
    let text = serde_json::to_string_pretty(snapshot)
        .map_err(|e| format!("清单序列化失败: {e}"))?;
    let tmp = dir.join("manifest.json.tmp");
    let final_path = dir.join("manifest.json");
    fs::write(&tmp, text).map_err(|e| format!("{}: 写失败: {e}", tmp.display()))?;
    fs::rename(&tmp, &final_path)
        .map_err(|e| format!("{}: 改名失败: {e}", final_path.display()))
}

fn load_manifest(dir: &Path) -> Result<Snapshot, String> {
    let path = dir.join("manifest.json");
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: 读失败: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: 解析失败: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// 动作
// ---------------------------------------------------------------------------

/// 拍一份快照。
///
/// `backups` 是备份目录（默认 `root/state/backups`，也可以指到目录之外 ——
/// 卸载就要求这样，因为整个目录马上要被删掉）。
pub fn snapshot(root: &Path, backups: &Path, label: &str) -> Result<Snapshot, String> {
    snapshot_paths(root, backups, label, None)
}

/// 只备份指定路径（相对运行根，目录会被递归）。
///
/// 卸载要的那一份和更新要的那一份不是一回事：卸载只需要留下"这台机器的身份"
/// —— 配置、证书、录音设备原值 —— 把几十兆的前端 bundle 和 DLL 也搬进
/// `ProgramData` 只是在浪费磁盘；更新才需要完整一份用于回退。`None` = 全量。
pub fn snapshot_paths(
    root: &Path,
    backups: &Path,
    label: &str,
    include: Option<&[String]>,
) -> Result<Snapshot, String> {
    // 全量快照沿用默认排除（logs 与 state 都不进）；显式点名时只保 state
    // 这一条 —— 见 [`EXPLICIT_SKIP`]。
    let explicit = include.map(|list| !list.is_empty()).unwrap_or(false);
    let mut files = if explicit {
        collect_skipping(root, EXPLICIT_SKIP)?
    } else {
        collect(root)?
    };
    if let Some(only) = include {
        if explicit {
            let wanted: Vec<String> = only
                .iter()
                // 命令行上写反斜杠更自然（这是 Windows 工具），但清单里统一是
                // 正斜杠，所以两边都归一化再比。
                .map(|s| s.trim().trim_start_matches("./").replace('\\', "/"))
                .filter(|s| !s.is_empty())
                .collect();
            files.retain(|rel| {
                let rel = rel_to_string(rel);
                wanted
                    .iter()
                    .any(|want| rel == *want || rel.starts_with(&format!("{want}/")))
            });
        }
    }
    let now = unix_now();
    let id = format!("{}-{}", compact_utc(now), sanitize_label(label));
    let dir = backups.join(&id);
    // 同一秒内同标签重名：加后缀而不是覆盖。覆盖会让"我刚才那份呢"变得无从回答。
    let dir = if dir.exists() {
        let mut n = 2;
        loop {
            let candidate = backups.join(format!("{id}-{n}"));
            if !candidate.exists() {
                break candidate;
            }
            n += 1;
        }
    } else {
        dir
    };
    let id = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or(id);
    // 空运行根（卸载后重新解包的目录）也要能拍一份"什么都没有"的快照，
    // 否则清单没地方写。
    fs::create_dir_all(&dir).map_err(|e| format!("{}: 建目录失败: {e}", dir.display()))?;
    let files_root = dir.join("files");

    let mut entries = Vec::with_capacity(files.len());
    for rel in files {
        let src = root.join(&rel);
        let (size, hash) = hash_file(&src)?;
        let dst = files_root.join(&rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("{}: 建目录失败: {e}", parent.display()))?;
        }
        fs::copy(&src, &dst).map_err(|e| {
            format!(
                "{}: 备份失败: {e}（文件可能正被独占）",
                src.display()
            )
        })?;
        entries.push(FileEntry {
            path: rel_to_string(&rel),
            size,
            hash,
        });
    }

    let stamp = compact_utc(now);
    let snapshot = Snapshot {
        id,
        created_unix: now,
        // `20261110T075512Z` -> `2026-11-10T07:55:12Z`：目录名要定宽可排序，
        // 人读的时间要带分隔符，两者只差一次切片，不值得存两份。
        created_at: format!(
            "{}-{}-{}T{}:{}:{}Z",
            &stamp[0..4],
            &stamp[4..6],
            &stamp[6..8],
            &stamp[9..11],
            &stamp[11..13],
            &stamp[13..15],
        ),
        label: label.to_owned(),
        version: crate::version::VERSION.to_owned(),
        commit: crate::version::COMMIT.to_owned(),
        root: root.display().to_string(),
        files: entries,
    };
    write_manifest(&dir, &snapshot)?;
    Ok(snapshot)
}

/// 列出全部快照，新的在前。读不动的目录跳过而不是报错：一个坏快照不该让
/// "还有哪些能退回去"这个问题无法回答。
pub fn list(backups: &Path) -> Vec<Snapshot> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(backups) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            if let Ok(snapshot) = load_manifest(&entry.path()) {
                out.push(snapshot);
            }
        }
    }
    out.sort_by(|a, b| b.id.cmp(&a.id));
    out
}

/// 找一份快照。
///
/// `to` 为空取**最近一份人主动拍的**快照（跳过 `pre-rollback` 自动留底，见
/// [`PRE_ROLLBACK_LABEL`]）；给了 id 就精确匹配，匹配不到再按前缀匹配（人从
/// 界面上复制 id 时常常少复制尾巴）。
fn resolve(backups: &Path, to: Option<&str>) -> Result<Snapshot, String> {
    let all = list(backups);
    if all.is_empty() {
        return Err(format!("{} 里没有任何快照", backups.display()));
    }
    match to {
        None => all
            .iter()
            .find(|s| s.label != PRE_ROLLBACK_LABEL)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "{} 里只有回滚前自动留底，没有可退回的目标（用 --ops-to <id> 指定具体一份）",
                    backups.display()
                )
            }),
        Some(wanted) => {
            if let Some(found) = all.iter().find(|s| s.id == wanted) {
                return Ok(found.clone());
            }
            let prefixed: Vec<&Snapshot> = all.iter().filter(|s| s.id.starts_with(wanted)).collect();
            match prefixed.len() {
                1 => Ok(prefixed[0].clone()),
                0 => Err(format!("没有 id 为 {wanted:?} 的快照")),
                _ => Err(format!(
                    "{wanted:?} 匹配到 {} 份快照，请写完整 id",
                    prefixed.len()
                )),
            }
        }
    }
}

/// 回滚到某一份快照。
///
/// 顺序刻意是"先给当前状态留底，再动文件"：回滚本身也会出错（判断错目标、
/// 拷到一半断电），而那时唯一能救命的就是刚刚被覆盖掉的那一份。
pub fn rollback(root: &Path, backups: &Path, to: Option<&str>) -> Result<RollbackReport, String> {
    let target = resolve(backups, to)?;
    let pre = snapshot(root, backups, PRE_ROLLBACK_LABEL)?;
    let files_root = target.files_dir(backups);

    let mut report = RollbackReport {
        to: target.id.clone(),
        pre: pre.id.clone(),
        ..Default::default()
    };

    let wanted: BTreeMap<&str, &FileEntry> =
        target.files.iter().map(|f| (f.path.as_str(), f)).collect();

    for entry in &target.files {
        let src = files_root.join(string_to_rel(&entry.path));
        let dst = root.join(string_to_rel(&entry.path));
        if !src.exists() {
            report
                .failed
                .push((entry.path.clone(), "备份里缺这个文件".to_owned()));
            continue;
        }
        // 已经一样就跳过：少一次"改名再拷回"，也少一次被占用挡住的机会。
        if let Ok((size, hash)) = hash_file(&dst) {
            if size == entry.size && hash == entry.hash {
                report.unchanged += 1;
                continue;
            }
        }
        if let Some(parent) = dst.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                report
                    .failed
                    .push((entry.path.clone(), format!("建目录失败: {e}")));
                continue;
            }
        }
        if dst.exists() {
            if let Err(e) = stash(&dst) {
                report.failed.push((entry.path.clone(), e));
                continue;
            }
        }
        match fs::copy(&src, &dst) {
            Ok(_) => report.restored.push(entry.path.clone()),
            Err(e) => report
                .failed
                .push((entry.path.clone(), format!("拷回失败: {e}"))),
        }
    }

    // 盘上有、目标快照里没有的文件：更新引入的新文件。让路而不是删除。
    if let Ok(current) = collect(root) {
        for rel in current {
            let key = rel_to_string(&rel);
            if wanted.contains_key(key.as_str()) {
                continue;
            }
            let path = root.join(&rel);
            match stash(&path) {
                Ok(()) => report.stashed.push(key),
                Err(e) => report.failed.push((key, e)),
            }
        }
    }

    Ok(report)
}

/// 把文件改名让路。已存在的残骸先删掉，否则 Windows 上 `rename` 会因为目标
/// 存在而失败（POSIX 会静默覆盖，两边语义不同，这里统一成"先删再改名"）。
fn stash(path: &Path) -> Result<(), String> {
    let mut stash_name = path.as_os_str().to_owned();
    stash_name.push(STASH_SUFFIX);
    let stash_path = PathBuf::from(stash_name);
    if stash_path.exists() {
        fs::remove_file(&stash_path)
            .map_err(|e| format!("{}: 旧残骸删不掉: {e}", stash_path.display()))?;
    }
    fs::rename(path, &stash_path).map_err(|e| {
        format!(
            "{}: 改名失败: {e}（文件正被占用时先停服务）",
            path.display()
        )
    })
}

/// 拿当前状态和某份快照对比。不写任何文件。
pub fn verify(root: &Path, backups: &Path, against: Option<&str>) -> Result<VerifyReport, String> {
    let target = resolve(backups, against)?;
    let mut report = VerifyReport {
        against: target.id.clone(),
        ..Default::default()
    };
    let wanted: BTreeMap<&str, &FileEntry> =
        target.files.iter().map(|f| (f.path.as_str(), f)).collect();

    for entry in &target.files {
        let path = root.join(string_to_rel(&entry.path));
        match hash_file(&path) {
            Ok((size, hash)) => {
                if size == entry.size && hash == entry.hash {
                    report.ok += 1;
                } else {
                    report.changed.push(entry.path.clone());
                }
            }
            Err(_) => report.missing.push(entry.path.clone()),
        }
    }
    if let Ok(current) = collect(root) {
        for rel in current {
            let key = rel_to_string(&rel);
            if !wanted.contains_key(key.as_str()) {
                report.extra.push(key);
            }
        }
    }
    Ok(report)
}

/// 只保留最近若干份，更旧的删掉。返回删掉的 id。
///
/// 保留策略不是单纯的"最近 N 份"：把自动留底和主动快照混在一起排序，会得出
/// 一个荒唐的结果 —— 连续回滚几次之后，最新的几份全是 `pre-rollback`，于是
/// `gc --ops-keep 1` 把唯一有用的那份（人主动拍的"改动之前"）删掉，留下一个
/// 谁也不会想要的自动留底。所以这里分开算：
///
/// * 人主动拍的：保留最新 `keep` 份；
/// * 自动留底的：只保留最新 1 份（回滚错了的那一次还能退回来）。
///
/// `keep` 至少为 1。把历史删空之后，"刚刚那次改动之前是什么样"就永久无解了。
pub fn gc(backups: &Path, keep: usize) -> Result<Vec<String>, String> {
    let keep = keep.max(1);
    let mut kept_deliberate = 0usize;
    let mut kept_auto = false;
    let mut removed = Vec::new();
    // `list` 是新的在前，所以"第一次遇到"就是最新的一份。
    for snapshot in list(backups) {
        let keep_this = if snapshot.label == PRE_ROLLBACK_LABEL {
            if kept_auto {
                false
            } else {
                kept_auto = true;
                true
            }
        } else if kept_deliberate < keep {
            kept_deliberate += 1;
            true
        } else {
            false
        };
        if keep_this {
            continue;
        }
        let dir = snapshot.dir(backups);
        fs::remove_dir_all(&dir).map_err(|e| format!("{}: 删除失败: {e}", dir.display()))?;
        removed.push(snapshot.id);
    }
    Ok(removed)
}

// ---------------------------------------------------------------------------
// 命令行
// ---------------------------------------------------------------------------

/// `--ops` 的动词。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// 当前状态汇总（只读）。
    State,
    /// 拍一份快照。
    Snapshot,
    /// 回滚。
    Rollback,
    /// 校验。
    Verify,
    /// 列出快照。
    List,
    /// 清理旧快照。
    Gc,
}

impl Verb {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "state" => Some(Self::State),
            "snapshot" => Some(Self::Snapshot),
            "rollback" => Some(Self::Rollback),
            "verify" => Some(Self::Verify),
            "list" => Some(Self::List),
            "gc" => Some(Self::Gc),
            _ => None,
        }
    }
}

/// 解析后的调用参数。
#[derive(Debug, Default)]
pub struct Request {
    pub verb: Option<Verb>,
    pub json: bool,
    pub label: String,
    pub dir: Option<PathBuf>,
    pub to: Option<String>,
    pub keep: Option<usize>,
    /// 只备份这些相对路径（`--ops-paths`，分号或逗号分隔）。空 = 全量。
    pub paths: Option<Vec<String>>,
    /// 顺带记下 `--config`：`state` 要报"这台机器实际读的是哪份配置"。
    /// 不解析配置内容，只记路径 —— 恢复入口不能依赖它要恢复的东西。
    pub config: Option<PathBuf>,
}

impl Request {
    /// 从原始命令行（不含程序名）解析。
    ///
    /// 返回 `None` = 命令行里没有 `--ops`，调用方继续走正常启动。
    /// 返回 `Some(Err)` = 有 `--ops` 但用法不对；**绝不能**在这种情况下回落到
    /// 正常启动 —— 那会把"我想回滚"变成一个"悄悄起了个服务"。
    pub fn parse(args: &[String]) -> Option<Result<Self, String>> {
        if !args.iter().any(|a| a == "--ops") {
            return None;
        }
        let mut req = Request::default();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--ops" => match iter.next() {
                    Some(v) => match Verb::parse(v) {
                        Some(verb) => req.verb = Some(verb),
                        None => {
                            return Some(Err(format!(
                                "unknown --ops verb {v:?} (state, snapshot, rollback, verify, list, gc)"
                            )))
                        }
                    },
                    None => return Some(Err("--ops 后面要跟一个动词".to_owned())),
                },
                "--ops-json" => req.json = true,
                "--ops-label" => {
                    if let Some(v) = iter.next() {
                        req.label = v.clone();
                    }
                }
                "--ops-dir" => {
                    if let Some(v) = iter.next() {
                        req.dir = Some(PathBuf::from(v));
                    }
                }
                "--ops-to" => {
                    if let Some(v) = iter.next() {
                        req.to = Some(v.clone());
                    }
                }
                "--ops-keep" => {
                    if let Some(v) = iter.next() {
                        match v.parse::<usize>() {
                            Ok(n) => req.keep = Some(n),
                            Err(_) => {
                                return Some(Err(format!("--ops-keep 需要一个数字，收到 {v:?}")))
                            }
                        }
                    }
                }
                "--ops-paths" => {
                    if let Some(v) = iter.next() {
                        let list: Vec<String> = v
                            .split(|c| c == ';' || c == ',')
                            .map(|s| s.trim().to_owned())
                            .filter(|s| !s.is_empty())
                            .collect();
                        req.paths = Some(list);
                    }
                }
                "--config" => {
                    if let Some(v) = iter.next() {
                        if !v.is_empty() {
                            req.config = Some(PathBuf::from(v));
                        }
                    }
                }
                _ => {}
            }
        }
        match req.verb {
            Some(_) => Some(Ok(req)),
            None => Some(Err("--ops 后面要跟一个动词".to_owned())),
        }
    }
}

/// 运行根：exe 所在目录。
///
/// 不用当前工作目录：看门狗与计划任务拉起本进程时，工作目录是 `System32`
/// （任务没写工作目录时的默认值），照那个路径拍快照会把整个系统目录当成
/// 运行根。exe 的位置是唯一可信的锚点。
pub fn run_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn summarize(snapshot: &Snapshot) -> serde_json::Value {
    serde_json::json!({
        "id": snapshot.id,
        "label": snapshot.label,
        "created_at": snapshot.created_at,
        "created_unix": snapshot.created_unix,
        "version": snapshot.version,
        "commit": snapshot.commit,
        "files": snapshot.files.len(),
        "bytes": snapshot.total_bytes(),
        "root": snapshot.root,
    })
}

/// 执行一次 `--ops` 调用并打印结果，返回值即进程退出码。
///
/// 输出口径与 `--print-config` 一致：**stdout 只有结果**，解释性的话走 stderr。
/// 调用方（cmd 脚本、未来的界面）因此可以放心把 stdout 当 JSON 解析。
pub fn execute(request: &Request) -> i32 {
    let root = run_root();
    let backups = request
        .dir
        .clone()
        .unwrap_or_else(|| default_backup_dir(&root));
    let json = request.json;

    let verb = match request.verb {
        Some(v) => v,
        None => return EXIT_USAGE,
    };

    match verb {
        Verb::State => {
            let snapshots = list(&backups);
            // `latest` 报的是"现在回滚会退到哪一份"，也就是默认目标 —— 不能
            // 报成最新的那一份，否则屏幕上写着 A、`rollback` 实际去 B。
            let target = snapshots
                .iter()
                .find(|s| s.label != PRE_ROLLBACK_LABEL)
                .map(summarize);
            // `--config` 优先，其次默认位置。默认位置还要再分两种：`Config::parse`
            // 自动发现的是 exe 旁的 `config.toml`，但整包分发的那份叫
            // `stream-config.toml` 且由 `start-stream.cmd --config` 显式指定。
            // 只报 `config.toml (absent)` 会让人以为自己没配置，而实际上文件
            // 就在旁边、服务也在用它 —— 所以这里退一步再找一个，纯只读。
            //
            // 刻意**不改** `configfile::default_path`：那是启动时的自动发现逻辑，
            // 动它等于改变已有部署的加载行为，不属于"报告得更准"。
            let config = request.config.clone().or_else(|| {
                let primary = crate::configfile::default_path()?;
                if primary.exists() {
                    return Some(primary);
                }
                let sibling = primary.with_file_name("stream-config.toml");
                if sibling.exists() {
                    Some(sibling)
                } else {
                    Some(primary)
                }
            });
            let mut doc = serde_json::json!({
                "root": root.display().to_string(),
                "backups": backups.display().to_string(),
                "version": crate::version::VERSION,
                "commit": crate::version::COMMIT,
                "dirty": crate::version::DIRTY,
                "built_at": crate::version::built_at_rfc3339(),
                "snapshot_count": snapshots.len(),
                "rollback_target": target,
                "config": config.as_ref().map(|p| p.display().to_string()),
                "config_exists": config.as_ref().map(|p| p.exists()).unwrap_or(false),
            });
            if let (Some(path), Some(object)) = (config.as_ref(), doc.as_object_mut()) {
                object.insert(
                    "config_hash".to_owned(),
                    serde_json::Value::String(
                        hash_file(path)
                            .map(|(_, h)| h)
                            .unwrap_or_else(|_| "unreadable".to_owned()),
                    ),
                );
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_owned())
                );
            } else {
                println!("root:      {}", root.display());
                println!(
                    "version:   {} ({}{})",
                    crate::version::VERSION,
                    crate::version::COMMIT,
                    if crate::version::DIRTY == "true" {
                        ", dirty"
                    } else {
                        ""
                    }
                );
                println!("backups:   {}", backups.display());
                match snapshots
                    .iter()
                    .find(|s| s.label != PRE_ROLLBACK_LABEL)
                {
                    Some(s) => println!(
                        "rollback:  {} ({} files, {})",
                        s.id,
                        s.files.len(),
                        human_bytes(s.total_bytes())
                    ),
                    None => println!("rollback:  (无可退回的快照)"),
                }
                println!("snapshots: {} (含自动留底)", snapshots.len());
                match config {
                    Some(path) => println!(
                        "config:    {} ({})",
                        path.display(),
                        if path.exists() { "present" } else { "absent" }
                    ),
                    None => println!("config:    (unknown location)"),
                }
            }
            EXIT_OK
        }

        Verb::Snapshot => match snapshot_paths(
            &root,
            &backups,
            &request.label,
            request.paths.as_deref(),
        ) {
            Ok(s) => {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&summarize(&s))
                            .unwrap_or_else(|_| "{}".to_owned())
                    );
                } else {
                    println!("snapshot: {}", s.id);
                    println!("  files:  {} ({})", s.files.len(), human_bytes(s.total_bytes()));
                    println!("  dir:    {}", s.dir(&backups).display());
                }
                EXIT_OK
            }
            Err(err) => {
                eprintln!("ops snapshot failed: {err}");
                EXIT_FAILED
            }
        },

        Verb::List => {
            let snapshots = list(&backups);
            if json {
                let doc: Vec<_> = snapshots.iter().map(summarize).collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "[]".to_owned())
                );
            } else if snapshots.is_empty() {
                println!("(no snapshots in {})", backups.display());
            } else {
                for s in &snapshots {
                    println!(
                        "{:<40} {:>4} files {:>10}  {}{}",
                        s.id,
                        s.files.len(),
                        human_bytes(s.total_bytes()),
                        s.version,
                        if s.label.is_empty() {
                            String::new()
                        } else {
                            format!("  [{}]", s.label)
                        }
                    );
                }
            }
            EXIT_OK
        }

        Verb::Rollback => match rollback(&root, &backups, request.to.as_deref()) {
            Ok(report) => {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&report)
                            .unwrap_or_else(|_| "{}".to_owned())
                    );
                } else {
                    println!("rollback: -> {}", report.to);
                    println!("  pre:      {} (回滚本身也能退回去)", report.pre);
                    println!("  restored: {}", report.restored.len());
                    println!("  unchanged:{}", report.unchanged);
                    if !report.stashed.is_empty() {
                        println!("  stashed:  {} (改名成 {STASH_SUFFIX}，没有删除)", report.stashed.len());
                    }
                    for path in &report.restored {
                        println!("    ~ {path}");
                    }
                    for (path, why) in &report.failed {
                        eprintln!("    ! {path}: {why}");
                    }
                }
                if report.failed.is_empty() {
                    EXIT_OK
                } else {
                    eprintln!(
                        "ops rollback: {} 个文件没能恢复（占用中的文件先停服务再试）",
                        report.failed.len()
                    );
                    EXIT_FAILED
                }
            }
            Err(err) => {
                eprintln!("ops rollback failed: {err}");
                EXIT_MISSING
            }
        },

        Verb::Verify => match verify(&root, &backups, request.to.as_deref()) {
            Ok(report) => {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&report)
                            .unwrap_or_else(|_| "{}".to_owned())
                    );
                } else {
                    println!("verify against: {}", report.against);
                    println!("  ok:      {}", report.ok);
                    for path in &report.changed {
                        println!("  changed: {path}");
                    }
                    for path in &report.missing {
                        println!("  missing: {path}");
                    }
                    for path in &report.extra {
                        println!("  extra:   {path}");
                    }
                }
                if report.is_clean() {
                    EXIT_OK
                } else {
                    eprintln!("ops verify: 当前状态与 {} 不一致", report.against);
                    EXIT_FAILED
                }
            }
            Err(err) => {
                eprintln!("ops verify failed: {err}");
                EXIT_MISSING
            }
        },

        Verb::Gc => {
            let keep = request.keep.unwrap_or(3);
            match gc(&backups, keep) {
                Ok(removed) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({ "kept": keep, "removed": removed })
                        );
                    } else if removed.is_empty() {
                        println!("gc: nothing to remove (keep {keep})");
                    } else {
                        println!("gc: removed {} (keep {keep})", removed.len());
                        for id in &removed {
                            println!("  - {id}");
                        }
                    }
                    EXIT_OK
                }
                Err(err) => {
                    eprintln!("ops gc failed: {err}");
                    EXIT_FAILED
                }
            }
        }
    }
}

/// 入口：有 `--ops` 就执行并返回退出码，没有就返回 `None` 让调用方继续启动。
pub fn try_run(args: &[String]) -> Option<i32> {
    match Request::parse(args)? {
        Ok(request) => Some(execute(&request)),
        Err(err) => {
            eprintln!("ops: {err}");
            Some(EXIT_USAGE)
        }
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 唯一的临时目录。不引 `tempfile`：只需要一个空目录，而它会把依赖树再
    /// 拉长一截。名字带进程号与计数器，避免并行测试互相踩。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "cs-ops-{}-{tag}-{n}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("建临时目录");
            TempDir(path)
        }

        fn write(&self, rel: &str, body: &str) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("建父目录");
            }
            fs::write(&path, body).expect("写文件");
        }

        fn read(&self, rel: &str) -> String {
            fs::read_to_string(self.0.join(rel)).expect("读文件")
        }

        fn backups(&self) -> PathBuf {
            default_backup_dir(&self.0)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn snapshot_copies_files_and_skips_logs_and_state() {
        let dir = TempDir::new("snap");
        dir.write("stream-server.exe", "binary");
        dir.write("dist/index.html", "<html>");
        dir.write("logs/stream-server.log", "noise");
        dir.write("state/backups/old/manifest.json", "{}");

        let snap = snapshot(&dir.0, &dir.backups(), "test").expect("快照应当成功");
        let paths: Vec<&str> = snap.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"stream-server.exe"));
        assert!(paths.contains(&"dist/index.html"));
        assert!(
            !paths.iter().any(|p| p.starts_with("logs/")),
            "日志不该进快照：{paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.starts_with("state/")),
            "state 不该进快照（会自己拷自己）：{paths:?}"
        );
        // 铺平在备份目录下，且内容一致。
        let copied = snap.files_dir(&dir.backups()).join("dist/index.html");
        assert_eq!(fs::read_to_string(copied).unwrap(), "<html>");
    }

    #[test]
    fn label_is_sanitized_so_it_cannot_escape_the_backup_dir() {
        assert_eq!(sanitize_label("pre-install"), "pre-install");
        assert_eq!(sanitize_label("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_label("配置 修改"), "snapshot");
        assert_eq!(sanitize_label(""), "snapshot");
        // 全点的标签不是文件名，是相对路径。
        assert_eq!(sanitize_label(".."), "snapshot");
        assert_eq!(sanitize_label("..."), "snapshot");
        let dir = TempDir::new("label");
        dir.write("a.txt", "x");
        let snap = snapshot(&dir.0, &dir.backups(), "../../evil").expect("快照应当成功");
        // 标签会变成目录名，路径分隔符必须被吃掉：否则 `../..` 能把备份写到
        // 备份目录之外，一次"快照"变成一次任意位置写入。
        assert!(
            !snap.id.contains('/') && !snap.id.contains('\\'),
            "标签里的路径分隔符必须被清洗掉，实际 id = {}",
            snap.id
        );
        assert_eq!(
            snap.dir(&dir.backups()).parent(),
            Some(dir.backups().as_path()),
            "快照目录必须直接落在备份目录下"
        );
    }

    #[test]
    fn rollback_restores_changed_and_deleted_files_and_stashes_extras() {
        let dir = TempDir::new("rollback");
        dir.write("app.exe", "v1");
        dir.write("dist/index.html", "v1-html");
        snapshot(&dir.0, &dir.backups(), "before").expect("快照应当成功");

        // 模拟一次失败的更新：改了内容、删了文件、多了一个新文件。
        dir.write("app.exe", "v2");
        fs::remove_file(dir.0.join("dist/index.html")).unwrap();
        dir.write("new-after-update.dll", "extra");

        let report = rollback(&dir.0, &dir.backups(), None).expect("回滚应当成功");
        assert_eq!(dir.read("app.exe"), "v1", "被改的文件要退回原内容");
        assert_eq!(dir.read("dist/index.html"), "v1-html", "被删的文件要回来");
        assert!(
            report.stashed.contains(&"new-after-update.dll".to_owned()),
            "多出来的文件要让路：{:?}",
            report.stashed
        );
        assert!(
            !dir.0.join("new-after-update.dll").exists(),
            "让路之后原名不该还在"
        );
        assert!(
            dir.0.join(format!("new-after-update.dll{STASH_SUFFIX}")).exists(),
            "让路的文件必须以 .rollback-old 留在原地，不能删"
        );
        assert!(report.failed.is_empty(), "不该有失败项：{:?}", report.failed);
    }

    #[test]
    fn rollback_itself_is_reversible() {
        let dir = TempDir::new("reversible");
        dir.write("app.exe", "v1");
        snapshot(&dir.0, &dir.backups(), "before").expect("快照应当成功");
        dir.write("app.exe", "v2");

        let report = rollback(&dir.0, &dir.backups(), None).expect("回滚应当成功");
        // 回滚之前自动留的那一份，内容必须是 v2 —— 否则"回滚错了"就无解。
        let pre = list(&dir.backups())
            .into_iter()
            .find(|s| s.id == report.pre)
            .expect("pre-rollback 快照必须在");
        let saved = pre.files_dir(&dir.backups()).join("app.exe");
        assert_eq!(fs::read_to_string(saved).unwrap(), "v2");
    }

    #[test]
    fn second_rollback_to_the_same_target_is_a_no_op() {
        let dir = TempDir::new("noop");
        dir.write("app.exe", "v1");
        snapshot(&dir.0, &dir.backups(), "before").expect("快照应当成功");
        dir.write("app.exe", "v2");
        rollback(&dir.0, &dir.backups(), None).expect("第一次回滚");
        let second = rollback(&dir.0, &dir.backups(), None).expect("第二次回滚");
        assert_eq!(second.restored.len(), 0, "已经一致就不该再动文件");
        assert!(second.unchanged >= 1);
    }

    #[test]
    fn verify_reports_changed_missing_and_extra() {
        let dir = TempDir::new("verify");
        dir.write("a.txt", "a");
        dir.write("b.txt", "b");
        snapshot(&dir.0, &dir.backups(), "before").expect("快照应当成功");
        dir.write("a.txt", "a-modified");
        fs::remove_file(dir.0.join("b.txt")).unwrap();
        dir.write("c.txt", "c");

        let report = verify(&dir.0, &dir.backups(), None).expect("校验应当成功");
        assert_eq!(report.changed, vec!["a.txt".to_owned()]);
        assert_eq!(report.missing, vec!["b.txt".to_owned()]);
        assert_eq!(report.extra, vec!["c.txt".to_owned()]);
        assert!(!report.is_clean());
    }

    #[test]
    fn verify_is_clean_right_after_a_snapshot() {
        let dir = TempDir::new("clean");
        dir.write("a.txt", "a");
        snapshot(&dir.0, &dir.backups(), "before").expect("快照应当成功");
        let report = verify(&dir.0, &dir.backups(), None).expect("校验应当成功");
        assert!(report.is_clean(), "{report:?}");
        assert!(report.extra.is_empty());
        assert_eq!(report.ok, 1);
    }

    #[test]
    fn gc_keeps_the_newest_n_and_never_empties_history() {
        let dir = TempDir::new("gc");
        dir.write("a.txt", "a");
        for i in 0..5 {
            dir.write("a.txt", &format!("v{i}"));
            // 同一秒内拍五份：id 会撞，所以靠后缀去重，顺序仍由 created_unix + id 决定。
            std::thread::sleep(std::time::Duration::from_millis(2));
            snapshot(&dir.0, &dir.backups(), "step").expect("快照应当成功");
        }
        let before = list(&dir.backups()).len();
        assert_eq!(before, 5);

        let removed = gc(&dir.backups(), 2).expect("gc 应当成功");
        assert_eq!(removed.len(), 3);
        assert_eq!(list(&dir.backups()).len(), 2);

        // keep=0 也要留下最新的一份。
        gc(&dir.backups(), 0).expect("gc 应当成功");
        assert_eq!(list(&dir.backups()).len(), 1);
    }

    #[test]
    fn stash_leftovers_do_not_pile_up_into_new_snapshots() {
        let dir = TempDir::new("stashskip");
        dir.write("app.exe", "v1");
        dir.write("app.exe.rollback-old", "junk");
        let snap = snapshot(&dir.0, &dir.backups(), "t").expect("快照应当成功");
        assert_eq!(snap.files.len(), 1, "残骸不该进快照");
    }

    #[test]
    fn rollback_to_an_unknown_id_is_an_error_not_a_silent_latest() {
        let dir = TempDir::new("unknown");
        dir.write("a.txt", "a");
        snapshot(&dir.0, &dir.backups(), "t").expect("快照应当成功");
        let err = rollback(&dir.0, &dir.backups(), Some("20990101T000000Z-nope"))
            .expect_err("指定不存在的快照必须报错");
        assert!(err.contains("没有 id"), "{err}");
    }

    #[test]
    fn prefix_match_is_accepted_but_ambiguity_is_refused() {
        let dir = TempDir::new("prefix");
        dir.write("a.txt", "a");
        let snap = snapshot(&dir.0, &dir.backups(), "t").expect("快照应当成功");
        let resolved = resolve(&dir.backups(), Some(&snap.id[..10])).expect("前缀应当能定位");
        assert_eq!(resolved.id, snap.id);
    }

    #[test]
    fn external_backup_dir_works_when_it_lives_outside_the_root() {
        // 卸载场景：备份必须能在被删的目录之外。
        let dir = TempDir::new("external-root");
        let outside = TempDir::new("external-backups");
        dir.write("stream-config.toml", "fps = 60");
        let snap = snapshot(&dir.0, &outside.0, "preuninstall").expect("快照应当成功");
        assert!(snap.dir(&outside.0).starts_with(&outside.0));

        // 目录被整个删掉（卸载），再解包回同一路径，从外面的备份恢复配置。
        fs::remove_dir_all(&dir.0).unwrap();
        fs::create_dir_all(&dir.0).unwrap();
        let report = rollback(&dir.0, &outside.0, None).expect("回滚应当成功");
        assert_eq!(report.restored, vec!["stream-config.toml".to_owned()]);
        assert_eq!(dir.read("stream-config.toml"), "fps = 60");
    }

    /// 这条守的是一个真实的坑：连续回滚几次之后，最新的几份全是自动留底，
    /// 如果 `gc` 只按"最近 N 份"删，就会把唯一有用的那份（人主动拍的
    /// "改动之前"）删掉，留下谁都不会想要的自动留底。
    #[test]
    fn gc_keeps_a_deliberate_target_instead_of_an_auto_one() {
        let dir = TempDir::new("gc-auto");
        dir.write("app.exe", "v1");
        snapshot(&dir.0, &dir.backups(), "before-update").expect("主动快照");
        dir.write("app.exe", "v2");
        // 回滚一次，产生一份 pre-rollback 自动留底（比主动快照更新）。
        rollback(&dir.0, &dir.backups(), None).expect("回滚");
        let labels: Vec<String> = list(&dir.backups()).into_iter().map(|s| s.label).collect();
        assert!(labels.contains(&PRE_ROLLBACK_LABEL.to_owned()), "{labels:?}");

        gc(&dir.backups(), 1).expect("gc");
        let left = list(&dir.backups());
        let ids: Vec<&str> = left.iter().map(|s| s.id.as_str()).collect();
        assert!(
            left.iter().any(|s| s.label == "before-update"),
            "主动快照必须留下，实际剩：{ids:?}"
        );
        // 自动留底最多留一份：它是"回滚错了还能退回来"的那条后路。
        assert_eq!(
            left.iter()
                .filter(|s| s.label == PRE_ROLLBACK_LABEL)
                .count(),
            1,
            "自动留底只该留最新的一份，实际剩：{ids:?}"
        );
    }

    #[test]
    fn default_target_skips_the_auto_snapshot() {
        let dir = TempDir::new("default-target");
        dir.write("app.exe", "v1");
        snapshot(&dir.0, &dir.backups(), "before").expect("主动快照");
        dir.write("app.exe", "v2");
        rollback(&dir.0, &dir.backups(), None).expect("回滚");
        // 回滚之后最新的是 pre-rollback。默认目标仍应是那份主动快照，
        // 否则连点两次回滚等于来回翻。
        let target = resolve(&dir.backups(), None).expect("应当有默认目标");
        assert_eq!(target.label, "before");
    }

    #[test]
    fn snapshot_paths_keeps_only_the_machine_identity() {
        // 卸载场景：只留配置与录音设备原值，不搬 bundle。
        let dir = TempDir::new("paths");
        dir.write("stream-config.toml", "fps = 15");
        dir.write("logs/mic-default.json", "{}");
        dir.write("dist/index.html", "<html>");
        dir.write("stream-server.exe", "bin");

        let only = vec!["stream-config.toml".to_owned(), "logs\\mic-default.json".to_owned()];
        let snap = snapshot_paths(&dir.0, &dir.backups(), "preuninstall", Some(&only))
            .expect("快照应当成功");
        let mut paths: Vec<&str> = snap.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, vec!["logs/mic-default.json", "stream-config.toml"]);
    }

    #[test]
    fn snapshot_paths_accepts_a_directory_and_recurses() {
        let dir = TempDir::new("paths-dir");
        dir.write("logs/a.log", "a");
        dir.write("logs/b.json", "b");
        dir.write("other.txt", "x");
        let only = vec!["logs".to_owned()];
        let snap = snapshot_paths(&dir.0, &dir.backups(), "t", Some(&only)).expect("快照应当成功");
        let paths: Vec<&str> = snap.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert!(paths.iter().all(|p| p.starts_with("logs/")), "{paths:?}");
    }

    #[test]
    fn request_parsing_is_strict_about_the_verb() {
        let args = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(Request::parse(&args(&["--port", "8080"])).is_none());
        let parsed = Request::parse(&args(&["--ops", "snapshot", "--ops-label", "x", "--ops-json"]))
            .expect("有 --ops")
            .expect("应当解析成功");
        assert_eq!(parsed.verb, Some(Verb::Snapshot));
        assert_eq!(parsed.label, "x");
        assert!(parsed.json);

        // 动词拼错必须是错误，而且不能悄悄回落到正常启动。
        let bad = Request::parse(&args(&["--ops", "snapsho"]))
            .expect("有 --ops")
            .expect_err("动词不认识应当报错");
        assert!(bad.contains("unknown --ops verb"), "{bad}");
    }

    #[test]
    fn compact_id_sorts_chronologically() {
        let early = compact_utc(1_700_000_000);
        let late = compact_utc(1_800_000_000);
        assert!(early < late, "{early} 应当排在 {late} 之前");
        assert_eq!(early.len(), 16);
    }
}
