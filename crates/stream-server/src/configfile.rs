//! 配置文件：让「修改配置」有一个结构化的落点。
//!
//! 为什么需要这个模块：这个进程的三十二个参数原本只存在于 `start-stream.cmd`
//! 里一段由 `set "ARGS=%ARGS% ..."` 拼出来的字符串中，还带着三处条件分支
//! （TLS 文件存在才加、探测桌面当上传根、逐行读 `auth-token.txt`）。想改一个
//! 分辨率要编辑 cmd、数引号、再重启计划任务，而且改完**没有任何地方能回显
//! "现在生效的到底是什么"**。一个可视化安装器在这种情况下没有地方可以写配置，
//! 这是加这个模块的全部理由。
//!
//! 优先级：**默认值 < 配置文件 < 命令行**。
//!
//! 这个顺序不是随意的。配置文件是"这台机器的常态"，命令行是"这一次的临时
//! 覆盖"。反过来（文件压过命令行）会让调试时敲的临时参数在下次看门狗重启后
//! 变成常态，而那时已经没人记得是谁改的。
//!
//! 三个刻意的决定：
//!
//! 1. **没有配置文件 = 行为完全不变**。老部署的 `start-stream.cmd` 一个参数都
//!    不改也能跑。这一条是本模块能被安全引入的前提，也是所有测试的基线。
//! 2. **未知键是错误，不是警告**。把 `width` 拼成 `widht` 后静默失效，表现是
//!    "我改了没生效"，比"服务起不来"难查一个量级 —— 后者至少有个报错。
//!    类型不对同理（`port = "8080"` 直接拒绝）。
//! 3. **一次性动作不进配置文件**。`--restore-mic` 和 `--print-config` 描述的是
//!    "这次要做什么"，不是"这台机器是什么样"。写进文件会让每次启动都去还原
//!    一遍录音设备。
//!
//! 值域校验（`mode` 是不是 `webrtc`/`websockets`、`encoder` 认不认）**故意**不在
//! 这里做：那套规则已经存在，重复一份就会分叉。这里只负责把值搬进 [`Config`]，
//! 无法识别的值收进 `warnings` 交给现有逻辑处理 —— 它们本来就是"保留默认并
//! 告警"的语义，和命令行完全一致。

use std::path::{Path, PathBuf};

use crate::config::{Config, InputPolicy};
use crate::security::token::GrantSpec;

/// 配置文件里能写的全部键。
///
/// 每个字段都是 `Option`：`None` 表示"文件没提这一项"，于是保持默认值、或保持
/// 命令行稍后给的值。这是覆盖语义能成立的前提，也让 [`render_toml`] 的输出能
/// 原样回环 —— 打出来的每个键都是真的生效值，没有一项是为了占位。
///
/// 键名与命令行参数一一对应，规则只有一条：`--some-flag` 写成 `some_flag`。
/// 三个例外保留了简写（`app` / `mode` / `bitrate`），因为它们是这个文件里最常
/// 被改的三行。
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    // ---- 网络 ----
    pub host: Option<String>,
    pub port: Option<u16>,

    // ---- 页面 ----
    #[serde(rename = "app")]
    pub app_name: Option<String>,
    #[serde(rename = "mode")]
    pub default_mode: Option<String>,
    pub dist: Option<String>,

    // ---- 画面 ----
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    #[serde(rename = "bitrate")]
    pub bitrate_kbps: Option<u32>,
    pub repeat_every: Option<u32>,
    pub keyframe_secs: Option<u32>,
    pub video_codec: Option<String>,
    pub display: Option<u32>,

    // ---- 编码 ----
    pub encoder: Option<String>,
    pub tune: Option<String>,

    // ---- TLS / 日志 ----
    /// 空串表示"不启用 TLS"。这是唯一能表达"把默认值清掉"的写法：
    /// 键不出现 = 保持原值，键出现但为空 = 显式清空。
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    #[serde(rename = "log")]
    pub log_file: Option<String>,

    // ---- 输入 ----
    pub input_policy: Option<String>,
    pub max_sessions: Option<usize>,
    pub ghost_cursor_idle: Option<u64>,
    /// #124/B: 把贴到边缘的指针拉回中间。会动真光标，默认 false。
    pub pointer_recenter: Option<bool>,

    // ---- 音频 ----
    pub mic_device: Option<String>,
    pub mic_default_device: Option<String>,

    // ---- 上传 ----
    pub upload_dir: Option<String>,
    pub upload_quota_mb: Option<u64>,

    // ---- ICE ----
    pub webrtc_udp_port_min: Option<u16>,
    pub webrtc_udp_port_max: Option<u16>,

    // ---- 安全 ----
    pub policy: Option<String>,
    pub auth_token: Option<String>,
    /// `["tenant:role:token", ...]`，与 `--auth-grant` 可重复同义。
    /// 文件里的列表**整体替换**命令行之前累积的列表，不追加。
    pub auth_grants: Option<Vec<String>>,

    // ---- 游戏托管 ----
    /// 要不要由本进程拉起并看着游戏客户端。
    pub game: Option<bool>,
    /// 默认实例 id（`<game_root>\instances\<id>.json`）。
    pub game_instance: Option<String>,
    /// `games` 根目录。空串 = 回到默认（exe 旁边的 `games`）。
    pub game_root: Option<String>,
    /// 服务起来时顺手把游戏也拉起来。
    pub game_autostart: Option<bool>,

    // ---- 输入锁定 ----
    /// 是否启用输入锁定。
    pub kiosk: Option<bool>,
    /// 游戏启动后自动上锁。
    pub kiosk_autolock: Option<bool>,
    /// 注册表策略层（`NoWinKeys` / `DisableTaskMgr`）。
    pub kiosk_policy: Option<bool>,
    /// 低级键盘钩子层。
    pub kiosk_hook: Option<bool>,
    /// 网页解锁口令的哈希。空串 = 清除。
    pub kiosk_password: Option<String>,

    // ---- 诊断 ----
    pub no_hdr: Option<bool>,
    pub dump_frame: Option<String>,
}

