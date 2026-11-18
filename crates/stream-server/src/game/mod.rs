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
use serde_json::Value;

use crate::config::Config;

pub mod host;
mod java;
mod natives;
mod template;
mod zipmin;

pub use host::{GameHost, Status};

use template::{
    args_from_array, args_from_string, drop_empty_values, duplicate_flags, find_flag_value,
    override_flags, rules_allow, Features, Placeholders,
};

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
fn d_player() -> String {
    "CloudPlayer".to_owned()
}
fn d_stop_timeout() -> u64 {
    10
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JavaSpec {
    /// `javaw.exe` 路径（相对 `games` 根或绝对）。为空则按常见位置探测。
    ///
    /// 非空时**永远优先**：它可能指向一个自编译的、带 agent 的定制运行时，那种
    /// 情况任何"自动挑选"都会选错。此时版本与 json 要求不符只记一条警告，不拦。
    #[serde(default)]
    pub path: String,
    /// 强制要求的主版本号（8 / 17 / 21）。`0` = 跟随版本 json 的
    /// `javaVersion.majorVersion`。
    ///
    /// 存在的理由：1.12 及以前的 json **没有** `javaVersion` 段，而一台机器上
    /// 同时装了 8 和 17 时，"1.12.2 该用哪个"是个真实的判断。默认策略是先找 8
    /// （见 `java::pick`），但世界上总有 `--enable-preview` 之类需要钉死的情况。
    #[serde(default)]
    pub major: u32,
    #[serde(default = "d_heap_mb")]
    pub heap_mb: u32,
    /// 追加的 JVM 参数，排在默认参数之后。
    #[serde(default)]
    pub jvm_args: Vec<String>,
    /// 启动前真的去 classpath 里找一遍主类与 tweaker。
    ///
    /// 默认开：改错一个类名时，`javaw` 只会打一行 `Could not find or load main
    /// class` 然后秒退，跟"哪个字段写错了"没有关联。这里先扫一遍 jar 的中央目录
    /// （不解压），找不到就拒绝启动并说清扫过几个 jar。
    ///
    /// 留了一个关掉的口子，因为校验本身也会出错：jar 是 zip64、包在别的容器里、
    /// 或者类由自定义 ClassLoader 从非 jar 来源加载时，扫描会漏。那时把这里设成
    /// false，回到"java 自己报错"的老路。
    #[serde(default = "d_true")]
    pub verify_classes: bool,
    /// 启动前按版本 json 的 `natives` 段把本地库解到 `runtime\natives\<版本>`。
    ///
    /// 默认开。关掉只影响"用哪一份 dll"：`-Djava.library.path` 会退回旧行为
    /// （客户端目录下的 `natives-tmp` 或版本目录），用于排障时对比。
    #[serde(default = "d_true")]
    pub extract_natives: bool,
}

fn d_true() -> bool {
    true
}

impl Default for JavaSpec {
    fn default() -> Self {
        Self {
            path: String::new(),
            major: 0,
            heap_mb: d_heap_mb(),
            jvm_args: Vec::new(),
            verify_classes: true,
            extract_natives: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GameSpec {
    /// 版本目录名，例如 `1.12.2-Forge_14.23.5.2864`。必须与 `versions\<name>\`
    /// 同名 —— 因为 classpath 是从那一层的 json 推导出来的。
    pub version: String,
    /// 主类覆盖。**留空是常态**：留空时从版本 json 的 `mainClass` 读。
    ///
    /// 以前这里是必填并且默认写死 `net.minecraft.launchwrapper.Launch`，
    /// 于是同一件事在版本 json 和实例文件里各存一份 —— 换一个不是 Forge 的
    /// 客户端（原版、Fabric、Quilt）时，忘了改这里就是一句
    /// `Could not find or load main class`，而 json 里明明写着对的。
    /// 现在只在"确实要偏离 json"时（自定义 jar、包装启动器）才写。
    #[serde(default)]
    pub main_class: Option<String>,
    /// tweaker 覆盖。留空是常态：留空时从版本 json 的参数模板里取
    /// `--tweakClass` 的值（Forge 的 `FMLTweaker`、OptiFine 的 tweaker 都在
    /// 那里，本来就不用抄）。
    #[serde(default)]
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
            main_class: None,
            tweak_class: None,
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

    /// 这个实例要求的 Java 主版本：实例覆盖优先，否则版本 json 的
    /// `javaVersion.majorVersion`（1.13 起才有），都没有则 `None`。
    fn required_java(&self, merged: &Value) -> Option<u32> {
        if self.java.major > 0 {
            return Some(self.java.major);
        }
        merged
            .pointer("/javaVersion/majorVersion")
            .and_then(Value::as_u64)
            .and_then(|n| if n <= u32::MAX as u64 { Some(n as u32) } else { None })
    }

    /// 目标窗口尺寸：0 = 跟随串流分辨率。
    pub fn target_size(&self, cfg: &Config) -> (u32, u32) {
        let w = if self.display.width == 0 { cfg.width } else { self.display.width };
        let h = if self.display.height == 0 { cfg.height } else { self.display.height };
        (w.max(320), h.max(240))
    }
}

// ---------------------------------------------------------------- 启动计划

/// 一个参数是从哪来的。
///
/// 存在的理由只有一个：`--print-game` 要能回答"这个参数是谁写的"。改坏一次启动
/// 之后，对着三四十个 token 猜是哪一层出的错，比改起来还费时间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgOrigin {
    /// 代码里写死的形状或值（`-Xmx`、`-cp`、`--username` 这些键，离线 uuid 之类）。
    Builtin,
    /// 来自版本 json（classpath 的内容、`assetIndex`、`mainClass`、模板里的 tweaker）。
    Json,
    /// 来自实例文件（存档目录、玩家名、追加参数、显式覆盖值）。
    Instance,
}

impl ArgOrigin {
    pub fn label(self) -> &'static str {
        match self {
            ArgOrigin::Builtin => "builtin",
            ArgOrigin::Json => "json",
            ArgOrigin::Instance => "instance",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArgToken {
    pub text: String,
    pub origin: ArgOrigin,
}

impl ArgToken {
    fn builtin(text: impl Into<String>) -> Self {
        Self { text: text.into(), origin: ArgOrigin::Builtin }
    }
    fn json(text: impl Into<String>) -> Self {
        Self { text: text.into(), origin: ArgOrigin::Json }
    }
    fn instance(text: impl Into<String>) -> Self {
        Self { text: text.into(), origin: ArgOrigin::Instance }
    }
}

/// 一次启动需要的全部输入，已经展开成可直接交给 `Command` 的形态。
///
/// 单独一层是为了"打印"和"执行"走同一份数据：`--print-game` 打出来的命令行
/// 必须是真启动时的那一条，否则这个开关就成了摆设。
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub java: PathBuf,
    /// 选中那个 JRE 的主版本（读不出来时 `None`）。`--print-game` 打出来 ——
    /// 「要求 17、用的是 8」这类问题，命令行里看是看不出来的。
    pub java_major: Option<u32>,
    pub cwd: PathBuf,
    /// JVM 段，含 `-cp` 与 classpath。
    ///
    /// `toolchain\start-mc.ps1` 里必须手拼字符串（PowerShell 的 `Start-Process
    /// -ArgumentList` 传数组会按空格拆断含空格的路径）。Rust 的 `Command` 自己
    /// 负责引号，所以这里保留参数数组这一层 —— 拼好的字符串只用于显示。
    pub jvm: Vec<ArgToken>,
    /// 主类。单拎出来不是因为顺序（它就在两段之间），而是因为它是唯一一个
    /// "写错了就必然起不来、而且报错完全指不到源头"的参数。
    pub main_class: ArgToken,
    /// 游戏段。
    pub game: Vec<ArgToken>,
    /// 目标窗口尺寸（写 options.txt 用）。
    pub size: (u32, u32),
    pub model: DisplayModel,
    /// classpath 里缺的 jar（相对 libraries 的路径）。非空不算失败：natives-only
    /// 的两个包本来就不在磁盘上，MC 也不需要它们进 classpath。
    pub missing: Vec<String>,
    /// 版本 json 的继承链（子 → 父）。`--print-game` 会打出来：一个"官方 installer
    /// 装的 Forge"和一个"合并式 json"在参数上看不出区别，但排查少库时这是第一个
    /// 要确认的事实。
    pub chain: Vec<String>,
    /// 启动前跑过的校验结论（一行一条，给 `--print-game` 与启动日志）。
    pub checks: Vec<String>,
}

impl LaunchPlan {
    /// 交给 `Command` 的顺序：jvm → mainClass → game。
    pub fn args(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.jvm.len() + 1 + self.game.len());
        out.extend(self.jvm.iter().map(|t| t.text.clone()));
        out.push(self.main_class.text.clone());
        out.extend(self.game.iter().map(|t| t.text.clone()));
        out
    }

    /// 给人看的命令行（`cmdline.txt` 与日志）。含空格的参数加双引号。
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&quote(&self.java.to_string_lossy()));
        for a in self.args() {
            out.push(' ');
            out.push_str(&quote(&a));
        }
        out
    }

    /// 带来源标注的分段视图（`--print-game`）。
    pub fn render_annotated(&self) -> String {
        let mut out = String::new();
        let section = |title: &str, tokens: &[ArgToken], out: &mut String| {
            out.push_str(&format!("{title} ({} token)\n", tokens.len()));
            for t in tokens {
                out.push_str(&format!("  {:<8} {}\n", t.origin.label(), t.text));
            }
        };
        section("jvm", &self.jvm, &mut out);
        out.push_str("main\n");
        out.push_str(&format!(
            "  {:<8} {}\n",
            self.main_class.origin.label(),
            self.main_class.text
        ));
        section("game", &self.game, &mut out);
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

// ------------------------------------------------------ 参数来源与启动前校验

/// 主类：实例覆盖优先，否则版本 json 的 `mainClass`。
///
/// 这个函数存在的全部意义是把"同一个事实只存一份"落到代码里：以前实例里默认
/// 写死 `net.minecraft.launchwrapper.Launch`，而 json 里也有一个 `mainClass`，
/// 两者不同步时的报错完全指不到源头。
fn resolve_main_class(
    spec: &InstanceSpec,
    json: &serde_json::Value,
) -> Result<(String, ArgOrigin), String> {
    if let Some(v) = spec.game.main_class.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return Ok((v.to_owned(), ArgOrigin::Instance));
    }
    if let Some(v) =
        json.get("mainClass").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
    {
        return Ok((v.to_owned(), ArgOrigin::Json));
    }
    Err("版本 json 里没有 mainClass，实例里也没写 game.main_class —— 两者至少要有一个"
        .to_owned())
}

/// 类名 → jar 条目名：`a.b.C` → `a/b/C.class`。
fn class_entry(class: &str) -> String {
    let mut s = class.replace('.', "/");
    s.push_str(".class");
    s
}

struct ClassScan {
    /// 扫遍 classpath 也没找到的类（原始类名）。
    missing: Vec<String>,
    /// 中央目录读不开的 jar（文件名）。它们不计入 missing —— 扫不到不等于不存在。
    unreadable: Vec<String>,
    /// 实际扫过的 jar 数。类在前几个 jar 就找到时会少于 classpath 长度，
    /// 所以报"扫了 N 个"比报"classpath 有 N 项"诚实。
    scanned: usize,
}

/// 在 classpath 的 jar 中央目录里找这些类，**不解压任何数据**。
///
/// 只读文件尾的 EOCD 和中央目录段，对 20 MB 的客户端 jar 也是几毫秒的事。
fn locate_classes(classpath: &[PathBuf], wanted: &[&str]) -> ClassScan {
    let mut pending: Vec<(String, String)> =
        wanted.iter().map(|c| ((*c).to_owned(), class_entry(c))).collect();
    let mut unreadable: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    for jar in classpath {
        if pending.is_empty() {
            break;
        }
        scanned += 1;
        match jar_entry_names(jar) {
            Ok(names) => {
                let set: std::collections::HashSet<&str> =
                    names.iter().map(|s| s.as_str()).collect();
                pending.retain(|(_, entry)| !set.contains(entry.as_str()));
            }
            Err(_) => unreadable.push(
                jar.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| jar.display().to_string()),
            ),
        }
    }
    ClassScan { missing: pending.into_iter().map(|(c, _)| c).collect(), unreadable, scanned }
}

/// 前三个名字 + "等 N 个"。列表长了以后，报错信息本身会变得没人读。
fn short_list(items: &[String]) -> String {
    let head: Vec<&str> = items.iter().take(3).map(|s| s.as_str()).collect();
    if items.len() > 3 {
        format!("{} 等 {} 个", head.join("、"), items.len())
    } else {
        head.join("、")
    }
}

/// jar（zip）中央目录里的文件名列表。实现见 [`zipmin`]。
fn jar_entry_names(path: &Path) -> Result<Vec<String>, String> {
    zipmin::names(path)
}


/// 读 `versions\<version>\<version>.json`。
///
/// 单独一个函数是因为这份 json 现在被三处要用（classpath、asssetIndex/type、
/// 主类与 tweaker），而它的读取失败信息必须一致 —— 三份各自的 "读不到" 只会让
/// 人以为是三个不同的问题。
fn read_version_json(client: &Path, version: &str) -> Result<(PathBuf, serde_json::Value), String> {
    let vjson = client.join("versions").join(version).join(format!("{version}.json"));
    let raw = std::fs::read_to_string(&vjson)
        .map_err(|e| format!("读不到版本 json {}：{e}", vjson.display()))?;
    let json: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("版本 json {} 解析失败：{e}", vjson.display()))?;
    Ok((vjson, json))
}

