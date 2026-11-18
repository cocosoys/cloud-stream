//! JRE 探测与选择：从「这个版本要求 Java 17」到「就用这一个 `javaw.exe`」。
//!
//! ## 为什么这件事该由服务做
//!
//! 版本 json 从 1.13 起带 `javaVersion.majorVersion`，官方启动器据此下载对应的
//! 运行时。我们以前完全不看这个字段，`java.path` 由实例手写一个绝对路径 ——
//! 于是同一台机器上「1.12.2 要 8、1.20 要 17」这件事只能靠人记住并改配置。
//! 症状是两类，都很贵：
//!
//! * 用 Java 17 起 1.12.2 Forge：`Unsupported class file major version 61` 或者
//!   LaunchWrapper 的反射报错，字面上与「你选错了 java」没有任何关系；
//! * 用 Java 8 起 1.20：同样的错，方向相反。
//!
//! ## 三个刻意的选择
//!
//! * **先读 `release` 文件，读不到才起进程。** 每个 JDK/JRE 根目录下都有一个
//!   `release` 文件（`JAVA_VERSION="17.0.9"`），读它是一次 fopen；`java -version`
//!   是起一个进程 + 解析 stderr + 可能挂住。探测要扫十几个目录，只有前者能接受。
//! * **版本对不上时拒绝启动，而不是硬试。** 用 Java 8 起 1.20 是**必然**失败，
//!   而且失败信息指不到 JRE —— 与其让 `javaw` 打一行 `UnsupportedClassVersionError`
//!   秒退，不如在这里说清「要求 17、这台机器上有 8 和 11」。
//! * **显式 `java.path` 永远优先，哪怕版本对不上。** 那是人写的，可能是有意的
//!   （自编译的 JRE、带 agent 的定制运行时）。这种情况只警告，不拦。
//!
//! 候选顺序见 [`Origin`]：随部署走的 `runtime\jre` 在最前 —— 它跟着包走，
//! 换一台机器不该换结论。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::{JavaSpec, Paths};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 这台机器上历史遗留的几个固定位置。
///
/// 留着它们不是为了"兼容"，是因为 `.128` 上现在跑的正是其中一个，而"升级服务
/// 之后游戏起不来"是最不该发生的事。放在 [`Origin::Legacy`]，任何其它来源都能
/// 顶掉它。
const LEGACY_HOMES: &[&str] = &[r"C:\mc\jre8", r"C:\dev\jdk8\jdk8u504-b01"];

/// 系统上的 JDK 发行商安装根。每个根下面的一级子目录是一个 JRE 根。
const VENDOR_ROOTS: &[&str] = &[
    r"Java",
    r"Eclipse Adoptium",
    r"Microsoft",
    r"Zulu",
    r"BellSoft",
    r"Amazon Corretto",
    r"AdoptOpenJDK",
    r"Semeru",
];

/// 候选 JRE 的来源。数值越小优先级越高。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `games\runtime\jre\` 下随部署放置的运行时。
    Vendored,
    /// 环境变量 `JAVA_HOME`。
    JavaHome,
    /// 系统安装目录（Program Files 下几个常见厂商）。
    Installed,
    /// 客户端目录自带的 `runtime\`（官方启动器下载的那一份）。
    ClientRuntime,
    /// 历史遗留的固定路径（[`LEGACY_HOMES`]）。
    Legacy,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Vendored => "runtime",
            Origin::JavaHome => "JAVA_HOME",
            Origin::Installed => "installed",
            Origin::ClientRuntime => "client",
            Origin::Legacy => "legacy",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Origin::Vendored => 0,
            Origin::JavaHome => 1,
            Origin::Installed => 2,
            Origin::ClientRuntime => 3,
            Origin::Legacy => 4,
        }
    }
}

/// 一个找到的 JRE。
#[derive(Debug, Clone)]
pub struct Candidate {
    /// `bin\javaw.exe`（没有则 `bin\java.exe`）。
    pub java: PathBuf,
    /// JRE 根（`bin` 的上一层）。
    pub home: PathBuf,
    /// 主版本号。`None` = 读不到也起不来（坏路径、被截断的安装）。
    pub major: Option<u32>,
    pub origin: Origin,
    /// 发现位置的可读描述，进日志。
    pub source: String,
}