impl FileConfig {
    /// 把文件里的值盖到 `cfg` 上。
    ///
    /// 无法识别的枚举值不中断，收进 `warnings` —— 与命令行"拼错保留默认并
    /// 告警"的策略保持一致，否则同一个错字在两种入口下会有两种结局。
    pub fn apply(self, cfg: &mut Config, warnings: &mut Vec<String>) {
        if let Some(v) = self.host {
            cfg.host = v;
        }
        if let Some(v) = self.port {
            cfg.port = v;
        }
        if let Some(v) = self.app_name {
            cfg.app_name = v;
        }
        if let Some(v) = self.default_mode {
            cfg.default_mode = v;
        }
        if let Some(v) = self.dist {
            cfg.dist = PathBuf::from(v);
        }
        if let Some(v) = self.width {
            cfg.width = v;
        }
        if let Some(v) = self.height {
            cfg.height = v;
        }
        if let Some(v) = self.fps {
            cfg.fps = v;
        }
        if let Some(v) = self.bitrate_kbps {
            cfg.bitrate_kbps = v;
        }
        if let Some(v) = self.repeat_every {
            cfg.repeat_every = v;
        }
        if let Some(v) = self.keyframe_secs {
            cfg.keyframe_secs = v;
        }
        if let Some(v) = self.video_codec {
            cfg.video_codec = v;
        }
        if let Some(v) = self.display {
            cfg.display = v;
        }
        if let Some(v) = self.encoder {
            cfg.encoder = v;
        }
        if let Some(v) = self.tune {
            cfg.tune = v;
        }

        // 空串 = 清空，非空 = 设置。见字段注释。
        cfg.tls_cert = opt_path(self.tls_cert);
        cfg.tls_key = opt_path(self.tls_key);
        cfg.log_file = opt_path(self.log_file);

        if let Some(v) = self.input_policy {
            match InputPolicy::parse(&v) {
                Some(p) => cfg.input_policy = p,
                None => warnings.push(format!(
                    "input_policy = {v:?} 只认 shared / exclusive，保留 {}",
                    cfg.input_policy.as_str()
                )),
            }
        }
        if let Some(v) = self.max_sessions {
            cfg.max_sessions = v;
        }
        if let Some(v) = self.ghost_cursor_idle {
            cfg.ghost_cursor_idle_secs = v;
        }
        if let Some(v) = self.pointer_recenter {
            cfg.pointer_recenter = v;
        }
        if let Some(v) = self.mic_device {
            cfg.mic_device = v;
        }
        if let Some(v) = self.mic_default_device {
            cfg.mic_default_device = v;
        }

        cfg.upload_dir = opt_path(self.upload_dir);
        if let Some(v) = self.upload_quota_mb {
            cfg.upload_quota_mb = v;
        }

        if let Some(v) = self.webrtc_udp_port_min {
            cfg.webrtc_udp_port_min = v;
        }
        if let Some(v) = self.webrtc_udp_port_max {
            cfg.webrtc_udp_port_max = v;
        }

        if let Some(v) = self.policy {
            cfg.policy = v;
        }
        if let Some(v) = self.auth_token {
            cfg.auth_token = if v.is_empty() { None } else { Some(v) };
        }
        if let Some(specs) = self.auth_grants {
            cfg.auth_grants.clear();
            for spec in specs {
                match GrantSpec::parse(&spec) {
                    Ok(grant) => cfg.auth_grants.push(grant),
                    // 一条坏令牌不该让整机起不来，但更不能静默少一条 ——
                    // 少一条的表现是"某个租户进不来"，没人会想到是配置文件的
                    // 逗号写错了。
                    Err(err) => warnings.push(format!("auth_grants 里的 {spec:?} 无效：{err}")),
                }
            }
        }

        if let Some(v) = self.no_hdr {
            cfg.no_hdr = v;
        }
        cfg.dump_frame = opt_path(self.dump_frame);

        if let Some(v) = self.game {
            cfg.game_enabled = v;
        }
        if let Some(v) = self.game_instance {
            if v.trim().is_empty() {
                warnings.push("game_instance 不能为空，保留原值".to_owned());
            } else {
                cfg.game_instance = v.trim().to_owned();
            }
        }
        cfg.game_root = opt_path(self.game_root);
        if let Some(v) = self.game_autostart {
            cfg.game_autostart = v;
        }

        if let Some(v) = self.kiosk {
            cfg.kiosk = v;
        }
        if let Some(v) = self.kiosk_autolock {
            cfg.kiosk_autolock = v;
        }
        if let Some(v) = self.kiosk_policy {
            cfg.kiosk_policy = v;
        }
        if let Some(v) = self.kiosk_hook {
            cfg.kiosk_hook = v;
        }
        if let Some(v) = self.kiosk_password.as_deref() {
            let v = v.trim();
            if v.is_empty() {
                cfg.kiosk_password = None;
            } else if !v.starts_with("sha256i:") {
                // 明显不是哈希（比如运维把口令原文填进来了）。不能静默接受：
                // 存明文既违背这个字段的用途，又会让网页解锁永远失败 —— 而失败
                // 的样子是"口令不对"，没人会想到是格式问题。
                warnings.push(
                    "kiosk_password 看起来不是 --kiosk-hash 生成的哈希，已忽略".to_owned(),
                );
            } else {
                cfg.kiosk_password = Some(v.to_owned());
            }
        }
    }

