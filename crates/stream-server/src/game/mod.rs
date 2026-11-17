//! 游戏托管：把"桌面上一台机器在跑什么游戏"变成这个进程能管的对象。
//!
//! ## 为什么归 stream-server 管，而不是再写一个启动器
//!
//! 过去这条链是 `ssh → schtasks /IT 投递 → powershell → wscript → javaw`。五层里
//! 有四层是在补偿同一件事：**Windows 会给 console 子系统的子进程配一个控制台，
//! 而那个控制台的关闭会连带杀死游戏**（`Ctrl-Close`）。最后靠 `wscript`（GUI
//! 宿主，没有控制台）绕过去。
//!
//! stream-server 自己就是 GUI 子系统（见 `build.rs` 与 `main.rs` 的 stdio 兜底），
//! 它 spawn 出来的 `javaw` **没有控制台祖先**，那条绕法连同它的全部理由一起消失。
//! 顺带拿回三样投递方式永远拿不到的东西：pid、退出码、活的子进程句柄。
//!
//! 另一半理由是这个进程"永远在跑、永远在听端口"：游戏崩了它能看见，游戏退出了
//! 它能上报，运维从网页上点一下就能起停 —— 而不是先 ssh 进去。
//!
//! ## 目录约定
//!
//! ```text
//! <game_root>\                    默认：exe 旁边的 games\
//!   clients\1.12.2-forge\         只读：versions / libraries / assets / mods / config
//!   saves\default\                可写：--gameDir 指向这里（存档、options.txt、logs）
//!   instances\1.12.2-forge.json   实例描述，见 [`InstanceSpec`]
//!   runtime\                      每次启动的 cmdline / 启动日志 / hs_err
//! ```
//!
//! 客户端与存档分开，是这套结构唯一的硬决定：客户端是"可替换的只读资产"（换版本
//! = 换一个 clients 子目录），存档是"绝不能动的用户数据"（换世界 = 换一个 saves
//! 子目录）。混在一起时，一次"恢复出厂"就会连世界一起删掉。
//!
//! ## 为什么没有 `borderless`
//!
//! `display.model` 只有 `exclusive` 与 `windowed` 两个值。无边框窗口那套
//! （`SetWindowLong(WS_POPUP)` 改样式）的前提是"独占全屏采不到画面"，而实测
//! 2026-11-17：MC 1.12.2 按 F11 切全屏后，DXGI 桌面复制仍然能出画面。既然
//! 真全屏可用，再加一层改样式的 hack 只会引入"画面正常但鼠标偏移"这类新故障。
//! 将来若换到采不到独占全屏的后端，这里再加第三个值，语义已经留好了。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;

pub mod host;

pub use host::{GameHost, Status};

/// 当前 Unix 秒。
///
/// 实例、锁定、退出记录三处都要一个时间戳，而它们都需要"算不出来时返回 0 而不是
/// 崩"—— 系统时钟被拨到 1970 之前这种事不会发生，但 `duration_since` 是会返回
/// `Err` 的，`unwrap` 在这里没有任何好处。
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}


/// `games` 根目录下的四个固定子目录。
pub const DIR_CLIENTS: &str = "clients";
pub const DIR_SAVES: &str = "saves";
pub const DIR_INSTANCES: &str = "instances";
pub const DIR_RUNTIME: &str = "runtime";

/// 实例文件的当前版本。读到更高版本的实例时**不拒绝**，只是把未知字段留着
/// （见 [`InstanceSpec::unknown`]）—— 一份"新版本写的文件"在旧版本上照样能跑，
/// 是回滚能被接受的前提。
pub const SCHEMA: u32 = 1;