/// 选中的结果。
///
/// `note` 与 `warning` 分开，是因为它们进的是两个不同的地方：前者是「选了这个」，
/// 属于事实；后者是「你得看一眼」，属于承诺 —— 版本不匹配时游戏很可能起不来。
#[derive(Debug, Clone)]
pub struct Choice {
    pub java: PathBuf,
    pub major: Option<u32>,
    pub note: String,
    pub warning: Option<String>,
}

/// 按版本要求挑一个 JRE。
///
/// `required` 来自版本 json 的 `javaVersion.majorVersion`，或实例里的
/// `java.major` 覆盖（后者优先，见 `InstanceSpec::plan`）。
pub fn select(
    spec: &JavaSpec,
    paths: &Paths,
    client: &Path,
    required: Option<u32>,
) -> Result<Choice, String> {
    if !spec.path.trim().is_empty() {
        return explicit(spec, paths, required);
    }

    let cands = discover(paths, client);
    if cands.is_empty() {
        return Err(format!(
            "找不到任何 JRE；扫过 {}。把 JRE 放进 {} 下（任一子目录，里面要有 bin\\javaw.exe），\
             或在实例里写 java.path",
            searched_roots(paths, client).join("、"),
            paths.runtime().join("jre").display()
        ));
    }
    let dest = paths.runtime().join("jre");
    let (idx, warning) = pick(&cands, required, &dest)?;
    let c = &cands[idx];
    // `JAVA_HOME` 这类候选的"来源"与来源标签是同一个词，重复一遍只是噪音。
    let from = if c.source == c.origin.label() {
        c.origin.label().to_owned()
    } else {
        format!("{}：{}", c.origin.label(), c.source)
    };
    let note = format!(
        "{}（{}，来自 {from}）",
        c.java.display(),
        match c.major {
            Some(m) => format!("Java {m}"),
            None => "版本未知".to_owned(),
        }
    );
    Ok(Choice { java: c.java.clone(), major: c.major, note, warning })
}

/// 实例里写了 `java.path`：照办，版本不符只警告。
fn explicit(spec: &JavaSpec, paths: &Paths, required: Option<u32>) -> Result<Choice, String> {
    let p = super::InstanceSpec::under_root(paths, &spec.path);
    if !p.is_file() {
        return Err(format!("java.path 指向的文件不存在：{}", p.display()));
    }
    let home = home_of(&p);
    let major = home.as_deref().and_then(probe_major);
    let mut warning = None;
    if let (Some(req), Some(got)) = (required, major) {
        if req != got {
            let who = if spec.major > 0 { "实例 java.major" } else { "版本 json" };
            warning = Some(format!(
                "java.path 指定的是 Java {got}，而{who} 要求 Java {req} —— 按实例配置照用，起不来时先改这里"
            ));
        }
    }
    let note = format!(
        "{}（{}，实例 java.path）",
        p.display(),
        match major {
            Some(m) => format!("Java {m}"),
            None => "版本未知".to_owned(),
        }
    );
    Ok(Choice { java: p, major, note, warning })
}

