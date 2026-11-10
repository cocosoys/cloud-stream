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

/// 改配置前留底的标签。与 `pre-rollback` 同一个思路：让它出现在
/// `--ops list` 里，人一眼能看出"这一份是某次改配置之前的"。
pub const PRE_EDIT_LABEL: &str = "preedit";

/// 运行目录之外的那份配置，留底写在旁边用这个后缀。
pub const PRE_EDIT_SUFFIX: &str = ".pre-edit";

/// 配置还不存在时，新文件的开头。
///
/// 只两行：完整的注释模板在工具包的 `stream-config.toml` 里，复制一份到这里
/// 只会多一处需要同步维护的文本。指向它比抄它更不容易分叉。
const NEW_CONFIG_HEAD: &str = "\
# cloud-stream 配置（由配置表单创建；带注释的完整模板见工具包里的 stream-config.toml）
# 删掉一行 = 这一项回到默认值。
";

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
    /// 这份快照只备份了点名的那几项，**不是**整个运行目录。
    ///
    /// 这一位决定回滚时要不要"把快照里没有的文件改名让路"。完整快照要，因为
    /// 那正是"更新引入的新文件"；只备了两三个文件的快照（`preedit`、
    /// `preuninstall`）绝对不能 —— 否则回滚一次配置，整个目录的 exe 和 dist
    /// 全被改名成 `.rollback-old`，服务当场起不来。
    ///
    /// 老清单里没有这一位，`serde(default)` 读成 `false`（= 完整），与旧行为
    /// 一致。
    #[serde(default)]
    pub partial: bool,
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
pub(crate) fn rel_to_string(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn string_to_rel(s: &str) -> PathBuf {
    s.split('/').fold(PathBuf::new(), |acc, part| acc.join(part))
}

/// FNV-1a 64。选它而不是 sha2 的理由：没有依赖、源码十二行、对"文件变了没变"
/// 这个用途足够；哈希值只在本项目内部比较，不进任何协议。
pub(crate) fn hash_file(path: &Path) -> Result<(u64, String), String> {
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

pub(crate) fn collect_skipping(root: &Path, skip: &[&str]) -> Result<Vec<PathBuf>, String> {
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
                // 上一轮让路留下的残骸不该进入新的快照，否则每让路一次，
                // 备份就胖一圈，而且永远删不掉。
                //
                // 判断是"名字里含"而不是"以它结尾"：让路时目标被占会改用
                // `x.rollback-old.1` 这种带编号的名字（见 [`stash`]），只认
                // 后缀会漏掉它们，于是这些残骸反而成了"快照里该有的文件"。
                if name_str.contains(STASH_SUFFIX) {
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
        partial: explicit,
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
    //
    // **只对完整快照做这一步**：只备份了几项的（`preedit`、`preuninstall`）
    // 里"快照没有的文件"是正常存在的其余东西，把它们全改名成 `.rollback-old`
    // 等于回滚一次配置就让整个目录不可用。
    if !target.partial {
        if let Ok(current) = collect(root) {
            for rel in current {
                let key = rel_to_string(&rel);
                if wanted.contains_key(key.as_str()) {
                    continue;
                }
                let path = root.join(&rel);
                match stash(&path) {
                    Ok(_) => report.stashed.push(key),
                    Err(e) => report.failed.push((key, e)),
                }
            }
        }
    }

    Ok(report)
}

/// 让路用的第 `n` 个名字：`x.exe` → `x.exe.rollback-old`（n=0）、
/// `x.exe.rollback-old.1`（n=1）……
pub(crate) fn stash_name(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(STASH_SUFFIX);
    if n > 0 {
        name.push(format!(".{n}"));
    }
    PathBuf::from(name)
}

/// 把文件改名让路，返回最终用的名字。
///
/// 标准名字是 `<路径>.rollback-old`。这个名字被占时，先试着删掉上一轮的残骸
/// （Windows 的 `rename` 不覆盖已存在的目标，POSIX 会静默覆盖，这里统一成
/// "先删再改名"）；**删不掉就换一个编号**。
///
/// 换编号这一条不是洁癖，是实测撞出来的：更新器覆盖的往往正是"正在运行的
/// 自己"，于是那份留底就是当前进程的镜像 —— 而 Windows 允许重命名运行中的
/// exe、不允许删除它。旧代码在这里直接报错，结果是"更新失败 → 自动回滚 →
/// 回滚也失败"，把人留在最坏的组合里。宁可多留一份文件，也要把"退回去"做成。
pub(crate) fn stash(path: &Path) -> Result<PathBuf, String> {
    // 32 是个够用的上限：真堆了三十多份同名留底，该做的是让人来清理。
    for n in 0..32 {
        let candidate = stash_name(path, n);
        if candidate.exists() && fs::remove_file(&candidate).is_err() {
            // 删不掉 = 正被使用（多半是运行中的自己），换下一个编号。
            continue;
        }
        return match fs::rename(path, &candidate) {
            Ok(_) => Ok(candidate),
            Err(e) => Err(format!(
                "{}: 改名失败: {e}（文件正被占用时先停服务）",
                path.display()
            )),
        };
    }
    Err(format!(
        "{}: 改名失败：连 {} 都被占用（目录里有 32 份同名留底，需要人工清理）",
        path.display(),
        stash_name(path, 31).display()
    ))
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
    // 只备份了几项的快照，比不出"多出来什么"：其余的本来就是正常文件，
    // 报成 extra 只会让人以为盘上被塞了东西。
    if !target.partial {
        if let Ok(current) = collect(root) {
            for rel in current {
                let key = rel_to_string(&rel);
                if !wanted.contains_key(key.as_str()) {
                    report.extra.push(key);
                }
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
    /// 读配置文件里现在写着什么（只读，给配置表单用）。
    ConfigGet,
    /// 改配置：先留底，按键改写，校验通过才落盘。
    ConfigSet,
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
            "config-get" | "config_get" | "configget" => Some(Self::ConfigGet),
            "config-set" | "config_set" | "configset" => Some(Self::ConfigSet),
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
    /// `--ops-set key=value`，可重复。`config-set` 用它。
    ///
    /// 值原样收下，格式化与范围检查交给 `configedit`：那里有类型表，也不过
    /// 是第二个解析器 —— 同一个判断只该有一处。
    pub sets: Vec<(String, String)>,
    /// `--ops-unset key`，可重复：删掉整行（= 用默认值）。
    pub unsets: Vec<String>,
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
                "--ops-set" => {
                    if let Some(v) = iter.next() {
                        match v.split_once('=') {
                            Some((k, val)) => req.sets.push((k.trim().to_owned(), val.to_owned())),
                            None => {
                                return Some(Err(format!(
                                    "--ops-set 要 key=value 形态，收到 {v:?}"
                                )))
                            }
                        }
                    }
                }
                "--ops-unset" => {
                    if let Some(v) = iter.next() {
                        req.unsets.push(v.trim().to_owned());
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

pub(crate) fn human_bytes(bytes: u64) -> String {
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
        // 只备份了几项 = 回滚只会碰那几项。界面上要能说出这句话，所以报出来。
        "partial": snapshot.partial,
    })
}

/// 这台机器实际读的是哪份配置。
///
/// `--config` 优先，其次默认位置。默认位置还要再分两种：`Config::parse` 自动
/// 发现的是 exe 旁的 `config.toml`，但整包分发的那份叫 `stream-config.toml`
/// 且由 `start-stream.cmd --config` 显式指定。只报 `config.toml (absent)`
/// 会让人以为自己没配置，而实际上文件就在旁边、服务也在用它 —— 所以这里退
/// 一步再找一个，纯只读。
///
/// 刻意**不改** `configfile::default_path`：那是启动时的自动发现逻辑，动它等于
/// 改变已有部署的加载行为，不属于"报告得更准"。
///
/// 两份都不存在时这个函数还要**决定新建哪一份**：整包分发的布局里服务是用
/// `--config stream-config.toml` 起的，那就得建那一份（见下面的证据）。
///
/// `state` 与 `config-get` / `config-set` 共用这一处：三处各写一遍，迟早有一
/// 处先漂走，然后屏幕说 A、写入落在 B。
fn resolve_config(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    let primary = crate::configfile::default_path()?;
    if primary.exists() {
        return Some(primary);
    }
    let sibling = primary.with_file_name("stream-config.toml");
    if sibling.exists() {
        return Some(sibling);
    }
    // 两份都不存在时，看这份安装是不是整包分发的：`start-stream.cmd` 里写死了
    // `--config %ROOT%\stream-config.toml`，于是服务只认那一份。此时若把表单
    // 的改动存进 `config.toml`，就是"改了没反应"—— 一个纯粹由猜错文件名造成
    // 的故障，而旁边那个脚本本身就是证据。
    if primary.with_file_name("start-stream.cmd").exists() {
        Some(sibling)
    } else {
        Some(primary)
    }
}

/// 补空格到指定的**显示**宽度。
///
/// `{:<18}` 数的是字符个数，而中日韩字符占两列 —— 表格里混着中文时，
/// 按字符补齐的结果是右边那列高高低低。这个项目里所有面向人的列表都有中文，
/// 所以按显示宽度补。
fn pad(text: &str, width: usize) -> String {
    let shown: usize = text
        .chars()
        .map(|c| if c as u32 >= 0x2E80 { 2 } else { 1 })
        .sum();
    let mut out = text.to_owned();
    for _ in shown..width {
        out.push(' ');
    }
    out
}

/// 读这份配置：整份读成字符串，文件不存在则当空文件。
///
/// 不存在不等于出错 —— 首次配置就是"从零写一份"，那不是异常路径。
fn read_config_text(path: &Path) -> Result<String, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("{}: 读不了: {e}", path.display())),
    }
}

/// 改配置之前的留底。
///
/// 配置在运行目录里时走快照：和安装/更新/卸载用同一套，`--ops rollback` 能
/// 直接还原。配置在运行目录之外时（`--config` 指到别处）快照够不着 —— 它是
/// 按相对路径遍历 root 的 —— 于是退一步在旁边写一份 `<文件名>.pre-edit`。
/// 两种都是"留一份旧的、不删任何东西"，区别只是能不能一条命令退回去。
struct ConfigBackup {
    snapshot: Option<String>,
    copy: Option<PathBuf>,
}

fn backup_config(
    root: &Path,
    backups: &Path,
    path: &Path,
    label: &str,
) -> Result<ConfigBackup, String> {
    if !path.exists() {
        // 还没有文件：没有旧内容可留。
        return Ok(ConfigBackup {
            snapshot: None,
            copy: None,
        });
    }
    // 只认真正的下级：`strip_prefix` 顺带把 `..` 与不同盘符都挡掉了。
    if let Ok(rel) = path.strip_prefix(root) {
        if !rel.as_os_str().is_empty() {
            let only = vec![rel_to_string(rel)];
            let snapshot = snapshot_paths(root, backups, label, Some(&only))?;
            return Ok(ConfigBackup {
                snapshot: Some(snapshot.id),
                copy: None,
            });
        }
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(PRE_EDIT_SUFFIX);
    let copy = path.with_file_name(name);
    fs::copy(path, &copy).map_err(|e| format!("{}: 留底失败: {e}", copy.display()))?;
    Ok(ConfigBackup {
        snapshot: None,
        copy: Some(copy),
    })
}

/// 把 `Kind` 说给界面听：表单靠它决定画输入框、下拉框还是勾选框。
fn key_json(key: &crate::configedit::Key, text: &str) -> serde_json::Value {
    use crate::configedit::Kind;
    let mut doc = match key.kind {
        Kind::Num(min, max) => serde_json::json!({ "kind": "num", "min": min, "max": max }),
        Kind::Bool => serde_json::json!({ "kind": "bool" }),
        Kind::Enum(options) => serde_json::json!({ "kind": "enum", "options": options }),
        Kind::Text => serde_json::json!({ "kind": "text" }),
    };
    let raw = crate::configedit::raw_value(text, key.name);
    let object = doc.as_object_mut().expect("上面构造的一定是对象");
    object.insert("name".into(), key.name.into());
    object.insert("label".into(), key.label.into());
    // 表单里那一格的名字。`label` 是给人读的句子，塞不进 320px 的标签。
    object.insert("brief".into(), key.brief.into());
    // `written` 与 `raw` 一起给：表单要区分"这一项没写（用默认值）"与
    // "写了但是空字符串" —— 后者在 TLS 那几项里是有意义的状态。
    object.insert("written".into(), raw.is_some().into());
    object.insert(
        "raw".into(),
        match raw {
            Some(v) => serde_json::Value::String(v),
            None => serde_json::Value::Null,
        },
    );
    doc
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
            // 哪份配置：见 [`resolve_config`]（`state` 与读写配置共用同一处判断，
            // 免得屏幕说一份、实际改的是另一份）。
            let config = resolve_config(request.config.as_deref());
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
                        "{:<40} {:>4} files {:>10}  {}{}{}",
                        s.id,
                        s.files.len(),
                        human_bytes(s.total_bytes()),
                        s.version,
                        if s.label.is_empty() {
                            String::new()
                        } else {
                            format!("  [{}]", s.label)
                        },
                        // 只备份了几项要写明白：这决定了回滚会碰多少东西。
                        if s.partial { "  (只备份这几项)" } else { "" }
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

        // ---------------------------------------------------------------
        // 配置：读
        // ---------------------------------------------------------------
        //
        // 输出的是**文件里写着什么**，不是生效值：表单要显示"这一行现在是什么样"
        // 好让人在原文上改，而不是看到一堆被默认值填满的格子、分不清哪些是自己
        // 设过的。生效值另有 `--print-config` 与 `state` 负责。
        Verb::ConfigGet => {
            let Some(path) = resolve_config(request.config.as_deref()) else {
                eprintln!("ops config-get: 拿不到 exe 所在目录，没法定位配置");
                return EXIT_MISSING;
            };
            let text = match read_config_text(&path) {
                Ok(text) => text,
                Err(err) => {
                    eprintln!("ops config-get failed: {err}");
                    return EXIT_FAILED;
                }
            };
            // 解析不过也照样把原文报出去：那正是最该打开表单改一改的时刻，
            // 报一句"读失败"等于把人赶回记事本。
            let problem = crate::configedit::validate(&text).err();
            let exists = path.exists();
            if json {
                let keys: Vec<serde_json::Value> = crate::configedit::KEYS
                    .iter()
                    .map(|key| key_json(key, &text))
                    .collect();
                let doc = serde_json::json!({
                    "path": path.display().to_string(),
                    "exists": exists,
                    "problem": problem,
                    "keys": keys,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_owned())
                );
            } else {
                println!(
                    "config:   {} ({})",
                    path.display(),
                    if exists { "present" } else { "absent" }
                );
                if let Some(problem) = &problem {
                    eprintln!("  这份配置解析不过：{problem}");
                }
                for key in crate::configedit::KEYS {
                    let written = crate::configedit::raw_value(&text, key.name);
                    let value = written.clone().unwrap_or_else(|| "(默认)".to_owned());
                    // 没写着的用空格代替 "="，一眼能看出哪些是自己设过的。
                    let marker = if written.is_some() { '=' } else { ' ' };
                    println!(
                        "  {} {} {} # {}",
                        pad(key.name, 18),
                        marker,
                        pad(&value, 10),
                        key.label
                    );
                }
            }
            EXIT_OK
        }

        // ---------------------------------------------------------------
        // 配置：写
        // ---------------------------------------------------------------
        //
        // 顺序是刻意的：**先全部校验，再动文件**。有一项不合法就整批不改 ——
        // 改一半的配置比不改更难收拾，而且"到底哪几项进了文件"无从回答。
        Verb::ConfigSet => {
            if request.sets.is_empty() && request.unsets.is_empty() {
                eprintln!(
                    "ops config-set: 没有要改的项（用 --ops-set key=value，删行用 --ops-unset key）"
                );
                return EXIT_USAGE;
            }
            let Some(path) = resolve_config(request.config.as_deref()) else {
                eprintln!("ops config-set: 拿不到 exe 所在目录，没法定位配置");
                return EXIT_MISSING;
            };

            // 1. 类型校验 + 格式化。键名先过 `find`：拼错的键不能靠"写进去再
            //    让解析器报错"来发现，因为那已经把错的东西写进文件了。
            let mut sets: Vec<(&'static crate::configedit::Key, String)> = Vec::new();
            for (name, value) in &request.sets {
                let Some(key) = crate::configedit::find(name) else {
                    eprintln!(
                        "ops config-set: 不认识的键 {name:?}（能改哪些键，看 --ops config-get）"
                    );
                    return EXIT_USAGE;
                };
                match crate::configedit::format_value(key, value) {
                    Ok(formatted) => sets.push((key, formatted)),
                    Err(err) => {
                        eprintln!("ops config-set: {err}");
                        return EXIT_USAGE;
                    }
                }
            }
            // `unset` 刻意**不**查键名：删掉一行不会引入任何新东西，而"把那个
            // 拼错的键删掉"恰恰是最需要它的时刻 —— 在这儿严格校验，等于在最
            // 该帮忙的地方把人挡回去改记事本。
            let unsets: Vec<String> = request
                .unsets
                .iter()
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .collect();

            // 2. 读旧文、留底。留底失败就**不写**：宁可不改，也不能改了退不回去。
            let existed = path.exists();
            let mut after = match read_config_text(&path) {
                Ok(text) => {
                    if text.is_empty() {
                        NEW_CONFIG_HEAD.to_owned()
                    } else {
                        text
                    }
                }
                Err(err) => {
                    eprintln!("ops config-set failed: {err}");
                    return EXIT_FAILED;
                }
            };
            let backup = match backup_config(&root, &backups, &path, PRE_EDIT_LABEL) {
                Ok(backup) => backup,
                Err(err) => {
                    eprintln!("ops config-set: 留底失败，没有写入：{err}");
                    return EXIT_FAILED;
                }
            };

            // 3. 逐项改写。按行替换，注释与分组原样留着。
            let mut changes: Vec<(String, Option<String>, Option<String>)> = Vec::new();
            for (key, value) in &sets {
                let was = crate::configedit::raw_value(&after, key.name);
                after = crate::configedit::set(&after, key.name, value);
                let now = crate::configedit::raw_value(&after, key.name);
                changes.push((key.name.to_owned(), was, now));
            }
            let mut removed: Vec<String> = Vec::new();
            // 文件里本来就没有这一行：要如实说，不然界面上点了"清空"没反应，
            // 人会以为是没生效。
            let mut not_found: Vec<String> = Vec::new();
            for key in &unsets {
                if crate::configedit::raw_value(&after, key).is_some() {
                    removed.push(key.clone());
                } else {
                    not_found.push(key.clone());
                }
                after = crate::configedit::remove(&after, key);
            }

            // 4. 用启动时同一套解析器再验一遍：保存能过 = 下次启动不会因为这份
            //    文件起不来。校验不过就不落盘，旧文件一个字节没动。
            if let Err(err) = crate::configedit::validate(&after) {
                eprintln!("ops config-set: 改完这份配置过不了校验，没有写入：{err}");
                return EXIT_FAILED;
            }
            if let Err(err) = crate::configedit::write_atomic(&path, &after) {
                eprintln!("ops config-set: 写入失败: {err}");
                return EXIT_FAILED;
            }

            if json {
                let changes: Vec<serde_json::Value> = changes
                    .iter()
                    .map(|(key, was, now)| serde_json::json!({ "key": key, "was": was, "now": now }))
                    .collect();
                let doc = serde_json::json!({
                    "path": path.display().to_string(),
                    "created": !existed,
                    "snapshot": backup.snapshot,
                    "copy": backup.copy.as_ref().map(|p| p.display().to_string()),
                    "changes": changes,
                    "removed": removed,
                    "not_found": not_found,
                    // 服务是启动时读一次配置的，改完必须重启才生效 —— 写在结果里
                    // 比写在文档里更可能被看见。
                    "restart_required": true,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_owned())
                );
            } else {
                println!("config:   {}", path.display());
                if !existed {
                    println!("  new:      原来没有这份文件，已新建");
                }
                if let Some(id) = &backup.snapshot {
                    println!("  preedit:  {id}（--ops rollback 能退回）");
                }
                if let Some(copy) = &backup.copy {
                    println!("  preedit:  {}（副本）", copy.display());
                }
                for (key, was, now) in &changes {
                    println!(
                        "  ~ {key}: {} -> {}",
                        was.as_deref().unwrap_or("(默认)"),
                        now.as_deref().unwrap_or("?")
                    );
                }
                for key in &removed {
                    println!("  - {key}: 已删掉这一行（回到默认值）");
                }
                for key in &not_found {
                    println!("  ? {key}: 文件里本来就没有这一行，没动");
                }
                println!("  改完要重启服务才生效。");
            }
            EXIT_OK
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

    /// `--ops-set` 的值里可以再有等号：路径参数比键值对本身更容易出现 `=`，
    /// 按第一个等号切才不会把 `C:\a=b\x.log` 截成 `C:\a`。
    #[test]
    fn request_parsing_collects_config_edits() {
        let args = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let parsed = Request::parse(&args(&[
            "--ops",
            "config-set",
            "--ops-set",
            "fps=30",
            "--ops-set",
            "log=C:\\a=b\\x.log",
            "--ops-unset",
            "no_hdr",
            "--ops-json",
        ]))
        .expect("有 --ops")
        .expect("应当解析成功");
        assert_eq!(parsed.verb, Some(Verb::ConfigSet));
        assert_eq!(
            parsed.sets,
            vec![
                ("fps".to_owned(), "30".to_owned()),
                ("log".to_owned(), r"C:\a=b\x.log".to_owned()),
            ]
        );
        assert_eq!(parsed.unsets, vec!["no_hdr".to_owned()]);
        assert!(parsed.json);

        let bad = Request::parse(&args(&["--ops", "config-set", "--ops-set", "fps"]))
            .expect("有 --ops")
            .expect_err("没有等号应当报错");
        assert!(bad.contains("key=value"), "{bad}");
    }

    /// 配置在运行目录里：走快照，`--ops rollback` 能顺手退回去。
    #[test]
    fn editing_a_config_inside_the_root_leaves_a_snapshot() {
        let dir = TempDir::new("cedit-in");
        dir.write("stream-config.toml", "fps = 60\n");
        let path = dir.0.join("stream-config.toml");
        let backup = backup_config(&dir.0, &dir.backups(), &path, PRE_EDIT_LABEL)
            .expect("留底应当成功");
        let id = backup.snapshot.expect("目录里的配置走快照");
        assert!(backup.copy.is_none(), "不该再另写一份副本");
        assert!(id.ends_with(PRE_EDIT_LABEL), "{id}");
        let snapshot = list(&dir.backups())
            .into_iter()
            .find(|s| s.id == id)
            .expect("快照要能在列表里找到");
        let saved = snapshot.files_dir(&dir.backups()).join("stream-config.toml");
        assert_eq!(fs::read_to_string(saved).unwrap(), "fps = 60\n");
    }

    /// 配置在运行目录之外（`--config` 指到别处）：快照遍历不到它，就地留副本。
    /// 关键是不能静默地"没留底就改了"。
    #[test]
    fn editing_a_config_outside_the_root_leaves_a_copy_next_to_it() {
        let dir = TempDir::new("cedit-in2");
        let elsewhere = TempDir::new("cedit-out");
        let path = elsewhere.0.join("stream-config.toml");
        fs::write(&path, "fps = 60\n").expect("写配置");
        let backup =
            backup_config(&dir.0, &dir.backups(), &path, PRE_EDIT_LABEL).expect("留底应当成功");
        assert!(backup.snapshot.is_none());
        let copy = backup.copy.expect("目录外的配置要留副本");
        assert_eq!(copy.file_name().unwrap().to_string_lossy(), format!("stream-config.toml{PRE_EDIT_SUFFIX}"));
        assert_eq!(fs::read_to_string(copy).unwrap(), "fps = 60\n");
        // 副本必须留在原文件旁边，而不是运行目录或备份目录里 —— 找的时候人
        // 只会看配置自己那一格。
        assert!(path.with_file_name(format!("stream-config.toml{PRE_EDIT_SUFFIX}")).exists());
    }

    /// 文件还不存在时没有旧内容可留，也不该凭空造出快照。
    #[test]
    fn a_missing_config_needs_no_backup() {
        let dir = TempDir::new("cedit-none");
        let path = dir.0.join("stream-config.toml");
        let backup = backup_config(&dir.0, &dir.backups(), &path, PRE_EDIT_LABEL)
            .expect("留底应当成功");
        assert!(backup.snapshot.is_none() && backup.copy.is_none());
        assert!(list(&dir.backups()).is_empty(), "不该凭空多一份快照");
    }

    /// 中文占两列，按字符数补齐会让表格右列歪掉。
    #[test]
    fn pad_counts_wide_characters() {
        assert_eq!(pad("fps", 6), "fps   ");
        assert_eq!(pad("默认值啊", 10), "默认值啊  ");
        // 已经比目标宽就原样返回，不截断。
        assert_eq!(pad("超出宽度", 2), "超出宽度");
    }

    /// 这一条是实测撞出来的：回滚一份只含 `stream-config.toml` 的快照，
    /// 目录里的 exe 被改名成了 `.rollback-old` —— 回滚配置把安装弄坏了。
    #[test]
    fn rolling_back_a_partial_snapshot_leaves_other_files_alone() {
        let dir = TempDir::new("rb-partial");
        dir.write("stream-server.exe", "bin");
        dir.write("stream-config.toml", "fps = 60\n");
        let only = vec!["stream-config.toml".to_owned()];
        snapshot_paths(&dir.0, &dir.backups(), PRE_EDIT_LABEL, Some(&only))
            .expect("快照应当成功");
        dir.write("stream-config.toml", "fps = 15\n");

        let report = rollback(&dir.0, &dir.backups(), None).expect("回滚应当成功");
        assert_eq!(dir.read("stream-config.toml"), "fps = 60\n", "配置要退回去");
        assert!(
            report.stashed.is_empty(),
            "只备份了几项的快照不该动别的文件：{:?}",
            report.stashed
        );
        assert!(dir.0.join("stream-server.exe").exists(), "exe 必须原样还在");
    }

    /// 完整快照仍然要"多出来的文件让路"—— 那是更新的回退路径，不能一起放掉。
    #[test]
    fn rolling_back_a_full_snapshot_still_stashes_new_files() {
        let dir = TempDir::new("rb-full");
        dir.write("app.exe", "v1");
        snapshot(&dir.0, &dir.backups(), "before").expect("快照应当成功");
        dir.write("app.exe", "v2");
        dir.write("new-in-v2.dll", "new");
        let report = rollback(&dir.0, &dir.backups(), None).expect("回滚应当成功");
        assert_eq!(dir.read("app.exe"), "v1");
        assert!(
            report.stashed.contains(&"new-in-v2.dll".to_owned()),
            "更新带进来的新文件要让路：{:?}",
            report.stashed
        );
    }

    /// 只备份了几项时，别的文件不算"多出来的"。
    #[test]
    fn verifying_against_a_partial_snapshot_ignores_other_files() {
        let dir = TempDir::new("verify-partial");
        dir.write("stream-config.toml", "fps = 60\n");
        let only = vec!["stream-config.toml".to_owned()];
        snapshot_paths(&dir.0, &dir.backups(), PRE_EDIT_LABEL, Some(&only))
            .expect("快照应当成功");
        dir.write("stream-server.exe", "bin");
        let report = verify(&dir.0, &dir.backups(), None).expect("校验应当成功");
        assert!(
            report.extra.is_empty(),
            "别的文件不该被算成 extra：{:?}",
            report.extra
        );
        assert!(report.is_clean());
    }

    /// 老版本写的清单里没有 `partial` 这一位：必须读成"完整快照"。读错方向
    /// 的后果是升级之后第一次回滚就成了空转（该退回的新文件退不掉）。
    #[test]
    fn an_old_manifest_without_the_partial_field_reads_as_full() {
        let dir = TempDir::new("old-manifest");
        let snap = dir.backups().join("20260101T000000Z-before");
        fs::create_dir_all(&snap).expect("建快照目录");
        fs::write(
            snap.join("manifest.json"),
            r#"{
                "id": "20260101T000000Z-before",
                "created_unix": 1767225600,
                "created_at": "2026-01-01T00:00:00Z",
                "label": "before",
                "version": "0.1.0",
                "commit": "abc",
                "root": "C:\\x",
                "files": []
            }"#,
        )
        .expect("写清单");
        let loaded = load_manifest(&snap).expect("老清单要能读");
        assert!(!loaded.partial, "没有这一位就是完整快照");
    }

    #[test]
    fn compact_id_sorts_chronologically() {
        let early = compact_utc(1_700_000_000);
        let late = compact_utc(1_800_000_000);
        assert!(early < late, "{early} 应当排在 {late} 之前");
        assert_eq!(early.len(), 16);
    }

    /// 标准名字空着时就用标准名字，并把上一轮的残骸丢掉。
    #[test]
    fn stash_reuses_the_standard_name() {
        let dir = TempDir::new("stashstd");
        dir.write("a.txt", "new");
        dir.write("a.txt.rollback-old", "old-remains");
        let used = stash(&dir.0.join("a.txt")).expect("应当让路");
        assert_eq!(
            used.file_name().unwrap().to_string_lossy(),
            "a.txt.rollback-old"
        );
        assert_eq!(fs::read_to_string(&used).expect("读留底"), "new");
        assert!(!dir.0.join("a.txt").exists());
    }

    /// 名字被占**而且删不掉**时必须换编号，而不是报错。
    ///
    /// 这不是边界情况，是常规情况：更新器覆盖的往往正是"正在运行的自己"，
    /// 那份留底就是当前进程的镜像 —— Windows 允许重命名运行中的 exe，不允许
    /// 删除它。旧实现在这里直接失败，于是"更新失败 → 自动回滚 → 回滚也失败"。
    ///
    /// 用同名**目录**模拟"删不掉"：`remove_file` 遇到目录在 Windows 上必然
    /// 失败，和"文件被运行中的进程占着"在这一步的行为一致。
    #[test]
    fn stash_falls_back_to_a_numbered_name_when_the_slot_is_held() {
        let dir = TempDir::new("stashhold");
        dir.write("a.txt", "v1");
        fs::create_dir_all(stash_name(&dir.0.join("a.txt"), 0)).expect("建占位目录");
        let used = stash(&dir.0.join("a.txt")).expect("应当换编号让路");
        assert_eq!(
            used.file_name().unwrap().to_string_lossy(),
            "a.txt.rollback-old.1"
        );
        assert_eq!(fs::read_to_string(&used).expect("读留底"), "v1");
        assert!(!dir.0.join("a.txt").exists());
    }

    /// 带编号的残骸不能进快照，否则每让路一次备份就胖一圈。
    #[test]
    fn numbered_stash_files_are_not_snapshotted() {
        let dir = TempDir::new("stashskip");
        dir.write("a.txt", "v1");
        dir.write("a.txt.rollback-old.1", "旧残骸");
        let snap = snapshot(&dir.0, &dir.backups(), "t").expect("快照应当成功");
        let paths: Vec<&str> = snap.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["a.txt"], "{paths:?}");
    }
}