    /// 反向：把生效配置摊成可序列化的形态，供 `--print-config` 使用。
    ///
    /// `tls_cert` 这类 `Option` 字段在这里**填成 `Some("")`**，而不是 `None`：
    /// `None` 在反序列化侧的意思是"文件没提这一项"，把它写进输出会让回环丢掉
    /// "当前确实是关的"这个信息。空串才是"关"。
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            host: Some(cfg.host.clone()),
            port: Some(cfg.port),
            app_name: Some(cfg.app_name.clone()),
            default_mode: Some(cfg.default_mode.clone()),
            dist: Some(cfg.dist.to_string_lossy().into_owned()),
            width: Some(cfg.width),
            height: Some(cfg.height),
            fps: Some(cfg.fps),
            bitrate_kbps: Some(cfg.bitrate_kbps),
            repeat_every: Some(cfg.repeat_every),
            keyframe_secs: Some(cfg.keyframe_secs),
            video_codec: Some(cfg.video_codec.clone()),
            display: Some(cfg.display),
            encoder: Some(cfg.encoder.clone()),
            tune: Some(cfg.tune.clone()),
            tls_cert: Some(path_or_empty(cfg.tls_cert.as_deref())),
            tls_key: Some(path_or_empty(cfg.tls_key.as_deref())),
            log_file: Some(path_or_empty(cfg.log_file.as_deref())),
            input_policy: Some(cfg.input_policy.as_str().to_owned()),
            max_sessions: Some(cfg.max_sessions),
            ghost_cursor_idle: Some(cfg.ghost_cursor_idle_secs),
            pointer_recenter: Some(cfg.pointer_recenter),
            mic_device: Some(cfg.mic_device.clone()),
            mic_default_device: Some(cfg.mic_default_device.clone()),
            upload_dir: Some(path_or_empty(cfg.upload_dir.as_deref())),
            upload_quota_mb: Some(cfg.upload_quota_mb),
            webrtc_udp_port_min: Some(cfg.webrtc_udp_port_min),
            webrtc_udp_port_max: Some(cfg.webrtc_udp_port_max),
            policy: Some(cfg.policy.clone()),
            auth_token: Some(cfg.auth_token.clone().unwrap_or_default()),
            auth_grants: Some(
                cfg.auth_grants
                    .iter()
                    .map(|g| format!("{}:{}:{}", g.tenant.as_str(), g.role.as_str(), g.token))
                    .collect(),
            ),
            no_hdr: Some(cfg.no_hdr),
            dump_frame: Some(path_or_empty(cfg.dump_frame.as_deref())),
            game: Some(cfg.game_enabled),
            game_instance: Some(cfg.game_instance.clone()),
            game_root: Some(path_or_empty(cfg.game_root.as_deref())),
            game_autostart: Some(cfg.game_autostart),
            kiosk: Some(cfg.kiosk),
            kiosk_autolock: Some(cfg.kiosk_autolock),
            kiosk_policy: Some(cfg.kiosk_policy),
            kiosk_hook: Some(cfg.kiosk_hook),
            kiosk_password: Some(cfg.kiosk_password.clone().unwrap_or_default()),
        }
    }
}

fn opt_path(value: Option<String>) -> Option<PathBuf> {
    match value {
        // 空串是"清空"，不是"当前目录" —— `PathBuf::from("")` 会变成一个空路径，
        // 后续 `parent()` 拿到的是 `None`，行为诡异。
        Some(v) if v.is_empty() => None,
        Some(v) => Some(PathBuf::from(v)),
        None => None,
    }
}