/// 在候选里挑一个。返回下标与（可选的）警告。
///
/// 三条策略，每条都对应一种真会发生的部署：
///
/// 1. **有精确匹配就用它**，同版本里按来源优先级（`runtime` > `JAVA_HOME` > …）。
/// 2. **没有精确匹配时向上找最小的那个** —— 1.17 的 json 写 16、机器上只有 17，
///    这是最正常的漂移，`--release 16` 的字节码在 17 上跑得起来。
/// 3. **全部低于要求时拒绝**。反过来（用旧的 Java 跑新的字节码）是必然失败，
///    没有"试一试"的余地。
pub(super) fn pick(
    cands: &[Candidate],
    required: Option<u32>,
    dest: &Path,
) -> Result<(usize, Option<String>), String> {
    if cands.is_empty() {
        return Err("没有候选 JRE".to_owned());
    }

    let Some(req) = required else {
        // json 没写 `javaVersion`（1.12 及以前都没有）。这时候按老惯例优先 Java 8：
        // 那批客户端全是 Java 8 时代的东西，机器上同时装了 17 时选它必炸。
        if let Some(i) = first_by_rank(cands, |c| c.major == Some(8)) {
            return Ok((i, None));
        }
        if let Some(i) = lowest_known(cands) {
            let m = cands[i].major.unwrap_or(0);
            return Ok((
                i,
                Some(format!("版本 json 没写 javaVersion，按最低可用版本选了 Java {m}")),
            ));
        }
        return Ok((0, None));
    };

    if let Some(i) = first_by_rank(cands, |c| c.major == Some(req)) {
        return Ok((i, None));
    }

    let mut above: Vec<usize> = (0..cands.len())
        .filter(|&i| cands[i].major.map(|m| m > req).unwrap_or(false))
        .collect();
    above.sort_by_key(|&i| (cands[i].major.unwrap_or(u32::MAX), cands[i].origin.rank()));
    if let Some(&i) = above.first() {
        let got = cands[i].major.unwrap_or(0);
        return Ok((
            i,
            Some(format!(
                "要求 Java {req}，这台机器上没有正好匹配的版本，退用 Java {got}（来自 {}）；\
                 起不来时把 Java {req} 放进 {} 或写 java.path",
                cands[i].source,
                dest.display()
            )),
        ));
    }

    // 走到这里只剩两种情况：全是更旧的版本，或者版本号一个都读不出来。
    if let Some(i) = cands.iter().position(|c| c.major.is_none()) {
        return Ok((
            i,
            Some(format!(
                "要求 Java {req}，但 {} 的版本号读不出来（没有 release 文件，`java -version` 也没输出）\
                 —— 先按它启动，起不来就是版本不对",
                cands[i].java.display()
            )),
        ));
    }

    let mut found: Vec<String> = cands
        .iter()
        .map(|c| format!("Java {}", c.major.map(|m| m.to_string()).unwrap_or_else(|| "?".to_owned())))
        .collect();
    found.sort();
    found.dedup();
    Err(format!(
        "这个版本要求 Java {req}，而扫到的 JRE 全部更旧（{}）。用旧 JVM 跑新字节码必然失败\
         （UnsupportedClassVersionError），所以不启动。把 Java {req} 放进 {} 下，或在实例里写 java.path",
        found.join("、"),
        dest.display()
    ))
}

/// 满足条件的候选里按来源优先级取第一个；同优先级按路径排序，保证同一台机器
/// 上多次调用结果一致（`read_dir` 的顺序在 Windows 上不保证稳定）。
fn first_by_rank(cands: &[Candidate], ok: impl Fn(&Candidate) -> bool) -> Option<usize> {
    let mut idx: Vec<usize> = (0..cands.len()).filter(|&i| ok(&cands[i])).collect();
    idx.sort_by_key(|&i| (cands[i].origin.rank(), cands[i].home.to_string_lossy().to_lowercase()));
    idx.first().copied()
}

fn lowest_known(cands: &[Candidate]) -> Option<usize> {
    let mut idx: Vec<usize> = (0..cands.len()).filter(|&i| cands[i].major.is_some()).collect();
    idx.sort_by_key(|&i| (cands[i].major.unwrap_or(0), cands[i].origin.rank()));
    idx.first().copied()
}

// ------------------------------------------------------------------ 探测