/// 目录根的解析结果。
///
/// 只装一个 `root`，四个子目录由方法拼 —— 少一处拼错路径的机会，也让测试可以
/// 拿一个临时目录构造。
#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    /// 从配置解析。
    ///
    /// 三种情况，顺序就是优先级：
    /// 1. `game_root` 是绝对路径 → 原样用（"我就要放在 D:\GameData"）；
    /// 2. `game_root` 是相对路径 → 相对 **exe 所在目录**（不是 cwd，见字段注释）；
    /// 3. 没配 → `<exe 目录>\games`。
    pub fn resolve(cfg: &Config) -> Result<Self, String> {
        let base = crate::run::root().ok_or_else(|| {
            "无法确定 exe 所在目录（std::env::current_exe 失败），游戏目录无从推导".to_owned()
        })?;
        let root = match cfg.game_root.as_deref() {
            Some(p) if p.is_absolute() => p.to_path_buf(),
            Some(p) => base.join(p),
            None => base.join("games"),
        };
        Ok(Self { root })
    }

    pub fn clients(&self) -> PathBuf {
        self.root.join(DIR_CLIENTS)
    }
    pub fn saves(&self) -> PathBuf {
        self.root.join(DIR_SAVES)
    }
    pub fn instances(&self) -> PathBuf {
        self.root.join(DIR_INSTANCES)
    }
    pub fn runtime(&self) -> PathBuf {
        self.root.join(DIR_RUNTIME)
    }

    /// 实例文件路径。id 里带路径分隔符或 `..` 一律拒绝：这个值来自网页，
    /// 拼进路径之前必须当成不可信输入。
    pub fn instance_file(&self, id: &str) -> Result<PathBuf, String> {
        if id.is_empty()
            || id.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
            || id == "."
            || id == ".."
        {
            return Err(format!("实例 id {id:?} 含非法字符"));
        }
        Ok(self.instances().join(format!("{id}.json")))
    }
}

// ---------------------------------------------------------------- 实例描述

/// 一份 `instances\<id>.json`。
///
/// 键名用 snake_case，与 `config.toml` 同一套习惯 —— 运维只需要记一种写法。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InstanceSpec {
    /// 文件格式版本。当前 [`SCHEMA`]。
    #[serde(default = "d_schema")]
    pub schema: u32,
    /// 人类可读的名字，出现在面板与日志里。
    #[serde(default)]
    pub name: String,
    /// 客户端目录：`versions / libraries / assets` 所在的那一层。
    /// 相对路径相对 `games` 根，绝对路径原样用。
    pub client_dir: String,
    /// 存档目录：`--gameDir` 指向这里。
    pub saves_dir: String,
    #[serde(default)]
    pub java: JavaSpec,
    #[serde(default)]
    pub game: GameSpec,
    #[serde(default)]
    pub display: DisplaySpec,
    #[serde(default)]
    pub lifecycle: LifecycleSpec,
    /// 本版本不认识的键，原样留着、原样写回。
    ///
    /// 这是"预留冗余"的落点：新版本加一个字段、旧版本读同一份文件，写回时
    /// 不能把不认识的那一行吃掉。`deny_unknown_fields` 在这里是错的 ——
    /// 配置文件的拼写错误该报，实例文件的**未来字段**不该。
    #[serde(flatten, default)]
    pub unknown: BTreeMap<String, serde_json::Value>,
}