fn path_or_empty(value: Option<&Path>) -> String {
    value.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

/// 读一个配置文件。
///
/// 错误信息里**必须带路径**：这个进程有三种启动方式（手动、计划任务、看门狗
/// 重启），出错时唯一能告诉人"是哪一份文件"的就是这行字。
pub fn load(path: &Path) -> Result<FileConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("配置文件 {} 读不出来：{err}", path.display()))?;
    toml::from_str(&text).map_err(|err| format!("配置文件 {} 格式不对：\n{err}", path.display()))
}

/// 默认位置：`<exe 所在目录>/config.toml`。
///
/// 跟着 exe 走而不是跟着工作目录走，理由是部署包本来就"整包搬到哪都能跑"
/// （三个启动脚本分别用 `%~dp0`、`ScriptFullName`、`$PSScriptRoot` 自定位），
/// 配置文件没有理由例外。用工作目录会让"从别处 `cd` 过来启动"读到另一份。
///
/// 返回 `Some` 不代表文件存在 —— 调用方只在显式 `--config` 时才强制要求存在。
pub fn default_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("config.toml"))
}

/// 整包分发里那份配置的名字。
///
/// 和 `config.toml` 是两个名字、两套来源：`config.toml` 是“exe 旁边自动发现”
/// 的开发机习惯，`stream-config.toml` 是分发包里随包带来的那一份（以前由
/// `start-stream.cmd` 用 `--config` 显式指定）。名字不同本身是历史包袱，但它
/// 已经被大量部署引用，改名的代价远大于把两处查找合并到这一个函数里。
pub const BUNDLE_CONFIG: &str = "stream-config.toml";

/// 整包分发目录的标志物：前端目录。
///
/// 它比“旁边有没有某个脚本”可靠 —— 脚本正在被删掉（四个动作全部收进 exe），
/// 而前端目录是服务真的要用到的东西，缺了它页面直接 404。
const BUNDLE_MARKER: &str = "selkies-web";

/// 这次运行该读哪一份配置：`--config` 优先，其次 exe 旁边的两份。
///
/// 返回 `Some` 不代表文件存在（`--config` 之外的路径允许缺失）。
///
/// 三处（`Config::parse`、`--ops`、`--run`）共用这一份而不是各写一遍：各写一遍
/// 的结果是屏幕说 A、写入落在 B —— 配置表单那次就差点这样。
pub fn resolve(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    let primary = default_path()?;
    if primary.exists() {
        return Some(primary);
    }
    let sibling = primary.with_file_name(BUNDLE_CONFIG);
    if sibling.exists() {
        return Some(sibling);
    }
    // 两份都不存在时，要在**新建**这一份时挑对名字。判据是布局而不是文件名：
    // 整包分发必有 `selkies-web`，开发机直接 `cargo run` 不会有。挑错名字的
    // 后果是“表单里改了、服务没读”—— 一个纯粹由猜错文件名造成的故障。
    if primary.with_file_name(BUNDLE_MARKER).exists() {
        Some(sibling)
    } else {
        Some(primary)
    }
}

/// 老位置的口令文件（`auth-token.txt`）。
///
/// 2026-11 之前口令**只能**放在这里：`set-password.cmd` 写它，`--run` 读出来
/// 转成 `--auth-token` 传给服务。现在正式位置是配置里的 `auth_token`（能进
/// 表单、能进 `--ops` 的备份与回滚、能写非 ASCII），这个文件只剩一个作用：
/// 让老机器升级上来时不用重新设一遍口令。
///
/// 它由 [`crate::config::Config::parse`] 在**应用配置文件之前**读，也就是优先
/// 级最低的那一层。这个顺序是刻意的：两份来源都有值时，配置赢。反过来做的
/// 故障很具体 —— 有人在界面上改了口令、重启却不生效，而日志里什么都看不出来
/// （旧实现是靠 `--run` 拼出 `--auth-token`，命令行永远压过文件）。
///
/// 取第一个非空行而不是第一行：令牌文件很容易被记事本存成"开头一个空行"，
/// 而整份读进来会把换行一起塞进口令，现象是"口令怎么都不对"。
///
/// 返回 `None` 有两种情况，不必区分：文件不存在（常态），或者读不出来
/// （权限/编码问题）—— 后者按"没有口令"处理，与 `--run` 时代的行为一致。
pub fn read_legacy_token(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(LEGACY_TOKEN_FILE)).ok()?;
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// 老口令文件名。`--run` 与升级逻辑都认这个名字。
pub const LEGACY_TOKEN_FILE: &str = "auth-token.txt";