// ---------------------------------------------------------------- 版本 json

/// 一条继承链合并后的结果。
///
/// `inheritsFrom` 不是可选项：**官方 installer 装出来的 Forge 就是继承式的**
/// （`1.12.2-forge-14.23.5.2864.json` 里只有 `inheritsFrom` 和自己的库，原版那
/// 五十多个库在父层）。旧实现只看单层 json，遇到这种会少掉一半库，症状是
/// `NoClassDefFoundError: net/minecraft/client/main/Main`。这台机器的 json 恰好
/// 是合并式的（"导入到本地"生成的那种），所以一直没炸 —— 那是运气，不是设计。
pub struct VersionJson {
    /// 合并后的 json：父在前，子覆盖父。
    pub merged: Value,
    /// 从子到父的版本名，`chain[0]` 是实例里写的那个。
    pub chain: Vec<String>,
    /// 逐层原始 json，同样从子到父。参数模板要按"父先子后"拼。
    pub layers: Vec<Value>,
    /// 客户端 jar：沿链找第一个存在的 `versions\<v>\<v>.jar`。
    pub client_jar: Option<PathBuf>,
}

/// 读一条继承链并合并。
///
/// 上限 16 层不是防"深"，是防环：`A → B → A` 这种手写错误会让循环永不结束，而
/// 报错里带上整条链比"栈溢出"有用得多。
fn load_version_chain(client: &Path, version: &str) -> Result<VersionJson, String> {
    let mut chain: Vec<String> = Vec::new();
    let mut layers: Vec<Value> = Vec::new();
    let mut cur = version.to_owned();
    let mut closed = false;
    for _ in 0..16 {
        if chain.iter().any(|v| *v == cur) {
            return Err(format!(
                "版本 json 的 inheritsFrom 成环：{} → {cur}",
                chain.join(" → ")
            ));
        }
        let (_path, json) = read_version_json(client, &cur)?;
        let next = json
            .get("inheritsFrom")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        chain.push(cur.clone());
        layers.push(json);
        match next {
            Some(parent) => cur = parent,
            None => {
                closed = true;
                break;
            }
        }
    }
    if !closed {
        return Err(format!(
            "版本 json 的继承链超过 16 层（{}），疑似成环",
            chain.join(" → ")
        ));
    }

    let mut merged = serde_json::Map::new();
    for layer in layers.iter().rev() {
        let Some(obj) = layer.as_object() else { continue };
        for (key, value) in obj {
            match key.as_str() {
                // 合并完就没有"父"这个概念了，留着只会让后续代码以为还有一层。
                "inheritsFrom" => {}
                "libraries" => merge_libraries(&mut merged, value),
                "arguments" => merge_arguments(&mut merged, value),
                _ => {
                    merged.insert(key.clone(), value.clone());
                }
            }
        }
    }

    let client_jar = chain
        .iter()
        .map(|v| client.join("versions").join(v).join(format!("{v}.jar")))
        .find(|p| p.is_file());

    Ok(VersionJson { merged: Value::Object(merged), chain, layers, client_jar })
}