/// 扫一遍这台机器，去重后按来源排好。
pub fn discover(paths: &Paths, client: &Path) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    // 1) 随部署走的运行时。这是唯一一个"跟着包走"的位置，所以排在第一。
    let vendored = paths.runtime().join("jre");
    for home in walk_homes(&vendored, 3) {
        push(&mut out, &mut seen, home, Origin::Vendored, format!("{}", vendored.display()));
    }

    // 2) JAVA_HOME。人已经明确表达了偏好，仅次于随包的那份。
    if let Ok(v) = std::env::var("JAVA_HOME") {
        let home = PathBuf::from(v.trim());
        if !home.as_os_str().is_empty() {
            push(&mut out, &mut seen, home, Origin::JavaHome, "JAVA_HOME".to_owned());
        }
    }

    // 3) 系统安装目录。厂商的目录名各不相同，但都是"根下的一级子目录是一个 JRE"。
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        let Ok(base) = std::env::var(var) else { continue };
        for vendor in VENDOR_ROOTS {
            let root = Path::new(&base).join(vendor);
            for home in walk_homes(&root, 2) {
                push(&mut out, &mut seen, home, Origin::Installed, root.display().to_string());
            }
        }
    }

    // 4) 客户端自带的 runtime（官方启动器下载的那种，层级是
    //    `runtime\<component>\<platform>\<component>\bin`）。
    let in_client = client.join("runtime");
    for home in walk_homes(&in_client, 4) {
        push(&mut out, &mut seen, home, Origin::ClientRuntime, in_client.display().to_string());
    }

    // 5) 历史遗留。最后才看 —— 它们存在的原因只是"以前就是这么配的"。
    for p in LEGACY_HOMES {
        push(&mut out, &mut seen, PathBuf::from(p), Origin::Legacy, (*p).to_owned());
    }

    out
}

fn searched_roots(paths: &Paths, client: &Path) -> Vec<String> {
    let mut v = vec![paths.runtime().join("jre").display().to_string()];
    v.push("JAVA_HOME".to_owned());
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(base) = std::env::var(var) {
            for vendor in VENDOR_ROOTS {
                v.push(Path::new(&base).join(vendor).display().to_string());
            }
        }
    }
    v.push(client.join("runtime").display().to_string());
    v.extend(LEGACY_HOMES.iter().map(|s| (*s).to_owned()));
    v
}

fn push(
    out: &mut Vec<Candidate>,
    seen: &mut BTreeSet<String>,
    home: PathBuf,
    origin: Origin,
    source: String,
) {
    let Some(java) = javaw_in(&home) else { return };
    // 去重按 home 而不是 exe：同一个 JRE 从 JAVA_HOME 和 Program Files 两条路都会
    // 被发现，而它们确实是同一份 —— 不去重会让"扫到 3 个 JRE"变成"I 有 6 个"。
    let key = home.to_string_lossy().to_lowercase();
    if !seen.insert(key) {
        return;
    }
    let major = probe_major(&home);
    out.push(Candidate { java, home, major, origin, source });
}

/// 在 `root` 下找 JRE 根（含 `bin\javaw.exe` 的那一层），最多往下 `depth` 层。
///
/// 为什么不是"读一级子目录"：厂商布局各不相同（`Java\jdk-17`、`Eclipse
/// Adoptium\jdk-17.0.9.9-hotspot`、Mojang 的 `runtime\<c>\<plat>\<c>`），写死
/// 层数早晚要改。找到就停 —— 一个 JRE 根里面不会再套一个 JRE 根。
fn walk_homes(root: &Path, depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_into(&mut out, root, depth);
    out
}

fn walk_into(out: &mut Vec<PathBuf>, dir: &Path, depth: usize) {
    if depth == 0 || !dir.is_dir() {
        return;
    }
    if javaw_in(dir).is_some() {
        out.push(dir.to_path_buf());
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    // 排序让结果与 `read_dir` 的返回顺序无关 —— 同一个目录两次扫描给出两种顺序，
    // 会变成"重启一次换了 java"这种无法复现的怪事。
    let mut kids: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    kids.sort();
    for kid in kids {
        walk_into(out, &kid, depth - 1);
    }
}

fn javaw_in(home: &Path) -> Option<PathBuf> {
    let bin = home.join("bin");
    let w = bin.join("javaw.exe");
    if w.is_file() {
        return Some(w);
    }
    let j = bin.join("java.exe");
    if j.is_file() {
        return Some(j);
    }
    None
}

/// exe 路径 → JRE 根（`…\bin\javaw.exe` → `…`）。
fn home_of(exe: &Path) -> Option<PathBuf> {
    exe.parent().and_then(|bin| bin.parent()).map(|p| p.to_path_buf())
}

/// 主版本号：先读 `release`，读不到再起一次 `java -version`。
pub fn probe_major(home: &Path) -> Option<u32> {
    read_release_major(home).or_else(|| spawn_major(home))
}

fn read_release_major(home: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(home.join("release")).ok()?;
    release_major(&text)
}

/// 从 `release` 文件里取主版本号。
///
/// 文件是 `KEY="value"` 一行一条，版本那一行长这样：
/// `JAVA_VERSION="1.8.0_504"` / `JAVA_VERSION="17.0.9"` / `JAVA_VERSION="21"`。
pub fn release_major(text: &str) -> Option<u32> {
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("JAVA_VERSION=") else { continue };
        return major_of(rest.trim().trim_matches('"'));
    }
    None
}