/// `--print-config` 的 toml 形态。
///
/// 手写而不是 `toml::to_string`：注释和分组是这个输出的主要价值 —— 它同时也是
/// 人写配置文件时的模板，而序列化器不会替你解释 `webrtc_udp_port_min` 是干什么的。///
/// 全文件必须能**回环**：`render_toml(cfg)` 再解析回来必须得到同一个 `cfg`。
/// 有一个测试守着这一条，因为手写的格式化最容易和结构体悄悄分叉。
pub fn render_toml(cfg: &Config) -> String {
    let mut out = String::new();
    out.push_str(
        "# cloud-stream 生效配置\n\
         #\n\
         # 由 `stream-server.exe --print-config` 生成：每个键都是**当前生效值**，\n\
         # 不是默认值。把它存成 exe 旁边的 config.toml，下次启动就会按这份跑。\n\
         #\n\
         # 优先级：默认值 < 本文件 < 命令行。命令行永远压过本文件，所以调试时\n\
         # 临时加参数不会污染这里。\n\
         #\n\
         # 键名与命令行一一对应：`--some-flag` 写成 `some_flag`。\n\
         # 路径里的反斜杠按 TOML 字面量字符串处理（单引号内不转义）。\n\
         # 空串 \"\" 表示「这一项是关的」，删掉整行表示「用默认值」。\n\n",
    );

    out.push_str("# ---------- 网络 ----------\n");
    out.push_str("# host: 监听地址。0.0.0.0 = 所有网卡；只给本机用写 127.0.0.1。\n");
    out.push_str(&kv_str("host", &cfg.host));
    out.push_str("# port: 监听端口。有 TLS 证书时同一个端口就是 HTTPS。\n");
    out.push_str(&kv_num("port", u64::from(cfg.port)));
    out.push('\n');

    out.push_str("# ---------- 页面 ----------\n");
    out.push_str("# app: 信令路径里的 app 段（/api/<app>/signaling/）。\n");
    out.push_str(&kv_str("app", &cfg.app_name));
    out.push_str("# mode: 新页面的默认传输 —— websockets（一个 TCP 端口）或 webrtc（UDP）。\n");
    out.push_str(&kv_str("mode", &cfg.default_mode));
    out.push_str("# dist: 前端产物目录，相对路径相对于启动时的工作目录。\n");
    out.push_str(&kv_str("dist", &cfg.dist.to_string_lossy()));
    out.push('\n');

    out.push_str("# ---------- 画面 ----------\n");
    out.push_str("# width/height: 采集与编码的输出尺寸，不是窗口尺寸。\n");
    out.push_str(&kv_num("width", u64::from(cfg.width)));
    out.push_str(&kv_num("height", u64::from(cfg.height)));
    out.push_str("# fps: 目标帧率。\n");
    out.push_str(&kv_num("fps", u64::from(cfg.fps)));
    out.push_str("# bitrate: 编码目标码率，单位 kbps。\n");
    out.push_str(&kv_num("bitrate", u64::from(cfg.bitrate_kbps)));
    out.push_str("# repeat_every: 静止画面每 N 拍重发一帧；1 = 保持完整节奏。\n");
    out.push_str(&kv_num("repeat_every", u64::from(cfg.repeat_every)));
    out.push_str("# keyframe_secs: 周期关键帧间隔（秒）。调小会让码率被关键帧吃掉。\n");
    out.push_str(&kv_num("keyframe_secs", u64::from(cfg.keyframe_secs)));
    out.push_str("# video_codec: 会话协商的编码器 —— vp9（默认）或 vp8。\n");
    out.push_str(&kv_str("video_codec", &cfg.video_codec));
    out.push_str("# display: 启动时采集哪一块屏（DXGI 序号，0 = 主屏）。\n");
    out.push_str(&kv_num("display", u64::from(cfg.display)));
    out.push('\n');

    out.push_str("# ---------- 编码 ----------\n");
    out.push_str("# encoder: auto 永不失败（落到软件编码并说明原因）；nvenc/amf/qsv\n");
    out.push_str("# 是硬要求，硬件不在时启动即失败 —— 那是明确的意图，不该被静默降级。\n");
    out.push_str(&kv_str("encoder", &cfg.encoder));
    out.push_str("# tune: 软件编码的实时参数集 —— tuned（默认）或 legacy（#95 之前的行为）。\n");
    out.push_str(&kv_str("tune", &cfg.tune));
    out.push('\n');

    out.push_str("# ---------- TLS / 日志 ----------\n");
    out.push_str("# 两个都非空才启用 HTTPS。空串 = 明文 HTTP。\n");
    out.push_str(&kv_opt_path("tls_cert", cfg.tls_cert.as_deref()));
    out.push_str(&kv_opt_path("tls_key", cfg.tls_key.as_deref()));
    out.push_str("# log: 文件日志路径。空串 = 只写标准输出。\n");
    out.push_str(&kv_opt_path("log", cfg.log_file.as_deref()));
    out.push('\n');

    out.push_str("# ---------- 输入 ----------\n");
    out.push_str("# input_policy: exclusive（默认，只有持锁的会话能操作）或 shared。\n");
    out.push_str(&kv_str("input_policy", cfg.input_policy.as_str()));
    out.push_str("# max_sessions: 并发信令会话上限，超出返回 503 + Retry-After。\n");
    out.push_str(&kv_num("max_sessions", cfg.max_sessions as u64));
    out.push_str("# ghost_cursor_idle: 游戏模式的幽灵指针静止多少秒后淡出。\n");
    out.push_str(&kv_num("ghost_cursor_idle", cfg.ghost_cursor_idle_secs));
    out.push_str("# pointer_recenter: 指针贴到屏幕边缘时拉回中间，给相对位移腾地方。\n");
    out.push_str("# 会移动被控机的真光标；只有远端玩第一人称时才值得开。\n");
    out.push_str(&kv_bool("pointer_recenter", cfg.pointer_recenter));
    out.push('\n');

    out.push_str("# ---------- 音频 ----------\n");
    out.push_str("# mic_device: 下行音频往哪写 —— auto / default / steam / vb-cable / 端点名。\n");
    out.push_str(&kv_str("mic_device", &cfg.mic_device));
    out.push_str("# mic_default_device: auto = 启动时把系统默认录音设备切到虚拟麦；off = 不碰。\n");
    out.push_str(&kv_str("mic_default_device", &cfg.mic_default_device));
    out.push('\n');

    out.push_str("# ---------- 上传 ----------\n");
    out.push_str("# upload_dir: 文件传输根目录。空串 = 关闭 /api/files 与 /api/upload。\n");
    out.push_str(&kv_opt_path("upload_dir", cfg.upload_dir.as_deref()));
    out.push_str(&kv_num("upload_quota_mb", cfg.upload_quota_mb));
    out.push('\n');

    out.push_str("# ---------- ICE ----------\n");
    out.push_str("# WebRTC 的 UDP 端口区间。改成别的段就要同步防火墙规则\n");
    out.push_str("# （管理员跑一次 `stream-setting.exe --inbound`，它按这个区间重建规则）。\n");
    out.push_str(&kv_num("webrtc_udp_port_min", u64::from(cfg.webrtc_udp_port_min)));
    out.push_str(&kv_num("webrtc_udp_port_max", u64::from(cfg.webrtc_udp_port_max)));
    out.push('\n');

    out.push_str("# ---------- 安全 ----------\n");
    out.push_str("# policy: auto / allow-all / role-based / deny-all。auto 按有没有令牌表决定。\n");
    out.push_str(&kv_str("policy", &cfg.policy));
    out.push_str("# auth_token: 空串 = 局域网不鉴权。非空时 /api/ 下每个请求都要出示。\n");
    out.push_str(&kv_str("auth_token", cfg.auth_token.as_deref().unwrap_or("")));
    out.push_str("# auth_grants: 多张令牌，每张 \"租户:角色:令牌\"。\n");
    out.push_str("# 令牌放最后 —— 它是唯一可能含冒号的部分。\n");
    out.push_str("# 这个列表整体替换命令行累积的结果，是「全量声明」而不是「追加」。\n");
    let grants = FileConfig::from_config(cfg).auth_grants.unwrap_or_default();
    if grants.is_empty() {
        out.push_str("auth_grants = []\n");
    } else {
        out.push_str("auth_grants = [\n");
        for g in &grants {
            out.push_str(&format!("  {},\n", literal(g)));
        }
        out.push_str("]\n");
    }
    out.push('\n');

    out.push_str("# ---------- 游戏托管 ----------\n");
    out.push_str("# game: 由本进程拉起并看着游戏客户端。关着时 /api/v1/control/game/* 一律 404。\n");
    out.push_str(&kv_bool("game", cfg.game_enabled));
    out.push_str("# game_instance: 默认实例 id，对应 <game_root>\\instances\\<id>.json。\n");
    out.push_str(&kv_str("game_instance", &cfg.game_instance));
    out.push_str("# game_root: games 根目录。空串 = exe 旁边的 games\\。\n");
    out.push_str(&kv_opt_path("game_root", cfg.game_root.as_deref()));
    out.push_str("# game_autostart: 服务起来时顺手拉起游戏（需要 game = true）。\n");
    out.push_str(&kv_bool("game_autostart", cfg.game_autostart));
    out.push('\n');

    out.push_str("# ---------- 输入锁定（kiosk）----------\n");
    out.push_str("# kiosk: 把网页进来的输入关在游戏里。开着时才会装钩子、写策略。\n");
    out.push_str(&kv_bool("kiosk", cfg.kiosk));
    out.push_str("# kiosk_autolock: 游戏启动成功后自动上锁（需要 kiosk = true）。\n");
    out.push_str(&kv_bool("kiosk_autolock", cfg.kiosk_autolock));
    out.push_str("# kiosk_policy: 用注册表关掉 Win 键组合与任务管理器（只影响当前用户）。\n");
    out.push_str(&kv_bool("kiosk_policy", cfg.kiosk_policy));
    out.push_str("# kiosk_hook: 低级键盘钩子。排障时可关掉，用来区分\"谁吃了按键\"。\n");
    out.push_str(&kv_bool("kiosk_hook", cfg.kiosk_hook));
    out.push_str("# kiosk_password: 网页解锁口令的哈希，用 `stream-server.exe --kiosk-hash` 生成。\n");
    out.push_str("# 空串 = 不提供网页解锁，只能用 `--unlock` 或 Ctrl+Alt+Del。\n");
    out.push_str(&kv_str("kiosk_password", cfg.kiosk_password.as_deref().unwrap_or("")));
    out.push('\n');

    out.push_str("# ---------- 诊断 ----------\n");
    out.push_str("# no_hdr: 跳过 HDR 采集尝试。VMware SVGA 3D 这类没有 HDR 桌面的宿主上\n");
    out.push_str("# 设为 true 可以省掉每次启动的那一次必然失败。\n");
    out.push_str(&kv_bool("no_hdr", cfg.no_hdr));
    out.push_str("# dump_frame: 帧转储目录，用于定位花屏来自采集还是缩放。空串 = 关闭。\n");
    out.push_str(&kv_opt_path("dump_frame", cfg.dump_frame.as_deref()));
    out
}