fn d_schema() -> u32 {
    SCHEMA
}
fn d_heap_mb() -> u32 {
    2048
}
fn d_main_class() -> String {
    "net.minecraft.launchwrapper.Launch".to_owned()
}
fn d_player() -> String {
    "CloudPlayer".to_owned()
}
fn d_tweak() -> Option<String> {
    None
}
fn d_stop_timeout() -> u64 {
    10
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JavaSpec {
    /// `javaw.exe` 路径（相对 `games` 根或绝对）。为空则按常见位置探测。
    #[serde(default)]
    pub path: String,
    #[serde(default = "d_heap_mb")]
    pub heap_mb: u32,
    /// 追加的 JVM 参数，排在默认参数之后。
    #[serde(default)]
    pub jvm_args: Vec<String>,
}

impl Default for JavaSpec {
    fn default() -> Self {
        Self { path: String::new(), heap_mb: d_heap_mb(), jvm_args: Vec::new() }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GameSpec {
    /// 版本目录名，例如 `1.12.2-Forge_14.23.5.2864`。必须与 `versions\<name>\`
    /// 同名 —— 因为 classpath 是从那一层的 json 推导出来的。
    pub version: String,
    #[serde(default = "d_main_class")]
    pub main_class: String,
    /// Forge 的 tweaker。为空表示原版（不带 mod 加载）。
    #[serde(default = "d_tweak")]
    pub tweak_class: Option<String>,
    #[serde(default = "d_player")]
    pub player: String,
    /// 追加的游戏参数，排在最后（`--demo`、`--width` 覆盖之类都可以放）。
    #[serde(default)]
    pub game_args: Vec<String>,
}

impl Default for GameSpec {
    fn default() -> Self {
        Self {
            version: String::new(),
            main_class: d_main_class(),
            tweak_class: d_tweak(),
            player: d_player(),
            game_args: Vec::new(),
        }
    }
}

/// 窗口形态。见模块文档"为什么没有 borderless"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DisplayModel {
    /// `options.txt: fullscreen:true` —— MC 自己的全屏（与按 F11 同一条路径）。
    Exclusive,
    /// 普通窗口，尺寸由 `width/height` 决定。
    Windowed,
}

impl Default for DisplayModel {
    fn default() -> Self {
        // 默认全屏：这台机器的用途就是"一个人隔着浏览器玩"，窗口模式会让
        // 桌面边框、任务栏一起进画面。
        DisplayModel::Exclusive
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DisplaySpec {
    #[serde(default)]
    pub model: DisplayModel,
    /// 0 = 跟随串流分辨率（`config.width/height`）。
    ///
    /// 为什么允许"跟随"而不是写死：采集是 `width×height`，游戏是另一对数时，
    /// 编码前要多一次缩放，等于白糊一层。让两边跟同一个来源走，就不会出现
    /// "改了串流分辨率、游戏还是老的"这种只在画质上体现的错。
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// 窗口模式下左上角位置；全屏模式忽略。
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
}

impl Default for DisplaySpec {
    fn default() -> Self {
        Self { model: DisplayModel::default(), width: 0, height: 0, x: 0, y: 0 }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LifecycleSpec {
    /// 点击停止后等它自己退出的秒数（存档、写回 options.txt 都要时间）。
    #[serde(default = "d_stop_timeout")]
    pub stop_timeout_sec: u64,
    /// 崩溃后自动重启的次数上限。0 = 不自动重启（默认）。
    #[serde(default)]
    pub restart_max: u32,
    /// 两次重启之间等多久。
    #[serde(default = "d_backoff")]
    pub restart_backoff_sec: u64,
}

fn d_backoff() -> u64 {
    5
}

impl Default for LifecycleSpec {
    fn default() -> Self {
        Self { stop_timeout_sec: d_stop_timeout(), restart_max: 0, restart_backoff_sec: d_backoff() }
    }
}

impl InstanceSpec {
    /// 读一份实例文件。解析失败时的错误带上路径，否则"哪个文件写错了"要靠猜。
    pub fn load(paths: &Paths, id: &str) -> Result<Self, String> {
        let path = paths.instance_file(id)?;
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("读不到实例文件 {}：{e}", path.display()))?;
        let spec: Self = serde_json::from_str(&raw)
            .map_err(|e| format!("实例文件 {} 不是合法 JSON：{e}", path.display()))?;
        spec.validate()?;
        Ok(spec)
    }

    /// 校验"起不起来"的硬条件，早说清楚比启动后一行 java 报错强。
    pub fn validate(&self) -> Result<(), String> {
        if self.game.version.trim().is_empty() {
            return Err("game.version 不能为空（要对应 versions\\<name>\\ 那一层）".to_owned());
        }
        if self.client_dir.trim().is_empty() {
            return Err("client_dir 不能为空".to_owned());
        }
        if self.saves_dir.trim().is_empty() {
            return Err("saves_dir 不能为空".to_owned());
        }
        Ok(())
    }

    /// 相对路径相对 `games` 根，绝对路径原样。
    fn under_root(paths: &Paths, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            paths.root.join(path)
        }
    }

    pub fn client_path(&self, paths: &Paths) -> PathBuf {
        Self::under_root(paths, &self.client_dir)
    }

    pub fn saves_path(&self, paths: &Paths) -> PathBuf {
        Self::under_root(paths, &self.saves_dir)
    }

    /// `javaw.exe` 的位置：显式配了就用，否则在几个常见位置里找。
    ///
    /// 探测顺序和 `toolchain\start-mc.ps1` 一致 —— 同一台机器上两条路径（脚本与
    /// 服务）必须找到同一个 java，否则会出现"脚本能起、服务起不来"。
    pub fn java_path(&self, paths: &Paths) -> Result<PathBuf, String> {
        if !self.java.path.trim().is_empty() {
            let p = Self::under_root(paths, &self.java.path);
            if p.is_file() {
                return Ok(p);
            }
            return Err(format!("java.path 指向的文件不存在：{}", p.display()));
        }
        let mut cands: Vec<PathBuf> = Vec::new();
        if let Ok(p) = std::env::var("JAVA_HOME") {
            cands.push(Path::new(&p).join("bin").join("javaw.exe"));
        }
        cands.push(PathBuf::from(r"C:\mc\jre8\bin\javaw.exe"));
        cands.push(PathBuf::from(r"C:\dev\jdk8\jdk8u504-b01\bin\javaw.exe"));
        for c in &cands {
            if c.is_file() {
                return Ok(c.clone());
            }
        }
        Err(format!(
            "找不到 javaw.exe；试过 {}。请在实例里写 java.path",
            cands.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("、")
        ))
    }

    /// 目标窗口尺寸：0 = 跟随串流分辨率。
    pub fn target_size(&self, cfg: &Config) -> (u32, u32) {
        let w = if self.display.width == 0 { cfg.width } else { self.display.width };
        let h = if self.display.height == 0 { cfg.height } else { self.display.height };
        (w.max(320), h.max(240))
    }
}

// ---------------------------------------------------------------- 启动计划

/// 一次启动需要的全部输入，已经展开成可直接交给 `Command` 的形态。
///
/// 单独一层是为了"打印"和"执行"走同一份数据：`--print-game` 打出来的命令行
/// 必须是真启动时的那一条，否则这个开关就成了摆设。
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub java: PathBuf,
    pub cwd: PathBuf,
    /// 逐个参数，不拼成一整串。
    ///
    /// `toolchain\start-mc.ps1` 里必须手拼字符串（PowerShell 的 `Start-Process
    /// -ArgumentList` 传数组会按空格拆断含空格的路径）。Rust 的 `Command` 自己
    /// 负责引号，所以这里保留参数数组这一层 —— 拼好的字符串只用于显示。
    pub args: Vec<String>,
    /// 目标窗口尺寸（写 options.txt 用）。
    pub size: (u32, u32),
    pub model: DisplayModel,
    /// classpath 里缺的 jar（相对 libraries 的路径）。非空不算失败：natives-only
    /// 的两个包本来就不在磁盘上，MC 也不需要它们进 classpath。
    pub missing: Vec<String>,
}

impl LaunchPlan {
    /// 给人看的命令行（`--print-game` 与日志）。含空格的参数加双引号。
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&quote(&self.java.to_string_lossy()));
        for a in &self.args {
            out.push(' ');
            out.push_str(&quote(a));
        }
        out
    }
}

fn quote(s: &str) -> String {
    if s.contains(' ') || s.contains('\t') {
        format!("\"{s}\"")
    } else {
        s.to_owned()
    }
}

/// 从 `versions\<version>\<version>.json` 拼 classpath。
///
/// 三条从脚本时代继承下来的规则，一条都不能省：
/// 1. 路径取 `downloads.artifact.path`，它是**相对 libraries 根**的（不是相对
///    客户端根）—— 首版少 55 个 jar 就是这么来的；
/// 2. 没有该字段时按 `group:artifact:version` 推导；
/// 3. `rules` 有匹配项时看最后一条 `action`，没有匹配项就不进 classpath。
fn build_classpath(client: &Path, version: &str) -> Result<(Vec<PathBuf>, Vec<String>), String> {
    let vdir = client.join("versions").join(version);
    let vjson = vdir.join(format!("{version}.json"));
    let raw = std::fs::read_to_string(&vjson)
        .map_err(|e| format!("读不到版本 json {}：{e}", vjson.display()))?;
    let json: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("版本 json {} 解析失败：{e}", vjson.display()))?;

    let lib_root = client.join("libraries");
    let mut cp: Vec<PathBuf> = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    if let Some(libs) = json.get("libraries").and_then(|v| v.as_array()) {
        for lib in libs {
            // rules：空 = 进；有则最后一条匹配的说了算。1.12.2 只用得到 os.name。
            let mut keep = true;
            if let Some(rules) = lib.get("rules").and_then(|v| v.as_array()) {
                keep = false;
                for r in rules {
                    let mut hit = true;
                    if let Some(os) = r.get("os") {
                        if let Some(name) = os.get("name").and_then(|v| v.as_str()) {
                            if name != "windows" {
                                hit = false;
                            }
                        }
                        if let Some(arch) = os.get("arch").and_then(|v| v.as_str()) {
                            if arch != "x86_64" {
                                hit = false;
                            }
                        }
                    }
                    if hit {
                        keep = r.get("action").and_then(|v| v.as_str()) == Some("allow");
                    }
                }
            }
            if !keep {
                continue;
            }

            let rel = lib
                .pointer("/downloads/artifact/path")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned())
                .or_else(|| lib.get("name").and_then(|v| v.as_str()).and_then(name_to_path));
            let Some(rel) = rel else { continue };

            let full = lib_root.join(rel.replace('/', r"\"));
            if full.is_file() {
                cp.push(full);
            } else {
                missing.push(rel);
            }
        }
    }

    let client_jar = vdir.join(format!("{version}.jar"));
    if client_jar.is_file() {
        cp.push(client_jar);
    } else {
        missing.push(format!("{version}.jar"));
    }

    Ok((cp, missing))
}

/// `group:artifact:version` → `group\artifact\version\artifact-version.jar`。
fn name_to_path(name: &str) -> Option<String> {
    let mut parts = name.split(':');
    let g = parts.next()?;
    let a = parts.next()?;
    let v = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some(format!(
        "{}\\{}\\{}\\{}-{}.jar",
        g.replace('.', "\\"),
        a,
        v,
        a,
        v
    ))
}

/// natives 目录：优先 `natives-tmp`，退回版本目录（与脚本一致）。
fn natives_dir(client: &Path, version: &str) -> PathBuf {
    let tmp = client.join("natives-tmp");
    if tmp.is_dir() {
        tmp
    } else {
        client.join("versions").join(version)
    }
}

impl InstanceSpec {
    /// 组装启动计划（不落盘、不起进程，纯计算）。
    pub fn plan(&self, cfg: &Config, paths: &Paths) -> Result<LaunchPlan, String> {
        self.validate()?;
        let client = self.client_path(paths);
        let saves = self.saves_path(paths);
        let java = self.java_path(paths)?;
        let size = self.target_size(cfg);
        let version = &self.game.version;

        let (cp, missing) = build_classpath(&client, version)?;
        if cp.is_empty() {
            return Err(format!(
                "classpath 是空的（客户端 {} 下没有 versions\\{}\\）",
                client.display(),
                version
            ));
        }
        let vjson = client.join("versions").join(version).join(format!("{version}.json"));
        let raw = std::fs::read_to_string(&vjson)
            .map_err(|e| format!("读不到版本 json {}：{e}", vjson.display()))?;
        let json: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| format!("版本 json {} 解析失败：{e}", vjson.display()))?;
        let asset_index = json
            .pointer("/assetIndex/id")
            .and_then(|v| v.as_str())
            .unwrap_or("1.12")
            .to_owned();
        let version_type = json
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("release")
            .to_owned();

        let cp_joined = cp
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(";");

        let mut args: Vec<String> = vec![
            format!("-Xmx{}M", self.java.heap_mb),
            "-Xms512M".to_owned(),
            "-XX:+UseG1GC".to_owned(),
            format!("-Djava.library.path={}", natives_dir(&client, version).display()),
            "-Dfml.ignoreInvalidMinecraftCertificates=true".to_owned(),
            "-Dfml.ignorePatchDiscrepancies=true".to_owned(),
            // JVM 自己的崩溃日志也进 runtime，与游戏的 crash-report 放在一起，
            // 否则一个在 %TEMP%、一个在存档里，没人能同时找到。
            format!("-XX:ErrorFile={}", paths.runtime().join("hs_err_%p.log").display()),
        ];
        args.extend(self.java.jvm_args.iter().cloned());

        args.push("-cp".to_owned());
        args.push(cp_joined);
        args.push(self.game.main_class.clone());
        args.push("--username".to_owned());
        args.push(self.game.player.clone());
        args.push("--version".to_owned());
        args.push(version.clone());
        args.push("--gameDir".to_owned());
        args.push(saves.to_string_lossy().into_owned());
        args.push("--assetsDir".to_owned());
        args.push(client.join("assets").to_string_lossy().into_owned());
        args.push("--assetIndex".to_owned());
        args.push(asset_index);
        args.push("--uuid".to_owned());
        args.push("00000000-0000-0000-0000-000000000000".to_owned());
        args.push("--accessToken".to_owned());
        args.push("0".to_owned());
        args.push("--userType".to_owned());
        args.push("legacy".to_owned());
        args.push("--versionType".to_owned());
        args.push(version_type);
        // 窗口尺寸走 MC 自己的参数（1.12.2 的 Main 认 --width/--height），
        // 全屏则只能靠 options.txt，见 write_options。
        args.push("--width".to_owned());
        args.push(size.0.to_string());
        args.push("--height".to_owned());
        args.push(size.1.to_string());
        if let Some(tweak) = self.game.tweak_class.as_deref() {
            if !tweak.trim().is_empty() {
                args.push("--tweakClass".to_owned());
                args.push(tweak.to_owned());
            }
        }
        args.extend(self.game.game_args.iter().cloned());

        Ok(LaunchPlan { java, cwd: client, args, size, model: self.display.model, missing })
    }

    /// 把窗口形态与尺寸写进 `options.txt`。
    ///
    /// 为什么必须改文件而不是加参数：1.12.2 的 `Main` 没有 `--fullscreen`（那是
    /// 1.13 之后才有的），全屏只从 `GameSettings` 读 `fullscreen:`。
    ///
    /// 两个前提，缺一个这次改就白改：
    /// * 游戏不能在跑 —— MC 只在启动时读这个文件，退出时把**内存里的状态**整份
    ///   写回去。跑着的时候改，退出那一刻就被旧值盖掉，看起来像"改了没用"。
    /// * 按行改。`options.txt` 有一百多行，重排等于清掉键位、音量、资源包。
    pub fn write_options(&self, cfg: &Config, paths: &Paths) -> Result<(), String> {
        let saves = self.saves_path(paths);
        let opt = saves.join("options.txt");
        let size = self.target_size(cfg);
        let full = matches!(self.display.model, DisplayModel::Exclusive);

        if !opt.is_file() {
            // 没有 options.txt = 这台机器上一次都没正常退出过。不替它造一份：
            // 缺文件的 MC 会用全套默认值，凭空写一行进去反而最坏。
            return Err(format!(
                "没有 {}；先正常启动并退出一次游戏，让它自己写出这个文件",
                opt.display()
            ));
        }
        let text = std::fs::read_to_string(&opt)
            .map_err(|e| format!("读 options.txt 失败 {}：{e}", opt.display()))?;

        let mut seen_full = false;
        let mut seen_w = false;
        let mut seen_h = false;
        let mut out: Vec<String> = Vec::new();
        for line in text.lines() {
            let (key, _) = match line.split_once(':') {
                Some(kv) => kv,
                None => {
                    out.push(line.to_owned());
                    continue;
                }
            };
            match key {
                "fullscreen" => {
                    out.push(format!("fullscreen:{full}"));
                    seen_full = true;
                }
                "overrideWidth" => {
                    out.push(format!("overrideWidth:{}", size.0));
                    seen_w = true;
                }
                "overrideHeight" => {
                    out.push(format!("overrideHeight:{}", size.1));
                    seen_h = true;
                }
                _ => out.push(line.to_owned()),
            }
        }
        // 缺行就补在末尾。老版本/首次运行的 options.txt 未必有 override*。
        if !seen_full {
            out.push(format!("fullscreen:{full}"));
        }
        if !seen_w {
            out.push(format!("overrideWidth:{}", size.0));
        }
        if !seen_h {
            out.push(format!("overrideHeight:{}", size.1));
        }

        let mut body = out.join("\r\n");
        body.push_str("\r\n");
        std::fs::write(&opt, body).map_err(|e| format!("写 options.txt 失败 {}：{e}", opt.display()))
    }
}