/// 父库先、子库后；子层覆盖同名构件（`group:artifact[:classifier]`，**不看版本**）
/// 时**原地替换**，位置不变。
///
/// 两条边界，都是踩过的：
///
/// * **同一层里的重复不去重。** 1.12.2 的 json 里 `lwjgl-platform` 有两个版本
///   （2.9.4 与 2.9.2），vanilla 也两个都留着；按构件身份去重会干掉 2.9.4，而
///   dll 恰恰只在它的 natives jar 里 —— 症状是 `UnsatisfiedLinkError: no lwjgl
///   in java.library.path`，与"合并库"这件事看不出关系。
/// * **覆盖要原地替换。** classpath 顺序对 Forge 有意义（`launchwrapper` 要在
///   原版库之前），把子层覆盖的库挪到末尾会改掉这个顺序。
fn merge_libraries(merged: &mut serde_json::Map<String, Value>, incoming: &Value) {
    let Some(list) = incoming.as_array() else { return };
    let target = merged
        .entry("libraries".to_owned())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(arr) = target.as_array_mut() else { return };
    // 本层开始的位置：只拿它前面的条目（父层）比对，本层内部互不干扰。
    let inherited = arr.len();
    for lib in list {
        let key = lib.get("name").and_then(Value::as_str).and_then(lib_identity);
        let pos = key.and_then(|k| {
            arr[..inherited].iter().position(|x| {
                x.get("name").and_then(Value::as_str).and_then(lib_identity).as_deref()
                    == Some(k.as_str())
            })
        });
        match pos {
            Some(i) => arr[i] = lib.clone(),
            None => arr.push(lib.clone()),
        }
    }
}

/// 库的身份：`group:artifact:version[:classifier]` → `group:artifact[:classifier]`。
///
/// 少于三段（不合 Maven 规范的自制库名）时原样返回 —— 这种情况按全名比对，
/// 至少不会把两个不同的东西当成同一个。
fn lib_identity(name: &str) -> Option<String> {
    let parts: Vec<&str> = name.split(':').collect();
    match parts.len() {
        3 => Some(format!("{}:{}", parts[0], parts[1])),
        4 => Some(format!("{}:{}:{}", parts[0], parts[1], parts[3])),
        _ => Some(name.to_owned()),
    }
}

/// `arguments.game` / `arguments.jvm` 都是数组：父的在前、子的在后。
///
/// 顺序就是优先级 —— 去重时"后面的赢"，所以子版本追加的 `--width` 会顶掉父层的。
fn merge_arguments(merged: &mut serde_json::Map<String, Value>, incoming: &Value) {
    let Some(obj) = incoming.as_object() else { return };
    let target = merged
        .entry("arguments".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(dst) = target.as_object_mut() else { return };
    for (key, value) in obj {
        match (dst.get_mut(key).and_then(Value::as_array_mut), value.as_array()) {
            (Some(existing), Some(list)) => existing.extend(list.iter().cloned()),
            _ => {
                dst.insert(key.clone(), value.clone());
            }
        }
    }
}

/// `rules` 过完一遍的库列表。classpath 与 natives 共用同一份判断 ——
/// 分成两处写，早晚会出现"这个库进了 classpath 但没解 natives"。
fn allowed_libraries<'a>(json: &'a Value, feat: Features) -> Vec<&'a Value> {
    json.get("libraries")
        .and_then(Value::as_array)
        .map(|libs| {
            libs.iter()
                .filter(|lib| rules_allow(lib.get("rules"), feat))
                .collect()
        })
        .unwrap_or_default()
}