/// `1.8.0_504` → 8，`17.0.9+9` → 17，`21` → 21。
///
/// 只认这两种形状：Java 5 之前没有 `release` 文件，而那些版本本来也跑不了 MC。
pub fn major_of(v: &str) -> Option<u32> {
    let v = v.trim().trim_matches('"');
    if v.is_empty() {
        return None;
    }
    let mut it = v.split('.');
    let first: u32 = it.next()?.parse().ok()?;
    if first == 1 {
        it.next()?.parse().ok()
    } else {
        Some(first)
    }
}

/// `java -version` 的 fallback。
///
/// 只在 `release` 缺失时走这条路（被手工裁剪过的 JRE、从别的程序里抠出来的
/// `bin` 目录）。`CREATE_NO_WINDOW` 是必需的：这个进程是 GUI 子系统，带出来的
/// 控制台子进程会在玩家屏幕上闪一下黑框。
fn spawn_major(home: &Path) -> Option<u32> {
    let exe = home.join("bin").join("java.exe");
    if !exe.is_file() {
        return None;
    }
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("-version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    let mut text = String::from_utf8_lossy(&out.stderr).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stdout));
    version_line_major(&text)
}

/// 从 `java -version` 的输出里取主版本号。
pub fn version_line_major(text: &str) -> Option<u32> {
    for line in text.lines() {
        let Some(pos) = line.find("version \"") else { continue };
        let rest = &line[pos + "version \"".len()..];
        let end = rest.find('"')?;
        return major_of(&rest[..end]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEST: &str = r"C://games//runtime//jre";

    fn cand(home: &str, major: Option<u32>, origin: Origin) -> Candidate {
        Candidate {
            java: PathBuf::from(format!(r"{home}\bin\javaw.exe")),
            home: PathBuf::from(home),
            major,
            origin,
            source: home.to_owned(),
        }
    }

    #[test]
    fn release_version_parsing() {
        assert_eq!(release_major("JAVA_VERSION=\"1.8.0_504\"\nOS_NAME=\"Windows\"\n"), Some(8));
        assert_eq!(release_major("JAVA_VERSION=\"17.0.9\"\n"), Some(17));
        assert_eq!(release_major("JAVA_VERSION=\"21\"\n"), Some(21));
        assert_eq!(release_major("JAVA_VERSION=\"21.0.1+12-LTS\"\n"), Some(21));
        assert_eq!(release_major("OS_NAME=\"Windows\"\n"), None, "没有那一行就是没有");
        assert_eq!(release_major(""), None);
    }

    #[test]
    fn version_line_parsing_survives_vendor_noise() {
        let openjdk = "openjdk version \"17.0.9\" 2023-10-17\nOpenJDK Runtime Environment\n";
        assert_eq!(version_line_major(openjdk), Some(17));
        let legacy = "java version \"1.8.0_504\"\nJava(TM) SE Runtime Environment\n";
        assert_eq!(version_line_major(legacy), Some(8));
        assert_eq!(version_line_major("Error: could not open `x'\n"), None);
    }

    #[test]
    fn major_of_rejects_junk() {
        assert_eq!(major_of("1.8.0_504"), Some(8));
        assert_eq!(major_of("17"), Some(17));
        assert_eq!(major_of(""), None);
        assert_eq!(major_of("abc"), None);
        // 只有 "1" 的时候没有次版本号可取。
        assert_eq!(major_of("1"), None);
    }

    #[test]
    fn exact_match_beats_higher_version() {
        // 要求 8 时不能因为机器上有 17 就拿 17 —— 1.12.2 Forge 在 17 上必然失败。
        let c = vec![
            cand(r"C:\jdk17", Some(17), Origin::Installed),
            cand(r"C:\jre8", Some(8), Origin::Legacy),
        ];
        let (i, warn) = pick(&c, Some(8), Path::new(DEST)).expect("应当有精确匹配");
        assert_eq!(i, 1);
        assert!(warn.is_none(), "精确匹配不该有警告");
    }

    #[test]
    fn vendored_java_wins_over_system() {
        let c = vec![
            cand(r"C:\Program Files\Java\jdk8", Some(8), Origin::Installed),
            cand(r"C:\games\runtime\jre\8", Some(8), Origin::Vendored),
        ];
        let (i, _) = pick(&c, Some(8), Path::new(DEST)).expect("两个都是 8");
        assert_eq!(i, 1, "随部署走的那份优先");
    }

    #[test]
    fn missing_exact_version_steps_up_with_a_warning() {
        // 1.17 的 json 写 16，机器上只有 17：向上取最小的那个，并留下警告。
        let c = vec![
            cand(r"C:\jdk21", Some(21), Origin::Installed),
            cand(r"C:\jdk17", Some(17), Origin::Installed),
            cand(r"C:\jre8", Some(8), Origin::Legacy),
        ];
        let (i, warn) = pick(&c, Some(16), Path::new(DEST)).expect("17 能跑 16 的字节码");
        assert_eq!(i, 1, "取最小的 ≥16，而不是 21");
        let warn = warn.expect("版本漂移必须留警告");
        assert!(warn.contains("Java 16") && warn.contains("Java 17"), "{warn}");
    }

    #[test]
    fn older_only_is_a_refusal_not_a_try() {
        // 要求 17 而只有 8：这是必然失败，不能"先试试"。
        let c = vec![cand(r"C:\jre8", Some(8), Origin::Legacy)];
        let err = pick(&c, Some(17), Path::new(DEST)).expect_err("必须拒绝");
        assert!(err.contains("Java 17"), "{err}");
        assert!(err.contains("UnsupportedClassVersionError"), "{err}");
        assert!(err.contains("Java 8"), "要说清扫到了什么：{err}");
    }

    #[test]
    fn no_requirement_prefers_java_8() {
        // 1.12.2 的 json 没有 javaVersion，机器上同时有 8 和 17 时选 17 必炸。
        let c = vec![
            cand(r"C:\jdk17", Some(17), Origin::Installed),
            cand(r"C:\jre8", Some(8), Origin::Legacy),
        ];
        let (i, warn) = pick(&c, None, Path::new(DEST)).expect("两个都能用");
        assert_eq!(i, 1);
        assert!(warn.is_none());
    }

    #[test]
    fn no_requirement_without_java_8_falls_back_to_lowest() {
        let c = vec![
            cand(r"C:\jdk21", Some(21), Origin::Installed),
            cand(r"C:\jdk17", Some(17), Origin::JavaHome),
        ];
        let (i, warn) = pick(&c, None, Path::new(DEST)).expect("总得选一个");
        assert_eq!(i, 1, "最低的已知版本");
        assert!(warn.is_some(), "没写 javaVersion 这件事要说出来");
    }

    #[test]
    fn unknown_version_is_used_but_flagged() {
        let c = vec![cand(r"C:\broken", None, Origin::Installed)];
        let (i, warn) = pick(&c, Some(17), Path::new(DEST)).expect("版本未知也先试");
        assert_eq!(i, 0);
        assert!(warn.expect("要警告").contains("读不出来"));
    }

    #[test]
    fn home_of_walks_up_from_bin() {
        assert_eq!(
            home_of(Path::new(r"C:\jdk17\bin\javaw.exe")),
            Some(PathBuf::from(r"C:\jdk17"))
        );
        assert_eq!(home_of(Path::new("javaw.exe")), None);
    }
}