/// 一个能直接落盘的最小实例文件（用于 `games\instances\` 的首次生成）。
pub fn example_instance(id: &str) -> InstanceSpec {
    InstanceSpec {
        schema: SCHEMA,
        name: id.to_owned(),
        client_dir: format!("{DIR_CLIENTS}\\{id}"),
        saves_dir: format!("{DIR_SAVES}\\default"),
        java: JavaSpec::default(),
        game: GameSpec { version: id.to_owned(), ..GameSpec::default() },
        display: DisplaySpec::default(),
        lifecycle: LifecycleSpec::default(),
        unknown: BTreeMap::new(),
    }
}

/// `--print-game` 的正文：生效实例 + 拼出来的那一条命令行。
///
/// 为什么单独一个人读的格式而不是 JSON：这个输出最常见的用途是"贴到别处比对"
/// —— 与 `toolchain\start-mc.ps1` 拼的那条、与上一次启动 `runtime\cmdline.txt`
/// 里的那条。三行注释 + 一行命令，能一眼看出差在哪。
pub fn report(cfg: &Config) -> Result<String, String> {
    let paths = Paths::resolve(cfg)?;
    let spec = InstanceSpec::load(&paths, &cfg.game_instance)?;
    let plan = spec.plan(cfg, &paths)?;
    let mut out = String::new();
    out.push_str(&format!("# games root : {}\n", paths.root.display()));
    out.push_str(&format!(
        "# instance   : {} ({})\n",
        cfg.game_instance,
        if spec.name.is_empty() { "-" } else { &spec.name }
    ));
    out.push_str(&format!("# client     : {}\n", spec.client_path(&paths).display()));
    out.push_str(&format!("# saves      : {}\n", spec.saves_path(&paths).display()));
    out.push_str(&format!("# java       : {}\n", plan.java.display()));
    out.push_str(&format!(
        "# display    : {}x{} {:?}\n",
        plan.size.0, plan.size.1, plan.model
    ));
    if !plan.missing.is_empty() {
        out.push_str(&format!("# missing    : {:?}\n", plan.missing));
    }
    out.push_str(&plan.render());
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_id_rejects_path_traversal() {
        let paths = Paths { root: PathBuf::from(r"C:\g") };
        for bad in ["", "..", "a/b", r"a\b", "c:x", "../x", "a*b"] {
            assert!(paths.instance_file(bad).is_err(), "{bad:?} 应当被拒绝");
        }
        assert!(paths.instance_file("1.12.2-forge").is_ok());
    }

    #[test]
    fn paths_resolve_relative_against_exe_dir() {
        let mut cfg = Config::default();
        cfg.game_root = None;
        let p = Paths::resolve(&cfg).expect("exe 目录应当可推导");
        assert!(p.root.ends_with("games"), "默认应当是 exe 旁边的 games：{:?}", p.root);

        cfg.game_root = Some(PathBuf::from(r"D:\GameData"));
        let p = Paths::resolve(&cfg).expect("绝对路径应当原样");
        assert_eq!(p.root, PathBuf::from(r"D:\GameData"));
    }

    #[test]
    fn zero_display_size_follows_stream_size() {
        let mut cfg = Config::default();
        cfg.width = 1600;
        cfg.height = 900;
        let mut spec = example_instance("x");
        assert_eq!(spec.target_size(&cfg), (1600, 900));
        spec.display.width = 1280;
        spec.display.height = 720;
        assert_eq!(spec.target_size(&cfg), (1280, 720));
    }

    #[test]
    fn unknown_fields_survive_round_trip() {
        let raw = r#"{
            "client_dir": "clients/a",
            "saves_dir": "saves/a",
            "game": {"version": "1.12.2"},
            "future_knob": {"deep": [1, 2, 3]}
        }"#;
        let spec: InstanceSpec = serde_json::from_str(raw).expect("应当能解析");
        assert!(spec.unknown.contains_key("future_knob"));
        let back = serde_json::to_string(&spec).expect("应当能写回");
        assert!(back.contains("future_knob"), "未知字段不能丢：{back}");
        assert!(back.contains("[1,2,3]"), "未知字段的值要原样保留：{back}");
    }

    #[test]
    fn validate_rejects_empty_essentials() {
        let mut spec = example_instance("x");
        spec.game.version = String::new();
        assert!(spec.validate().is_err());
        spec = example_instance("x");
        spec.client_dir = "  ".to_owned();
        assert!(spec.validate().is_err());
    }

    #[test]
    fn name_to_path_follows_maven_layout() {
        assert_eq!(
            name_to_path("org.ow2.asm:asm-all:5.2").as_deref(),
            Some(r"org\ow2\asm\asm-all\5.2\asm-all-5.2.jar")
        );
        // 四段以上（带 classifier）不是 classpath 成员，交给 artifact.path。
        assert!(name_to_path("a:b:1:natives").is_none());
    }

    #[test]
    fn quote_only_when_needed() {
        assert_eq!(quote("abc"), "abc");
        assert_eq!(quote(r"C:\Program Files\x"), r#""C:\Program Files\x""#);
    }
}