/// 从合并后的库列表拼 classpath。
///
/// 三条从脚本时代继承下来的规则，一条都不能省：
/// 1. 路径取 `downloads.artifact.path`，它是**相对 libraries 根**的（不是相对
///    客户端根）—— 首版少 55 个 jar 就是这么来的；
/// 2. 没有该字段时按 `group:artifact:version` 推导；
/// 3. `rules` 不通过就不进（见 [`allowed_libraries`]）。
fn build_classpath(
    client: &Path,
    vj: &VersionJson,
    feat: Features,
) -> (Vec<PathBuf>, Vec<String>) {
    let lib_root = client.join("libraries");
    let mut cp: Vec<PathBuf> = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    for lib in allowed_libraries(&vj.merged, feat) {
        let rel = lib
            .pointer("/downloads/artifact/path")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| lib.get("name").and_then(Value::as_str).and_then(name_to_path));
        let Some(rel) = rel else { continue };
        let full = lib_root.join(rel.replace('/', r"\"));
        if full.is_file() {
            // 同一个 jar 在这份 json 里出现两次（`text2speech` 就是），完全相同的
            // 路径进两次 classpath 没有语义，只会让 `-cp` 变长、让"扫了几个 jar"
            // 这个计数失真。不同版本的同名构件不在这里去重 —— 那是 json 的意图
            // （log4j 2.8.1 与 2.15.0 同时存在），由 JVM 的"先出现者胜"决定。
            if !cp.contains(&full) {
                cp.push(full);
            }
        } else {
            missing.push(rel);
        }
    }

    match &vj.client_jar {
        Some(jar) => cp.push(jar.clone()),
        None => missing.push(format!("{}.jar（继承链上各层都没有）", vj.chain[0])),
    }
    (cp, missing)
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

/// 旧行为那个 natives 目录：优先 `natives-tmp`，退回版本目录。
///
/// 只在两种情况下用到它：解 natives 全部失败时的兜底，以及
/// `java.extract_natives = false` 的排障路径。新路径是 `runtime\natives\<版本>`。
fn fallback_natives_dir(client: &Path, version: &str) -> PathBuf {
    let tmp = client.join("natives-tmp");
    if tmp.is_dir() {
        tmp
    } else {
        client.join("versions").join(version)
    }
}

// ------------------------------------------------------------- 参数模板接线

/// 模板里的游戏参数。
///
/// 两种写法都会读，顺序是**旧式在前、新式在后**：
///
/// * 旧式（`minecraftArguments` 一整串）只有 1.12 及以前有，取**最近一层**的那个
///   —— 子版本要换模板时直接写一份新的覆盖父层即可。
/// * 新式（`arguments.game` 数组）从 1.13 起才有，父先子后。
///
/// 为什么不是"有 `arguments` 就不用 `minecraftArguments`"：继承式的版本里这两个
/// 会同时出现 —— 父层是老版本的整串（`--gameDir` 之类的基础参数都在里面），子层
/// 只追加了自己那几条。二选一会丢掉一半参数。拼接之后由去重收口：同一个键后面
/// 的赢，与 vanilla 的合并效果一致。
///
/// 返回 `None` 表示这份 json 根本不给模板（自制版本、手写的最小版本 json）。
fn template_game_args(
    vj: &VersionJson,
    ph: &Placeholders,
    feat: Features,
) -> Result<Option<Vec<ArgToken>>, String> {
    let mut out: Vec<ArgToken> = Vec::new();
    let mut found = false;

    for layer in &vj.layers {
        if let Some(raw) = layer.get("minecraftArguments").and_then(Value::as_str) {
            if !raw.trim().is_empty() {
                out.extend(args_from_string(raw, ph)?);
                found = true;
                break;
            }
        }
    }

    let mut appended = false;
    for layer in vj.layers.iter().rev() {
        if let Some(arr) = layer.pointer("/arguments/game") {
            if arr.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                appended = true;
                out.extend(args_from_array(arr, ph, feat)?);
            }
        }
    }

    Ok(if found || appended { Some(out) } else { None })
}

/// json 没给模板时的底线参数。
///
/// 留着它不是因为"怕删"，而是因为模板缺失时唯一能做的事就是给一组 1.6~1.12 都
/// 认的值；`--print-game` 输出一段空白比输出这组值更难排障。
#[allow(clippy::too_many_arguments)]
fn fallback_game_args(
    spec: &InstanceSpec,
    version: &str,
    saves: &Path,
    client: &Path,
    asset_index: &str,
    version_type: &str,
) -> Vec<ArgToken> {
    let mut game = vec![
        ArgToken::builtin("--username"),
        ArgToken::instance(spec.game.player.clone()),
        ArgToken::builtin("--version"),
        ArgToken::builtin(version.to_owned()),
        ArgToken::builtin("--gameDir"),
        ArgToken::instance(saves.to_string_lossy().into_owned()),
        ArgToken::builtin("--assetsDir"),
        ArgToken::builtin(client.join("assets").to_string_lossy().into_owned()),
        ArgToken::builtin("--assetIndex"),
        ArgToken::builtin(asset_index.to_owned()),
        ArgToken::builtin("--uuid"),
        ArgToken::builtin("00000000-0000-0000-0000-000000000000"),
        ArgToken::builtin("--accessToken"),
        ArgToken::builtin("0"),
        ArgToken::builtin("--userType"),
        ArgToken::builtin("legacy"),
        ArgToken::builtin("--versionType"),
        ArgToken::builtin(version_type.to_owned()),
    ];
    game.shrink_to_fit();
    game
}

/// 版本 json 的 `arguments.jvm`（1.13+）。
///
/// 只作为**补充**：内存、GC、`-XX:ErrorFile`、`-Djava.library.path`、`-cp`
/// 由我们自己算，所以这些键在 json 里的版本一律丢弃。
///
/// 丢弃时**连值一起丢**：`-cp` 在数组里是两个 token（`"-cp"` 与
/// `"${classpath}"`），只删键不删值的话，那个 classpath 字符串会变成主类前面
/// 多出来的位置参数 —— 报错是 `Could not find or load main class C:\...jar`，
/// 与真正的原因隔着十万八千里。
fn jvm_from_json(
    vj: &VersionJson,
    ph: &Placeholders,
    feat: Features,
) -> Result<(Vec<ArgToken>, usize), String> {
    let mut raw: Vec<ArgToken> = Vec::new();
    for layer in vj.layers.iter().rev() {
        if let Some(arr) = layer.pointer("/arguments/jvm") {
            raw.extend(args_from_array(arr, ph, feat)?);
        }
    }
    let total = raw.len();
    let mut out = Vec::with_capacity(total);
    let mut i = 0usize;
    while i < raw.len() {
        let token = raw[i].clone();
        if jvm_arg_is_ours(&token.text) {
            let bare = matches!(token.text.as_str(), "-cp" | "-classpath");
            i += 1;
            if bare && i < raw.len() && !raw[i].text.starts_with('-') {
                i += 1;
            }
            continue;
        }
        out.push(token);
        i += 1;
    }
    let dropped = total - out.len();
    Ok((out, dropped))
}

/// 这些 JVM 键由 [`InstanceSpec::plan`] 自己算，json 里的同名项一律丢弃。
fn jvm_arg_is_ours(arg: &str) -> bool {
    let key = arg.split('=').next().unwrap_or(arg);
    if matches!(
        key,
        "-cp" | "-classpath" | "-Djava.library.path" | "-Dos.name" | "-Dos.version"
    ) {
        return true;
    }
    key.starts_with("-Xmx")
        || key.starts_with("-Xms")
        || key.starts_with("-Xss")
        || key.starts_with("-XX:ErrorFile")
        || key.starts_with("-XX:+HeapDumpOnOutOfMemoryError")
        || key.starts_with("-Dminecraft.launcher.")
}

impl InstanceSpec {
    /// 组装启动计划（纯计算 + 一次 natives 解压）。
    ///
    /// 参数只有一个来源：版本 json 的模板。实例文件只负责"这台机器的事实"
    /// （玩家名、存档目录、内存、覆盖值），不再自己拼参数 —— 那正是旧实现
    /// "改一个参数就起不来"的根源。
    pub fn plan(&self, cfg: &Config, paths: &Paths) -> Result<LaunchPlan, String> {
        self.validate()?;
        let client = self.client_path(paths);
        let saves = self.saves_path(paths);
        let size = self.target_size(cfg);
        let version = &self.game.version;

        let vj = load_version_chain(&client, version)?;
        let merged = &vj.merged;

        // ---- JRE：json 说"这个版本要 Java N"，那就按 N 去挑 ----
        //
        // 顺序是「先读 json，再找 java」，与"先找到 java 再说"的直觉相反：要哪个
        // 版本是 json 的事（1.13 起每个 json 都写了），机器上有哪几个才是探测器
        // 的事。反过来的话，1.12.2 与 1.20 在同一台机器上就只能靠人肉改配置。
        let required = self.required_java(merged);
        let choice = java::select(&self.java, paths, &client, required)?;
        let java = choice.java.clone();
        // 我们永远给得出两个分辨率的数，所以 `has_custom_resolution` 恒真 ——
        // 它决定的正是 1.13+ 的 `--width/--height` 要不要出现在模板参数里。
        let feat = Features {
            demo_user: false,
            custom_resolution: true,
            quick_plays_support: false,
        };

        let (cp, missing) = build_classpath(&client, &vj, feat);
        if cp.is_empty() {
            return Err(format!(
                "classpath 是空的（客户端 {} 下没有 versions\\{}\\）",
                client.display(),
                version
            ));
        }
        let asset_index = merged
            .pointer("/assetIndex/id")
            .and_then(Value::as_str)
            .unwrap_or("1.12")
            .to_owned();
        let version_type = merged
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("release")
            .to_owned();
        let (main_text, main_origin) = resolve_main_class(self, merged)?;
        let cp_joined = cp
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(";");

        // ---- natives：按 json 的 `natives` 段解到 runtime，客户端目录保持只读 ----
        let lib_root = client.join("libraries");
        let nat_dest = paths.runtime().join("natives").join(version);
        let nat_fallback = fallback_natives_dir(&client, version);
        let nat = if self.java.extract_natives {
            natives::prepare(
                &lib_root,
                &allowed_libraries(merged, feat),
                &nat_dest,
                &nat_fallback,
            )
        } else {
            let mut off = natives::Outcome::default();
            if nat_fallback.is_dir() {
                off.dirs.push(nat_fallback.clone());
            }
            off
        };
        // 一个目录都没有时退回旧路径：没有 `java.library.path` 的 lwjgl 启动即崩。
        let library_path = if nat.dirs.is_empty() {
            nat_fallback.to_string_lossy().into_owned()
        } else {
            nat.library_path()
        };

        // ---- 占位符：这里列的是 Mojang 的协议名，不是我们的口味 ----
        let mut ph = Placeholders::new();
        ph.set("auth_player_name", self.game.player.clone())
            .set("version_name", version.clone())
            .set("game_directory", saves.to_string_lossy().into_owned())
            .set("assets_root", client.join("assets").to_string_lossy().into_owned())
            .set("game_assets", client.join("assets").to_string_lossy().into_owned())
            .set("assets_index_name", asset_index.clone())
            .set("auth_uuid", "00000000-0000-0000-0000-000000000000")
            .set("auth_access_token", "0")
            .set("auth_session", "-")
            .set("user_type", "legacy")
            // MC 1.7 会把它当 JSON 解析；空串会让它当成"没有属性表"。
            .set("user_properties", "{}")
            .set("version_type", version_type.clone())
            .set("natives_directory", library_path.clone())
            .set("launcher_name", "stream-server")
            .set("launcher_version", env!("CARGO_PKG_VERSION"))
            .set("classpath", cp_joined.clone())
            .set("classpath_separator", ";")
            .set("library_directory", lib_root.to_string_lossy().into_owned())
            .set(
                "primary_jar",
                vj.client_jar
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
            .set("resolution_width", size.0.to_string())
            .set("resolution_height", size.1.to_string());

        let mut jvm: Vec<ArgToken> = vec![
            ArgToken::builtin(format!("-Xmx{}M", self.java.heap_mb)),
            ArgToken::builtin("-Xms512M"),
            ArgToken::builtin("-XX:+UseG1GC"),
            ArgToken::builtin(format!("-Djava.library.path={library_path}")),
            ArgToken::builtin("-Dfml.ignoreInvalidMinecraftCertificates=true"),
            ArgToken::builtin("-Dfml.ignorePatchDiscrepancies=true"),
            // JVM 自己的崩溃日志也进 runtime，与游戏的 crash-report 放在一起，
            // 否则一个在 %TEMP%、一个在存档里，没人能同时找到。
            ArgToken::builtin(format!(
                "-XX:ErrorFile={}",
                paths.runtime().join("hs_err_%p.log").display()
            )),
        ];
        let (json_jvm, json_jvm_dropped) = jvm_from_json(&vj, &ph, feat)?;
        jvm.extend(json_jvm);

        // log4j2 的配置（1.12+）。给了它 `logs/latest.log` 才有正常格式；文件不在
        // 就跳过 —— 这不该阻止启动，只是日志换成 log4j 默认样式。
        let mut log_note: Option<String> = None;
        if let Some(arg) = merged.pointer("/logging/client/argument").and_then(Value::as_str) {
            let id = merged
                .pointer("/logging/client/file/id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let cfg_file = client.join("assets").join("log_configs").join(id);
            if !id.is_empty() && cfg_file.is_file() {
                let mut lp = Placeholders::new();
                lp.set("path", cfg_file.to_string_lossy().into_owned());
                jvm.push(ArgToken::json(lp.expand(arg)?));
            } else if !arg.trim().is_empty() {
                log_note = Some(format!(
                    "logging: assets\\log_configs\\{id} 不存在，本次用 log4j 默认配置"
                ));
            }
        }

        jvm.extend(self.java.jvm_args.iter().cloned().map(ArgToken::instance));
        jvm.push(ArgToken::builtin("-cp"));
        // classpath 的内容来自版本 json 的 libraries，形状（`-cp` 与 `;`）是我们定的。
        jvm.push(ArgToken::json(cp_joined));

        // ---- 游戏段：模板 + 我们的覆盖值，最后统一去重 ----
        let from_template = template_game_args(&vj, &ph, feat)?;
        let used_template = from_template.is_some();
        let mut game = from_template.unwrap_or_else(|| {
            fallback_game_args(self, version, &saves, &client, &asset_index, &version_type)
        });
        // 尺寸：模板里已经有 `--width`（1.13+ 的写法）也没关系，去重时后面赢。
        game.push(ArgToken::builtin("--width"));
        game.push(ArgToken::builtin(size.0.to_string()));
        game.push(ArgToken::builtin("--height"));
        game.push(ArgToken::builtin(size.1.to_string()));
        // tweaker 覆盖：实例写了才追加，去重会顶掉模板里的那一个。
        if let Some(tweak) = self.game.tweak_class.as_deref().map(str::trim).filter(|s| !s.is_empty())
        {
            game.push(ArgToken::builtin("--tweakClass"));
            game.push(ArgToken::instance(tweak.to_owned()));
        }
        game.extend(self.game.game_args.iter().cloned().map(ArgToken::instance));

        // 空值先删（`--versionType ""` 这种），再去重（后者覆盖前者）。
        let game = drop_empty_values(game);
        let (game, overridden) = override_flags(game);
        let texts: Vec<String> = game.iter().map(|t| t.text.clone()).collect();
        let tweak = find_flag_value(&texts, "--tweakClass");

        // ---- 启动前的校验 -----------------------------------------------
        // 都是"几分钟就能查、但错过了要花半小时"的错，所以宁可在这里拒绝启动。
        let mut checks: Vec<String> = Vec::new();

        // 0) 选了哪个 JRE。排在最前是因为后面每一条校验（尤其"类在哪"）都建立在
        //    "这个 JVM 能加载这个字节码"之上。
        checks.push(format!("java: {}", choice.note));
        if let Some(w) = &choice.warning {
            checks.push(format!("java: {w}"));
        }

        if vj.chain.len() > 1 {
            checks.push(format!("chain: {}", vj.chain.join(" → ")));
        }
        // 1) 占位符残留。json 里的模板串被抄进 game_args、或者将来新增的占位符
        //    没人提供值时，都会在这里被拦住。
        for t in jvm.iter().chain(game.iter()) {
            if t.text.contains("${") {
                return Err(format!(
                    "启动参数里残留未展开的占位符 {:?}（来源：{}）—— 这不是 java 能认的写法",
                    t.text,
                    t.origin.label()
                ));
            }
        }

        // 2) 去重之后仍然重复：说明去重逻辑漏了某个键（拆成了两组 token 之类）。
        //    这是断言，不是策略 —— 正常路径上恒为空。
        let dups = duplicate_flags(&game);
        if !dups.is_empty() {
            return Err(format!(
                "去重之后游戏参数里还有重复的键：{}。这是参数拼接的 bug，请报出来",
                dups.join("、")
            ));
        }
        checks.push(format!(
            "args: {} 个 jvm / {} 个 game token，模板={}，覆盖掉的键={}，残留占位符=0",
            jvm.len(),
            game.len(),
            if used_template { "json" } else { "内置（json 没有模板）" },
            if overridden.is_empty() { "-".to_owned() } else { overridden.join(",") }
        ));
        if json_jvm_dropped > 0 {
            checks.push(format!(
                "jvm: json 的 arguments.jvm 里丢掉 {json_jvm_dropped} 个与我们重复的 token"
            ));
        }
        if let Some(note) = log_note {
            checks.push(note);
        }
        checks.push(nat.summary());
        for note in &nat.notes {
            checks.push(format!("natives: {note}"));
        }
        if !nat.failed.is_empty() {
            checks.push(format!(
                "natives: 解压失败 {} 个（{}），已退回旧目录",
                nat.failed.len(),
                short_list(&nat.failed)
            ));
        }

        // 3) 主类与 tweaker 必须真的在 classpath 里。
        if self.java.verify_classes {
            let mut wanted: Vec<&str> = vec![main_text.as_str()];
            if let Some(text) = tweak.as_deref() {
                wanted.push(text);
            }
            let found = locate_classes(&cp, &wanted);
            if !found.missing.is_empty() {
                // 读不开的 jar 单独点出来：这类失败最常见的两种原因是"类名写错"
                // 与"扫描器读不懂这个 jar"，而它们的处置完全相反。
                let tail = if found.unreadable.is_empty() {
                    String::new()
                } else {
                    format!(
                        "；另有 {} 个 jar 读不开（{}），扫不到不等于不存在",
                        found.unreadable.len(),
                        short_list(&found.unreadable)
                    )
                };
                return Err(format!(
                    "classpath 里找不到 {}（扫了 {} 个 jar{tail}）。\
                     确认 versions/{version}.json 的 mainClass 与实例里的 game.main_class 是否写错；\
                     确实无法扫描时把实例里的 java.verify_classes 设为 false",
                    found.missing.join("、"),
                    found.scanned
                ));
            }
            checks.push(format!(
                "classes: {} 个类都在 classpath 里（扫了 {} 个 jar）",
                wanted.len(),
                found.scanned
            ));
        }

        Ok(LaunchPlan {
            java,
            java_major: choice.major,
            cwd: client,
            jvm,
            main_class: ArgToken { text: main_text, origin: main_origin },
            game,
            size,
            model: self.display.model,
            missing,
            chain: vj.chain,
            checks,
        })
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

/// `--print-game` 的正文：生效实例 + 分段参数 + 那一条完整命令行。
///
/// 为什么不是 JSON：这个输出最常见的用途是"贴到别处比对"—— 与
/// `toolchain\start-mc.ps1` 拼的那条、与上一次启动 `runtime\cmdline.txt` 里的那条、
/// 以及与改坏之前的那一次。分段的 `builtin / json / instance` 三列回答的是
/// "这个参数是谁写的"，比一整行命令少一半猜的时间。最后仍然给一整行，方便复制。
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
        "# java major : {}\n",
        plan.java_major.map(|m| m.to_string()).unwrap_or_else(|| "?".to_owned())
    ));
    out.push_str(&format!(
        "# display    : {}x{} {:?}\n",
        plan.size.0, plan.size.1, plan.model
    ));
    out.push_str(&format!("# main       : {}\n", plan.main_class.text));
    if plan.chain.len() > 1 {
        out.push_str(&format!("# chain      : {}\n", plan.chain.join(" → ")));
    }
    for c in &plan.checks {
        out.push_str(&format!("# check      : {c}\n"));
    }
    if !plan.missing.is_empty() {
        out.push_str(&format!("# missing    : {:?}\n", plan.missing));
    }
    out.push('\n');
    out.push_str(&plan.render_annotated());
    out.push_str("\ncommand\n");
    out.push_str(&plan.render());
    out.push('\n');
    Ok(out)
}

/// `--print-java` 的正文：这台机器上所有能找到的 JRE、它们的主版本、以及最终会
/// 选哪一个。
///
/// 为什么值得单独一个开关：JRE 的问题全都发生在「服务起得来、游戏起不来」这一层，
/// 而那时人只能隔着串流页面看一个黑屏。这个开关可以在任何一台机器上离线跑，
/// 输出足够回答三个问题 —— 机器上有哪几个 Java、json 要求几、会选哪一个。
///
/// **实例读不到也照样输出**：实例文件写坏时，最需要知道的正是"机器上有哪些
/// Java"，而不是一句"实例读不到"。
pub fn report_java(cfg: &Config) -> Result<String, String> {
    let paths = Paths::resolve(cfg)?;
    let mut out = String::new();
    out.push_str(&format!("# games root : {}\n", paths.root.display()));
    out.push_str(&format!("# jre dir    : {}\n", paths.runtime().join("jre").display()));

    let loaded = InstanceSpec::load(&paths, &cfg.game_instance);
    let mut spec_java = JavaSpec::default();
    let (client, required) = match &loaded {
        Ok(spec) => {
            let client = spec.client_path(&paths);
            let req = load_version_chain(&client, &spec.game.version)
                .ok()
                .and_then(|vj| spec.required_java(&vj.merged));
            out.push_str(&format!(
                "# instance   : {} ({})\n",
                cfg.game_instance,
                if spec.name.is_empty() { "-" } else { &spec.name }
            ));
            spec_java = spec.java.clone();
            (client, req)
        }
        Err(err) => {
            out.push_str(&format!("# instance   : 读不到（{err}）\n"));
            (paths.clients(), None)
        }
    };
    out.push_str(&format!("# client     : {}\n", client.display()));
    out.push_str(&format!(
        "# required   : {}\n",
        match required {
            Some(n) => format!("Java {n}"),
            None => "无（json 没写 javaVersion，也没设 java.major）".to_owned(),
        }
    ));
    if !spec_java.path.trim().is_empty() {
        out.push_str(&format!("# pin        : java.path = {}\n", spec_java.path));
    }

    // 表里列的是"机器上有哪些"，`chosen` 走的是 `select` —— 与真正启动时**同一个
    // 函数**。这里曾经直接调 `pick`，于是实例写了 `java.path` 时表里的选择与实际
    // 启动不一致：一个查错用的输出偏偏不说真话，是最坏的一种。
    let cands = java::discover(&paths, &client);
    out.push_str(&format!("# found      : {} 个 JRE\n", cands.len()));
    for c in &cands {
        out.push_str(&format!(
            "  {:<9} {:<7} {}   <- {}\n",
            c.origin.label(),
            c.major.map(|m| format!("Java {m}")).unwrap_or_else(|| "?".to_owned()),
            c.java.display(),
            c.source
        ));
    }

    match java::select(&spec_java, &paths, &client, required) {
        Ok(ch) => {
            out.push_str(&format!("# chosen     : {}\n", ch.note));
            if let Some(w) = ch.warning {
                out.push_str(&format!("# warning    : {w}\n"));
            }
        }
        Err(err) => out.push_str(&format!("# chosen     : 无 —— {err}\n")),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::template::split_arguments;
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

    #[test]
    fn split_arguments_respects_quotes() {
        let tokens = split_arguments(r#"--gameDir "C:\Program Files\x" --width 1280"#);
        assert_eq!(
            tokens,
            vec!["--gameDir", r"C:\Program Files\x", "--width", "1280"],
            "引号内的空格不能拆开"
        );
        // 空串参数（`""`）必须留下一个空 token，否则位置参数会整体左移。
        assert_eq!(split_arguments(r#"--opt "" --x"#), vec!["--opt", "", "--x"]);
        // 引号内的 \" 还原成 "
        assert_eq!(split_arguments(r#""a\"b""#), vec![r#"a"b"#]);
        assert!(split_arguments("   ").is_empty());
    }

    #[test]
    fn find_flag_value_takes_the_next_token() {
        let toks: Vec<String> =
            ["--a", "1", "--tweakClass", "x.Y"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(find_flag_value(&toks, "--tweakClass").as_deref(), Some("x.Y"));
        assert_eq!(find_flag_value(&toks, "--missing"), None);
        // flag 在末尾时没有值可取，必须是 None 而不是越界 panic。
        assert_eq!(find_flag_value(&toks[3..], "--tweakClass"), None);
    }

    #[test]
    fn duplicate_flags_only_counts_double_dash() {
        let toks = vec![
            ArgToken::builtin("--width"),
            ArgToken::builtin("1280"),
            ArgToken::builtin("-Dfoo=1"),
            ArgToken::builtin("-Dfoo=2"),
            ArgToken::builtin("--width"),
            ArgToken::builtin("1920"),
        ];
        assert_eq!(duplicate_flags(&toks), vec!["--width"]);
        // 只有值、没有键时不该报。
        let ok = vec![ArgToken::builtin("--a"), ArgToken::builtin("--b")];
        assert!(duplicate_flags(&ok).is_empty());
    }

    /// 测试用的最小 `VersionJson`：只有 `layers` 对模板拼接有意义。
    fn vj(layers: Vec<Value>) -> VersionJson {
        let n = layers.len();
        VersionJson {
            merged: Value::Null,
            chain: vec![String::new(); n],
            layers,
            client_jar: None,
        }
    }

    fn ph() -> Placeholders {
        let mut p = Placeholders::new();
        p.set("auth_player_name", "CloudPlayer");
        p.set("game_directory", r"C:\g\saves\default");
        p.set("natives_directory", r"C:\g\runtime\natives");
        p.set("classpath", r"C:\g\cp.jar");
        p
    }

    fn texts(tokens: &[ArgToken]) -> Vec<&str> {
        tokens.iter().map(|t| t.text.as_str()).collect()
    }

    fn owned_texts(tokens: &[ArgToken]) -> Vec<String> {
        tokens.iter().map(|t| t.text.clone()).collect()
    }

    #[test]
    fn main_class_prefers_the_instance_override() {
        let mut spec = example_instance("x");
        let json = serde_json::json!({ "mainClass": "net.minecraft.launchwrapper.Launch" });

        let (text, origin) = resolve_main_class(&spec, &json).expect("json 里有");
        assert_eq!(text, "net.minecraft.launchwrapper.Launch");
        assert_eq!(origin, ArgOrigin::Json);

        spec.game.main_class = Some("custom.Main".to_owned());
        let (text, origin) = resolve_main_class(&spec, &json).expect("实例覆盖");
        assert_eq!(text, "custom.Main");
        assert_eq!(origin, ArgOrigin::Instance);

        // 空白覆盖视为没写 —— 空的 `""` 是最容易在 JSON 里手滑出来的值。
        spec.game.main_class = Some("   ".to_owned());
        assert_eq!(resolve_main_class(&spec, &json).expect("回退 json").1, ArgOrigin::Json);

        // 两边都没有：必须报错，而不是给一个猜的值。
        spec.game.main_class = None;
        assert!(resolve_main_class(&spec, &serde_json::json!({})).is_err());
    }

    #[test]
    fn tweak_class_comes_from_the_template() {
        // 旧式：minecraftArguments 一整串
        let old = vj(vec![serde_json::json!({
            "minecraftArguments": "--username ${auth_player_name} --tweakClass net.minecraftforge.fml.common.launcher.FMLTweaker --versionType Forge"
        })]);
        let args = template_game_args(&old, &ph(), Features::default())
            .expect("应当能取到")
            .expect("旧式模板应当有参数");
        assert_eq!(
            find_flag_value(&owned_texts(&args), "--tweakClass").as_deref(),
            Some("net.minecraftforge.fml.common.launcher.FMLTweaker")
        );
        assert_eq!(texts(&args)[1], "CloudPlayer", "占位符要展开");

        // 新式：arguments.game 数组
        let new = vj(vec![serde_json::json!({
            "arguments": { "game": ["--tweakClass", "optifine.OptiFineTweaker"] }
        })]);
        let args = template_game_args(&new, &ph(), Features::default())
            .expect("应当能取到")
            .expect("新式模板也应当有参数");
        assert_eq!(
            find_flag_value(&owned_texts(&args), "--tweakClass").as_deref(),
            Some("optifine.OptiFineTweaker")
        );

        // 原版 json / 自制 json 没有模板：返回 None，由调用方走内置底线参数。
        assert!(template_game_args(&vj(vec![serde_json::json!({})]), &ph(), Features::default())
            .expect("没有就是没有")
            .is_none());
    }

    #[test]
    fn new_style_args_concatenate_parent_then_child() {
        // layers 顺序是"子 → 父"，拼接顺序必须是"父 → 子"，否则子的覆盖值会先被
        // 父值顶掉 —— 这是组合顺序最容易写反的一处。
        let chain = vj(vec![
            serde_json::json!({"arguments": {"game": ["--flag", "child"]}}),
            serde_json::json!({"arguments": {"game": ["--flag", "parent"]}}),
        ]);
        let args = template_game_args(&chain, &ph(), Features::default()).unwrap().unwrap();
        assert_eq!(texts(&args), vec!["--flag", "parent", "--flag", "child"]);
        // 交给去重之后，后者（子）赢。
        let (merged, over) = override_flags(args);
        assert_eq!(texts(&merged), vec!["--flag", "child"]);
        assert_eq!(over, vec!["--flag"]);
    }

    #[test]
    fn old_and_new_style_coexist_in_an_inheritance_chain() {
        // 继承式的真实形态：父层是老版本的整串（基础参数），子层只追加自己的几条。
        // 二选一就会丢掉一半参数，所以两者都要读，顺序是旧式在前。
        let chain = vj(vec![
            serde_json::json!({"arguments": {"game": ["--child-flag", "1"]}}),
            serde_json::json!({"minecraftArguments": "--gameDir ${game_directory}"}),
        ]);
        let args = template_game_args(&chain, &ph(), Features::default()).unwrap().unwrap();
        assert_eq!(texts(&args), vec!["--gameDir", r"C:\g\saves\default", "--child-flag", "1"]);
    }

    #[test]
    fn jvm_args_from_json_drop_the_keys_we_own() {
        // `-cp` 在数组里是两个 token：只删键不删值，那个 classpath 会变成主类前面
        // 多出来的位置参数。
        let chain = vj(vec![serde_json::json!({
            "arguments": {"jvm": [
                "-Dtest.keep=1",
                "-Djava.library.path=${natives_directory}",
                "-cp", "${classpath}",
                "-Xmx999M"
            ]}
        })]);
        let (out, dropped) = jvm_from_json(&chain, &ph(), Features::default()).unwrap();
        assert_eq!(texts(&out), vec!["-Dtest.keep=1"]);
        assert_eq!(dropped, 4, "-Djava.library.path / -cp 与其值 / -Xmx999M");
    }

    #[test]
    fn libraries_merge_by_name_in_place() {
        let mut merged = serde_json::Map::new();
        merge_libraries(
            &mut merged,
            &serde_json::json!([{"name": "a:b:1"}, {"name": "c:d:1"}, {"name": "a:b:0"}]),
        );
        // 同一层里的重复**保留**：1.12.2 的 lwjgl-platform 就有两个版本，而 dll
        // 只在其中一个的 natives jar 里。
        let names: Vec<String> = merged["libraries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, vec!["a:b:1", "c:d:1", "a:b:0"]);

        // 子层覆盖父层的构件：位置不变，内容替换。
        merge_libraries(
            &mut merged,
            &serde_json::json!([{"name": "a:b:2"}, {"name": "e:f:1"}]),
        );
        let names: Vec<String> = merged["libraries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, vec!["a:b:2", "c:d:1", "a:b:0", "e:f:1"]);

        // arguments 的子键是数组拼接，父在前。
        let mut args = serde_json::Map::new();
        merge_arguments(&mut args, &serde_json::json!({"game": ["--a"], "jvm": ["-Dx=1"]}));
        merge_arguments(&mut args, &serde_json::json!({"game": ["--b"]}));
        assert_eq!(args["arguments"]["game"], serde_json::json!(["--a", "--b"]));
        assert_eq!(args["arguments"]["jvm"], serde_json::json!(["-Dx=1"]));
    }

    #[test]
    fn class_entry_uses_slash_and_suffix() {
        assert_eq!(class_entry("a.b.C"), "a/b/C.class");
        assert_eq!(class_entry("Launch"), "Launch.class");
    }
}