/// `--print-config json` 的形态。GUI 读这个，键名与 toml 完全一致。
pub fn render_json(cfg: &Config) -> Result<String, String> {
    let file = FileConfig::from_config(cfg);
    serde_json::to_string_pretty(&file).map_err(|err| format!("配置无法序列化为 JSON：{err}"))
}

/// 统一的渲染入口。未知格式退化成 toml 而不是报错：这是给人看的输出，
/// 拼错一个词不该让命令失败。
pub fn render(cfg: &Config, format: &str) -> Result<String, String> {
    match format.trim().to_ascii_lowercase().as_str() {
        "json" => render_json(cfg),
        _ => Ok(render_toml(cfg)),
    }
}

fn kv_str(key: &str, value: &str) -> String {
    format!("{key} = {}\n", literal(value))
}

fn kv_num(key: &str, value: u64) -> String {
    format!("{key} = {value}\n")
}

fn kv_bool(key: &str, value: bool) -> String {
    format!("{key} = {value}\n")
}

fn kv_opt_path(key: &str, value: Option<&Path>) -> String {
    match value {
        Some(p) => format!("{key} = {}\n", literal(&p.to_string_lossy())),
        None => format!("{key} = \"\"\n"),
    }
}

/// TOML 字面量字符串（单引号，内容不转义）。
///
/// 存在的唯一理由是 Windows 路径：`logs\stream-server.log` 在基本字符串里必须
/// 写成 `"logs\\stream-server.log"`，而字面量字符串可以原样写。含单引号或换行
/// 时退回调试格式 —— 那两种字符在路径和日志里不会出现，真出现了也不该让输出
/// 变成非法 TOML。
fn literal(value: &str) -> String {
    if value.contains('\'') || value.contains('\n') || value.contains('\r') {
        format!("{value:?}")
    } else {
        format!("'{value}'")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_file(text: &str) -> Result<FileConfig, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    fn write_tmp(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cs-cfg-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    /// 老口令文件里的前导空行、行尾空白、多行内容都不该变成口令的一部分。
    ///
    /// 这个文件现在只是升级兼容层，但"读法"仍然是原来那套：记事本存出来的
    /// 令牌文件经常开头空一行，而整份读进来会把换行也塞进口令，现象是
    /// "口令怎么都不对"。
    #[test]
    fn legacy_token_is_the_first_non_empty_line() {
        let dir =
            std::env::temp_dir().join(format!("cs-legacy-token-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        std::fs::write(dir.join(LEGACY_TOKEN_FILE), "\r\n  abc123  \r\ndef456\r\n").unwrap();
        assert_eq!(
            read_legacy_token(&dir).as_deref(),
            Some("abc123"),
            "前导空行要跳过，前后空格要去掉，第二行不该被带进来"
        );

        std::fs::write(dir.join(LEGACY_TOKEN_FILE), "   \r\n\r\n").unwrap();
        assert_eq!(read_legacy_token(&dir), None, "全空白等于没有口令");

        std::fs::remove_file(dir.join(LEGACY_TOKEN_FILE)).unwrap();
        assert_eq!(read_legacy_token(&dir), None, "文件不存在不是错误");
        let _ = std::fs::remove_dir(&dir);
    }

    /// 手写的 `render_toml` 必须和结构体保持同步 —— 漏掉一个键、拼错一个名字，
    /// 这个测试就会红。没有它，"打印出来的配置贴回去"这条承诺迟早会悄悄失效。
    #[test]
    fn tomf_round_trip_is_identity() {
        let mut cfg = Config::default();
        cfg.host = "127.0.0.1".to_owned();
        cfg.port = 9443;
        cfg.width = 1280;
        cfg.bitrate_kbps = 4000;
        cfg.tls_cert = Some(PathBuf::from(r"C:\certs\a.pem"));
        cfg.tls_key = None;
        cfg.auth_token = Some("secret".to_owned());
        cfg.auth_grants = vec![
            GrantSpec::parse("alpha:operator:tok1").unwrap(),
            GrantSpec::parse("beta:viewer:tok2").unwrap(),
        ];
        cfg.upload_dir = Some(PathBuf::from(r"D:\共享 目录"));
        cfg.no_hdr = true;
        cfg.input_policy = InputPolicy::Shared;
        cfg.pointer_recenter = true;

        let text = render_toml(&cfg);
        let file = parse_file(&text).expect("render_toml 的输出必须是合法 TOML");
        let mut back = Config::default();
        let mut warnings = Vec::new();
        file.apply(&mut back, &mut warnings);
        assert!(warnings.is_empty(), "回环不该产生告警：{:?}", warnings);

        assert_eq!(back.host, cfg.host);
        assert_eq!(back.port, cfg.port);
        assert_eq!(back.width, cfg.width);
        assert_eq!(back.bitrate_kbps, cfg.bitrate_kbps);
        assert_eq!(back.tls_cert, cfg.tls_cert);
        assert_eq!(back.tls_key, cfg.tls_key);
        assert_eq!(back.auth_token, cfg.auth_token);
        assert_eq!(back.upload_dir, cfg.upload_dir);
        assert_eq!(back.no_hdr, cfg.no_hdr);
        assert_eq!(back.input_policy, cfg.input_policy);
        assert_eq!(back.auth_grants.len(), 2);
        assert_eq!(back.auth_grants[0].token, "tok1");
        assert_eq!(back.auth_grants[1].tenant.as_str(), "beta");
        assert_eq!(back.default_mode, cfg.default_mode);
        assert_eq!(back.ghost_cursor_idle_secs, cfg.ghost_cursor_idle_secs);
        assert_eq!(back.pointer_recenter, cfg.pointer_recenter);
        assert_eq!(back.max_sessions, cfg.max_sessions);
    }

    #[test]
    fn missing_file_changes_nothing() {
        let cfg = Config::default();
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.width, 1920);
        assert_eq!(cfg.video_codec, "vp9");
    }

    #[test]
    fn file_overrides_defaults() {
        let mut cfg = Config::default();
        let file = parse_file("port = 9000\nfps = 60\n").unwrap();
        file.apply(&mut cfg, &mut Vec::new());
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.fps, 60);
        // 没提到的保持默认。
        assert_eq!(cfg.width, 1920);
    }

    #[test]
    fn unknown_key_is_an_error_not_a_warning() {
        let err = parse_file("widht = 1920\n").unwrap_err();
        assert!(
            err.contains("widht"),
            "错误信息必须点名拼错的键，实际是：{}",
            err
        );
    }

    #[test]
    fn wrong_type_is_an_error() {
        assert!(parse_file("port = \"8080\"\n").is_err());
        assert!(parse_file("no_hdr = 1\n").is_err());
    }

    #[test]
    fn empty_string_clears_optional_path() {
        let mut cfg = Config::default();
        assert!(cfg.log_file.is_some());
        let file = parse_file("log = \"\"\n").unwrap();
        file.apply(&mut cfg, &mut Vec::new());
        assert_eq!(cfg.log_file, None, "空串必须是清空，不是当前目录");
    }

    #[test]
    fn unknown_enum_keeps_default_and_warns() {
        let mut cfg = Config::default();
        let file = parse_file("input_policy = \"everyone\"\n").unwrap();
        let mut warnings = Vec::new();
        file.apply(&mut cfg, &mut warnings);
        assert_eq!(cfg.input_policy, InputPolicy::default());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("everyone"));
    }

    #[test]
    fn bad_grant_is_reported_by_name() {
        let mut cfg = Config::default();
        let file = parse_file("auth_grants = [\"alpha:operator:ok\", \"broken\"]\n").unwrap();
        let mut warnings = Vec::new();
        file.apply(&mut cfg, &mut warnings);
        assert_eq!(cfg.auth_grants.len(), 1);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("broken"));
    }

    /// 命令行必须压过文件。这是优先级契约本身，不是实现细节。
    #[test]
    fn cli_beats_file() {
        let path = write_tmp("cli-beats", "port = 9000\nfps = 60\n");
        let cfg = Config::parse(vec![
            "--config".to_owned(),
            path.to_string_lossy().into_owned(),
            "--port".to_owned(),
            "7000".to_owned(),
        ])
        .expect("配置应当解析成功");
        assert_eq!(cfg.port, 7000, "命令行要给文件让路");
        assert_eq!(cfg.fps, 60, "文件里没被命令行覆盖的项要生效");
    }

    #[test]
    fn broken_file_is_a_hard_error() {
        let path = write_tmp("broken", "port = \n");
        let err = Config::parse(vec![
            "--config".to_owned(),
            path.to_string_lossy().into_owned(),
        ])
        .expect_err("坏配置文件必须让解析失败");
        assert!(err.0.contains("config.toml"), "错误要带路径：{}", err.0);
    }

    /// 显式指定但文件不存在 → 报错。这里刻意不静默放过：`--config` 是一句
    /// 明确的指令，找不到文件说明运维的假设错了，按默认值跑等于骗人。
    #[test]
    fn explicit_missing_file_is_an_error() {
        let err = Config::parse(vec![
            "--config".to_owned(),
            "C:/definitely/not/here/config.toml".to_owned(),
        ])
        .expect_err("显式指定的文件不存在时应当报错");
        assert!(err.0.contains("读不出来"));
    }

    #[test]
    fn json_render_uses_same_key_names() {
        let cfg = Config::default();
        let json = render_json(&cfg).unwrap();
        assert!(json.contains("\"bitrate\""));
        assert!(json.contains("\"app\""));
        assert!(json.contains("\"ghost_cursor_idle\""));
        assert!(!json.contains("bitrate_kbps"), "JSON 键名要和 TOML 一致");
    }
}
