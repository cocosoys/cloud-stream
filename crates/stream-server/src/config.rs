//! Stage1 server configuration.
//!
//! CLI surface mirrors the retired Python service so the deploy wrapper and
//! the dist probe line up; defaults follow design 4.4. Parsing is hand-rolled
//! to avoid a second clap copy beside hbb_common's dev-only clap.

use std::path::PathBuf;

/// #105: who may inject input into a program (see [`Config::input_policy`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputPolicy {
    /// Every session attached to the program may type and move the pointer.
    Shared,
    /// Only the session holding the program's input lease may.
    Exclusive,
}

impl InputPolicy {
    /// Names are the CLI spelling, lower case, one word. Anything unknown is
    /// rejected so the caller can keep the default and warn — same contract as
    /// `--encoder`.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "shared" => Some(Self::Shared),
            "exclusive" => Some(Self::Exclusive),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Exclusive => "exclusive",
        }
    }
}

impl Default for InputPolicy {
    /// #110: **exclusive** by default.
    ///
    /// This was `shared`, on the reasoning that two tabs of one page are one
    /// person and refusing the second one looked like "input is broken". That
    /// trade was right for a single-operator box and wrong as a default: with
    /// `shared`, every attached session may type, move the pointer, reflow the
    /// host desktop and change the encoder, and `viewer_may_run` — the whole
    /// role gate — never runs, because `ControlGate::owns()` answers `true` for
    /// everybody. A viewer could not be told apart from a controller because
    /// there was no such thing.
    ///
    /// The cost is real and documented: a second tab, or a browser that
    /// reconnects while its old tab is still live, comes up read-only until it
    /// takes the lease. `--input-policy shared` restores the old behaviour in one
    /// word, and the panel says which role it got (`AUTH_SUCCESS.role`).
    fn default() -> Self {
        Self::Exclusive
    }
}

/// 配置文件不能用：读不出来、格式不对、键名拼错、类型不对。
///
/// 单独一个类型而不是裸 `String`，是为了让 `app::run` 能用退出码 2 把"配置错"
/// 和"端口被占"这类运行期失败分开：前者要人改文件，重试多少次都不会好；后者
/// 值得重试。看门狗把这台机器重启一万次也不会救回一个拼错的键名。
#[derive(Debug)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone)]
pub struct Config {
    /// Listen interface.
    pub host: String,
    /// Listen TCP port (HTTPS when a certificate chain is supplied).
    pub port: u16,
    /// Frontend dist root served on non-API GET paths.
    pub dist: PathBuf,
    /// WebRTC app segment echoed into the WS path; the client hardcodes
    /// `webrtc`, stage1 accepts the segment without validation.
    pub app_name: String,
    /// #106: the transport a page with **no stored choice** should start on.
    ///
    /// Both transports are implemented, so the old "the page is pinned to
    /// WebRTC at build time" workaround (`index.html`) is gone: the page asks
    /// `/api/status` and this is the answer. A browser that already switched
    /// with the panel keeps its own choice — see `main.jsx`, which seeds
    /// localStorage only when it is empty instead of overriding it.
    pub default_mode: String,
    /// Output capture width requested from the capturer/encoder.
    pub width: u32,
    /// Output capture height requested from the capturer/encoder.
    pub height: u32,
    /// Target frames per second; `spf = 1.0 / fps` drives the pipeline beat.
    pub fps: u32,
    /// Encoder target bitrate in kbps (VP8 base-bitrate preset ratio applies
    /// inside scrap; this value selects the quality ratio later, design 4.4).
    pub bitrate_kbps: u32,
    /// TLS PEM certificate; empty means plain HTTP.
    pub tls_cert: Option<PathBuf>,
    /// TLS PEM private key; empty means plain HTTP.
    pub tls_key: Option<PathBuf>,
    /// File log path; empty writes to stdout only.
    pub log_file: Option<PathBuf>,
    /// #60: skip the HDR (R16G16B16A16_FLOAT) DuplicateOutput1 attempt. VMware
    /// SVGA 3D has no HDR desktop, so every process start pays one guaranteed
    /// `DuplicateOutput1` failure before falling back to the legacy call; the
    /// flag silences that warning and opens the duplication path directly.
    pub no_hdr: bool,
    /// 一键卸载：把上次遗留的默认录音设备还原，然后立刻退出，不启动任何服务。
    ///
    /// `uninstall.cmd` 在删目录之前跑一次。为什么必须有这个入口：默认录音
    /// 设备是启动时被 `--mic-default-device auto` 接管的，而进程从来不是
    /// 优雅退出的（看门狗、卸载、关机都是 `Stop-Process -Force`），拿不到
    /// 任何清理机会；原值只在 `logs/mic-default.json` 这一个文件里，卸载
    /// 又会把文件连同目录一起删掉 —— 不先还回去，这台机器的默认录音就
    /// 永久停在虚拟线缆上，表现是"本地麦克风没声音"，得让人自己去声音
    /// 设置里改，而那时已经没有任何线索指向我们。
    ///
    /// 与 `--mic-default-device off` 的区别只在于"还原完就退"：后者会把整个
    /// 服务拉起来（绑端口、开始采集），在一个正在被卸载的目录里跑它是荒谬的。
    pub restore_mic: bool,
    /// 一次性动作：打印生效配置后退出，不绑端口、不开日志、不碰采集。
    ///
    /// 值是输出格式（`toml` 默认，或 `json`）。两个理由：人要看"这台机器现在
    /// 到底按什么在跑"——三十二个参数的默认值没人记得住；可视化配置界面要读
    /// 一份**结构化、键名稳定**的当前值去回填表单，让它自己去解析 cmd 里的引号
    /// 拼接是荒唐的。
    ///
    /// 输出必须能回环：把 `--print-config` 的 toml 存成 config.toml 再启动，
    /// 得到的必须是同一个配置。`configfile` 里有一个测试守着这一条。
    pub print_config: Option<String>,
    /// 实际读到的配置文件（`--config` 指定，或 exe 旁边自动发现的）。
    ///
    /// 不是配置项，是"这份配置从哪来"的元信息。两个用处：启动日志要解释
    /// "为什么是 1280 而不是 1920"，诊断时要能一眼看出读的是不是那份以为的文件。
    /// `None` = 没有配置文件，全部来自默认值 + 命令行。
    ///
    /// **刻意不进 `--print-config` 的输出**：那是配置本体，掺进元信息后
    /// "打印出来存回去再打印"就不再逐字节相同，回环承诺会破。
    pub config_source: Option<PathBuf>,
    /// #60: diagnostic frame dump directory. When set, the pipeline writes the
    /// pre-scale BGRA frame and the post-scale BGRA frame every N-th beat to
    /// this directory as BMP files, so a smear/glitch can be attributed to the
    /// capturer, the scaler or the encoder instead of guessing.
    pub dump_frame: Option<PathBuf>,
    /// #64/#66: emit one static-screen repeat frame every N WouldBlock beats.
    /// 1 = hold the full target cadence on an idle desktop; 2 halves the repeat
    /// rate for a browser whose software VP8 decoder cannot keep up (measured
    /// 3.6 % dropped frames at ~54 repeats/s on this host).
    pub repeat_every: u32,
    /// #86: wall-clock period of the periodic keyframe, in seconds (default
    /// 10). This replaces `kf_max_dist = 60` as the authority: libvpx counts
    /// that budget in **encoded frames**, and this pipeline only encodes when
    /// the desktop changes or a repeat is due, so "60 frames" is ~1 s at 60 fps
    /// on a moving picture and minutes on a static one. A keyframe is 100-200 KB
    /// at 1080p, so one per second spends the entire bitrate budget and pins the
    /// quantizer at `q_max` (the #61 experiment D finding); 10 s keeps the
    /// reference chain anchored without paying for it, and the PLI path still
    /// re-anchors on demand.
    pub keyframe_secs: u32,
    /// #67: file-transfer root. When set, `/api/files/{rel}` lists/downloads
    /// files under this directory and `/api/upload` writes into it; `None`
    /// disables both endpoints (403), matching the retired Python service's
    /// `file_manager_path` behavior. Every path component is validated against
    /// traversal, drive letters and reserved characters before touching the
    /// filesystem (RustDesk `fs::join_validated_path` style).
    pub upload_dir: Option<PathBuf>,
    /// #110: 每个租户在上传根下各占一个子目录，这里是**每个租户**的字节配额
    /// （MiB）。`0` 表示不限。
    ///
    /// 为什么配额必须存在：`upload_dir` 是唯一一个外部可以无限往里写字节的
    /// 面。以前它只有一份，一个人就能把主机磁盘写满；分租户之后没有配额，那
    /// 只是把"一个人写满"变成"任意一个人写满自己那份、但总量仍然无上限"。
    ///
    /// 512 MiB 是个偏保守的默认：够传存档、截图、日志，不够把系统盘写死。
    pub upload_quota_mb: u64,
    /// #94: the video codec sessions negotiate with, `vp9` (default) or `vp8`.
    ///
    /// Only the starting value: the panel can change it at runtime, and the
    /// choice lives in `pipeline::codec` (a pick has to outlive the session that
    /// carried it, or the reload it triggers would revert it). This flag exists
    /// so a fresh process can start on VP8 without anyone touching the UI.
    pub video_codec: String,
    /// #122: WebRTC 的 ICE 只用这一段 UDP 端口（含两端），默认 40000–40049。
    ///
    /// 为什么必须固定：ICE 的 host 候选默认绑 `:0`，也就是每个会话由系统随机
    /// 挑端口 —— 防火墙按端口写的规则永远追不上它。全新 Windows 上只放行了
    /// TCP 8080，浏览器发往那个随机端口的 STUN 检查被静默丢弃，ICE 凑不出
    /// 可用候选对，页面只看到"媒体路径失败"，而信令和 WS 画面一切正常。
    /// .148 之所以从没暴露这件事，是因为那台机器的防火墙**三档全关**。
    ///
    /// 一段区间而不是单端口：`ice` 的 `gather_candidates_local` 对**每个本机
    /// 地址**各绑一个 socket（IPv4 多网卡、非 link-local 的 IPv6 都算），一个
    /// 会话通常占 2–4 个。50 个端口对 `max_sessions=8` 留了足够余量。
    ///
    /// 改了这里就要让防火墙规则跟上：管理员跑一次 `stream-setting.exe --inbound`，
    /// 它按这个区间重建 `cloud-stream webrtc UDP`（程序规则 `cloud-stream app`
    /// 仍然覆盖任意端口，但它绑绝对路径，换个目录解压就失效）。
    pub webrtc_udp_port_min: u16,
    pub webrtc_udp_port_max: u16,
    /// #96: how long the client's game-mode ghost pointer stays on screen
    /// without an effective move, in seconds (default 5).
    ///
    /// The pointer itself is drawn by the browser, so this is a *number to
    /// announce*, not a timer the server runs: it rides `server_settings` as
    /// `ghost_cursor.idle_secs` and the page fades its ghost after that. A
    /// game draws its own crosshair, so a ghost that never disappears sits on
    /// top of the action and lies about where the pointer is; one that
    /// disappears too early is gone before the hand starts moving again.
    pub ghost_cursor_idle_secs: u64,
    /// #124/B: 把指针从屏幕边缘拉回中间。**默认关闭**。
    ///
    /// 相对位移是往**真光标**上累加的，光标一贴到桌面边界，后续 `SendInput`
    /// 的位移就被系统丢掉，远端看到的 `movementX` 同时变成 0 —— 表现是"视角
    /// 转不动了"，而且越用力甩越转不动。一次 180° 反手在参考客户端上是 ~1500
    /// counts，1920 宽的屏两枪就顶到边。
    ///
    /// 关着的原因不是"没用"，是它会**动真光标**：开着的机器上，本机用户会和
    /// 远端抢同一个指针，而且 `SetCursorPos` 在没被 `ClipCursor` 夹住的桌面上
    /// 是可见的跳动。默认行为保持插桩前的样子，谁需要谁开，翻车面留在一个开关里。
    pub pointer_recenter: bool,
    /// #88: 监听面鉴权策略名（`auto` / `allow-all` / `role-based` / `deny-all`）。
    ///
    /// 默认 `auto`，由 `main` 决定成哪一个：**没有令牌表时是 `allow-all`**
    /// （与插桩前行为完全一致，骨架不改变任何访问控制），**配了令牌表时是
    /// `role-based`**（令牌不再是"对不对"的开关，而是身份，见
    /// [`crate::security::Role`]）。这样"开了鉴权却还全放行"这种空心状态不会
    /// 悄悄存在 —— 那正是 #110 之前的形态。
    ///
    /// `deny-all` 不是安全模式，是**覆盖率探针**：装上它跑一遍，凡是还能用的
    /// 入口就是漏插桩的地方。真正的策略由上层项目实现 `Policy` 后注入。
    pub policy: String,
    /// #101: 会话口令。`Some` 且非空时，`/api/` 下的每个请求（含信令握手）
    /// 都必须出示它，否则 401；`None` 保持本项目原来的"局域网不鉴权"。
    ///
    /// 载体见 [`crate::security::token`]：`Authorization: Bearer`、cookie
    /// `selkies_token`、`?token=`、以及信令握手的 `selkies.token.*`
    /// subprotocol。四条都是前端 `session-token.js` 已经在用的，所以这个
    /// 开关打开后前端不需要任何改动。
    ///
    /// 刻意**不**做 `auto` 生成：随机口令需要一个密码学随机源，而本项目的
    /// 依赖表里没有（现成可用的只有 SipHash 的随机种子，拿它当 CSPRNG 是
    /// 错的）。口令是运维输入，不是程序猜出来的。
    pub auth_token: Option<String>,
    /// #110: 多张令牌，每张带一个 `(租户, 角色)`。`--auth-grant` 可重复。
    ///
    /// `--auth-token` 是它的单条特例（默认租户的主机级身份），两者可以并存：
    /// 老部署只写 `--auth-token` 时行为不变，多租户部署改用若干条
    /// `--auth-grant tenant:role:token`。
    pub auth_grants: Vec<crate::security::token::GrantSpec>,
    /// MC 云客户端登记：启动后是否向 Minecraft 服务器上报"我在线"。
    ///
    /// 开着的时候本进程在监听就绪后 `POST {mc_report_url}`，把
    /// `name / ip / port / version` 写进服务端 `cloud-clients.yml`。失败不致命：
    /// 重试几次就放弃，Minecraft 侧还有 90s 心跳可以自己把状态拉起来。
    pub mc_report: bool,
    /// 上报目标，完整 URL，例如
    /// `http://127.0.0.1:25574/api/plugins/WebGame/cloud/report`。为空则不上报。
    pub mc_report_url: Option<String>,
    /// 上报用的共享密钥，走 `X-Cloud-Key`。与服务端登记的首报值做 TOFU 比对，
    /// 同一台设备的后续上报必须一致。**与 [`Self::auth_token`] 默认同值**：
    /// 首次生成时用它填好，两者分开存只是为了让运维能单独换掉上报密钥。
    pub mc_report_key: Option<String>,
    /// 上报给 MC 的本机地址。留空则自动探测（第一个非回环 IPv4）。
    /// 监听 `0.0.0.0` 时不能拿 `host` 当地址用，那是"本机所有网卡"不是可连地址。
    pub mc_report_ip: Option<String>,
    /// #95: which encoder backend to use — `auto` (default), `software`, or one
    /// of `nvenc` / `amf` / `qsv` to demand a specific one.
    ///
    /// This is a *policy* name, resolved against the hardware probe in
    /// `platform::hwenc`. `auto` never fails: it lands on software and logs the
    /// reason. A named hardware encoder that is not there **does** fail at
    /// startup — an operator who wrote `--encoder nvenc` asked for a quality
    /// level, and silently streaming software instead would hide the mistake
    /// until the first frame looked wrong.
    pub encoder: String,
    /// Which display the process starts on (DXGI output ordinal, 0 = primary).
    ///
    /// A per-session request (`SETTINGS.displayId`) wins over this; the flag is
    /// what a headless/kiosk host uses when the screen it wants is not the
    /// primary one. Asking for an index that does not exist is a startup-time
    /// capture failure with a clear log, not a silent fallback to the primary —
    /// serving the wrong screen is worse than serving none.
    pub display: u32,
    /// #105: who may inject input into the shared program.
    ///
    /// Default [`InputPolicy::Shared`] — every attached session may type and
    /// move the pointer. This is not a relaxation of security, it is a
    /// correction of a wrong default: upstream selkies has no exclusive input
    /// at all, and the only deployments that exist today are one operator with
    /// one or two tabs on the same machine. Gating the second tab made "open
    /// the page twice" look like "input is broken" (denied_inputs counted
    /// hundreds of refused pointer moves per minute) while buying nothing:
    /// Windows can only isolate input per *session*, never per window, so a
    /// real second user needs a second OS session, not a lease.
    ///
    /// [`InputPolicy::Exclusive`] restores the single-controller behaviour for
    /// the day a program really does serve several people.
    pub input_policy: InputPolicy,
    /// #102: ceiling on live signaling sessions.
    ///
    /// Eight is not a capacity claim — it is the point past which an unbounded
    /// fan-out on this class of host stops protecting the people already
    /// watching. One shared encoder means every extra viewer costs a
    /// packetizer pass and a full copy of the bitrate, so the limit exists to
    /// keep a late joiner's failure *polite* (a 503 with `Retry-After`) instead
    /// of degrading everyone's picture.
    pub max_sessions: usize,
    /// #109: 麦克风往哪儿写 —— `auto`（默认，按优先级挑一台虚拟线缆）、`default`
    /// （写默认扬声器，只服务对讲且必定自听）、`steam` / `vb-cable` /
    /// `voicemeeter` / `parsec`，或者一个具体的端点名（`Line 1`）。
    ///
    /// 默认不再是"写默认扬声器"：那条路会把注入的声音交给下行的 loopback 采回来，
    /// 开麦就听到自己，而且改延迟、改码率都消不掉。见 `platform::virtual_mic`。
    pub mic_device: String,
    /// P1: 要不要把系统默认**录音**设备切到虚拟麦 —— `auto`（默认）、`off`。
    ///
    /// `auto` 在启动时切换（游戏在启动时读默认设备并缓存，会话开始才切已经晚了），
    /// 退出时还原；`off` 完全不碰系统设置，玩家自己在游戏里选麦克风。三条 role
    /// （console / multimedia / communications）都会切，否则走通信角色的
    /// Discord / Steam 语音仍指着物理麦。见 `platform::default_mic`。
    pub mic_default_device: String,
    /// #95: realtime knob set for the software encoder — `tuned` (default) or
    /// `legacy` (exactly what shipped before #95).
    ///
    /// Diagnostic, not a feature: it exists so the same binary can produce the
    /// old numbers on the same machine, which is the only way to tell "the
    /// tuning helped" from "the machine was busier". Live encoders keep what
    /// they were opened with. See `pipeline::encoder::tuning`.
    pub tune: String,

    /// 游戏托管：本进程要不要负责"把游戏客户端拉起来并看着它"。
    ///
    /// 关掉时所有游戏端点都答 404（不是 200 + 空状态）：一个没启用游戏托管的
    /// 部署里，`POST /api/v1/control/game/start` 成功返回会让人以为游戏在起，
    /// 而屏幕上什么都不会发生。见 [`crate::game`]。
    pub game_enabled: bool,

    /// 游戏托管：默认实例 id —— 对应 `<game_root>\instances\<id>.json`。
    ///
    /// 实例而不是路径：一台机器可以有多个客户端/多个存档，切换的动作应该是
    /// "改一个字段"，不是"删了重配"。命令行 `--game-instance` 在单次启动里赢。
    pub game_instance: String,

    /// 游戏托管：`games` 根目录。
    ///
    /// **默认相对 exe 所在目录**（[`crate::run::root`]），不是相对工作目录 ——
    /// 计划任务拉起时 cwd 由任务本身决定，谁也不能保证。`dist` 当初选相对
    /// cwd 是因为它跟着包走；这里选了另一半：游戏目录里有存档，不能因为
    /// 某人从别的目录敲了一次 exe 就"找不到世界了"。
    pub game_root: Option<PathBuf>,

    /// 游戏托管：服务起来时顺手把游戏也拉起来。
    ///
    /// 默认关。默认开会让"只想看串流、不想动桌面"的部署凭空多一个全屏窗口，
    /// 而这个窗口正好盖住要串流的桌面 —— 那是一种很贵的惊喜。
    pub game_autostart: bool,

    /// 一次性动作：把生效的游戏实例与拼出来的启动命令行打到 stdout，然后退出。
    ///
    /// 与 `--print-config` 同一个理由：三十二个参数 + 一份实例 json 拼出来的
    /// 那一条命令行，没人能凭记忆复述。运维要能"看一眼，然后复制到别处比对"。
    /// 刻意不进 `--print-config` 的输出（那是配置本体），也不从文件读。
    pub print_game: bool,

    /// 一次性动作：把「这台机器上有哪些 JRE、版本各是几、会选哪一个」打到 stdout。
    ///
    /// 与 `--print-game` 分开，是因为它回答的是**另一个**问题：`--print-game` 说的是
    /// "要跑什么"，这个说的是"拿什么跑"。JRE 的问题全发生在"服务起得来、游戏起不来"
    /// 那一层，那时候人通常只能在串流页面外看一个黑屏 —— 这个开关能在任何一台机器
    /// 上离线跑，且**实例文件坏了也照样输出**。
    pub print_java: bool,

    /// 输入锁定：把"从网页进来的输入"关在游戏里，不让它切走桌面。
    ///
    /// 默认关。理由和 `game_autostart` 一样是"别让默认配置改变一台机器的行为"：
    /// 一个只想串流桌面的部署被默默锁上键盘，是运维最不想遇到的那种惊喜。
    /// 但**只要开了游戏托管，它几乎总是该开**——全屏的 MC 一失去焦点就自己
    /// 最小化，而串流采的是整屏。见 [`crate::kiosk`]。
    pub kiosk: bool,

    /// 游戏启动后自动上锁。
    ///
    /// 与 `kiosk` 分开是有意的：手工锁（面板上点一下）与自动锁的风险完全不同。
    /// 自动锁意味着"玩家点开始游戏 → 桌面立刻被锁 → 只有口令或 SSH 能解"，
    /// 这一步必须有人明确打开。
    pub kiosk_autolock: bool,

    /// 用注册表策略关掉 `Win` 组合与任务管理器（四档里的第 1 档）。
    ///
    /// 默认开。它只影响当前用户、改回来只要删一个值，而且是**唯一**能挡住
    /// `Win+D` 这类"钩子看不见"的系统快捷键的一层。
    pub kiosk_policy: bool,

    /// 安装低级键盘钩子（第 2 档）。
    ///
    /// 默认开。留 `--no-kiosk-hook` 是为了排障：钩子出问题时表现是"键盘全没
    /// 反应"，而关掉它就能一步区分"是钩子吃的"还是"注入端吃的"。
    pub kiosk_hook: bool,

    /// 网页解锁用的口令哈希（`sha256i:<轮数>:<盐>:<哈希>`）。
    ///
    /// 存哈希不存口令：运维拷一份 `stream-config.toml` 去排障、贴进聊天窗口，
    /// 里面的字串不该等于口令。生成方式见 `--kiosk-hash`。
    pub kiosk_password: Option<String>,

    /// 一次性动作：算出口令哈希后退出。
    ///
    /// 明文从 `--kiosk-hash <口令>` 或 stdin（`echo 口令 | exe --kiosk-hash`）来。
    /// 两条路都留着，因为它们的代价不同：参数会被 shell 历史记下，管道不会，
    /// 但管道在 Windows 的 GUI 子系统下要绕一层重定向 —— 运维手边有什么就用
    /// 什么，比"只提供一种正确姿势"更可能被照着做完。
    pub kiosk_hash: bool,

    /// `--kiosk-hash` 的明文。只在那一个一次性动作里有意义，不落盘、不打印。
    pub kiosk_hash_plain: Option<String>,

    /// 一次性动作：解除本机的输入锁定，然后退出。
    ///
    /// 这是"救命通道"：SSH 进去跑一条命令就能解锁。它不校验口令 —— 能在这台
    /// 机器上执行任意命令的人本来就有更高权限，再要一次口令只是仪式。
    pub unlock: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_owned(),
            port: 8080,
            // Relative on purpose: the runtime directory is wherever the deploy
            // bundle was unpacked, and the launcher `cd`s into it before starting
            // this process. A baked `C:\dev\selkies-web` would make the binary
            // serve a 404-less empty root on any machine that keeps the bundle
            // somewhere else — the kind of failure that looks like a broken build.
            dist: PathBuf::from("selkies-web"),
            app_name: "webrtc".to_owned(),
            // #117: WebSockets is the first load now. It needs one TCP port
            // (the one already serving the page) instead of a UDP range, so a
            // fresh browser on a strange network gets a picture without any
            // port forwarding first — the case every earlier deployment had to
            // discover the hard way. WebRTC is still a click away in the panel,
            // and `--mode webrtc` restores the old default for a deployment
            // that wants NACK/PLI/FEC from the very first frame.
            default_mode: "websockets".to_owned(),
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate_kbps: 2500,
            tls_cert: None,
            tls_key: None,
            // Stage1 decision (design 9.2 #5): file log lands under logs\ relative
            // to the working directory; rotation is deferred to stage4 serviceization.
            log_file: Some(PathBuf::from(r"logs\stream-server.log")),
            no_hdr: false,
            restore_mic: false,
            print_config: None,
            config_source: None,
            dump_frame: None,
            repeat_every: 2,
            keyframe_secs: 10,
            upload_dir: None,
            upload_quota_mb: 512,
            // #117: VP9 is the codec a fresh process opens with. The panel's
            // encoder row echoes `s.codec().encoder_id()` — the codec actually
            // running — so this default is also what that row says before
            // anyone touches it; leaving VP8 here meant the menu read "VP8" on
            // a clean profile while the statistics tile promised a modern
            // codec. `--video-codec vp8` is the way back: VP9 costs noticeably
            // more CPU per frame on the software encoder, which matters on a
            // small host asked for 1080p60.
            video_codec: "vp9".to_owned(),
            // #122: pinned so the firewall rule can describe the range. The
            // library default is ":0" (a different random port per session),
            // which no firewall rule can follow -- see the field docs.
            webrtc_udp_port_min: 40000,
            webrtc_udp_port_max: 40049,
            ghost_cursor_idle_secs: crate::dispatch::ghost::DEFAULT_IDLE_SECS,
            pointer_recenter: false,
            policy: "auto".to_owned(),
            auth_token: None,
            auth_grants: Vec::new(),
            mc_report: false,
            mc_report_url: None,
            mc_report_key: None,
            mc_report_ip: None,
            display: 0,
            encoder: "auto".to_owned(),
            tune: "tuned".to_owned(),
            input_policy: InputPolicy::default(),
            max_sessions: 8,
            mic_device: "auto".to_owned(),
            mic_default_device: "auto".to_owned(),
            game_enabled: false,
            game_instance: "default".to_owned(),
            game_root: None,
            game_autostart: false,
            print_game: false,
            print_java: false,
            // 锁定默认关，但"关"的是**行为**不是**能力**：策略与钩子两层的默认
            // 值是开，`kiosk` 打开之后立刻是完整的 2 档锁定，不需要再记两个开关。
            kiosk: false,
            kiosk_autolock: false,
            kiosk_policy: true,
            kiosk_hook: true,
            kiosk_password: None,
            kiosk_hash: false,
            kiosk_hash_plain: None,
            unlock: false,
        }
    }
}

impl Config {
    /// #122: 生效的 ICE UDP 端口区间（含两端）；`None` 表示交给库的默认
    /// （每个会话随机一个端口，任何防火墙规则都跟不上）。
    ///
    /// 判定只做两件事：低于 1024 的端口要特权，部署包是普通用户会话里起的，
    /// 绑不上；`max < min` 是一个必然让每个候选都失败的区间 —— 那种情况下
    /// 退回随机端口比"HOST 候选一个都出不来"更容易查。
    pub fn webrtc_udp_ports(&self) -> Option<(u16, u16)> {
        let (min, max) = (self.webrtc_udp_port_min, self.webrtc_udp_port_max);
        (min >= 1024 && max >= min).then_some((min, max))
    }

    /// #110: 这次部署的门。
    ///
    /// `--auth-token` 与若干条 `--auth-grant` 合并成一张令牌表：前者是后者的
    /// 单条特例（默认租户的主机级身份），所以老部署只写 `--auth-token` 时行为
    /// 不变。两者都没配时返回 `None` —— 不鉴权，和以前一致。
    pub fn token_gate(&self) -> Option<crate::security::TokenGate> {
        let mut grants: Vec<(String, crate::security::token::Grant)> = Vec::new();
        if let Some(token) = self.auth_token.as_deref() {
            grants.push((
                token.to_owned(),
                crate::security::token::Grant::new(crate::security::TenantId::DEFAULT, crate::security::Role::Host),
            ));
        }
        grants.extend(self.auth_grants.iter().map(|spec| spec.to_grant()));
        crate::security::TokenGate::from_grants(grants)
    }

    /// Parse `argv` after the program name. Unknown flags keep the default and
    /// are logged once; a non-numeric value for a numeric flag keeps the
    /// default rather than aborting startup.
    ///
    /// 配置文件先应用、命令行后应用，于是**命令行永远赢**。文件本身有问题时
    /// 返回 [`ConfigError`] 而不是继续：那是运维写的输入错了，带着半份配置起
    /// 服务只会让人以为"我改的生效了"。
    ///
    /// 缺省位置（exe 旁边的 `config.toml`）是**可选**的 —— 老部署里没有这个
    /// 文件，行为必须一个字节都不变，这是整个模块能被安全引入的前提。
    pub fn parse(argv: Vec<String>) -> Result<Self, ConfigError> {
        let mut cfg = Self::default();

        // 第一遍只找 `--config`。文件是优先级最低的一层，必须先于任何命令行
        // 参数应用；边扫边用会让 `--config` 写在末尾时，前面的参数悄悄输给文件。
        let mut explicit: Option<PathBuf> = None;
        {
            let mut probe = argv.iter();
            while let Some(arg) = probe.next() {
                if arg == "--config" {
                    explicit = probe.next().map(PathBuf::from);
                }
            }
        }

        // 口令的老位置：`auth-token.txt`。
        //
        // 它排在配置文件**之前**（后面 `file.apply` 会覆盖它），也就是优先级
        // 最低的一层。2026-11 之前这文件是唯一的来源，由 `--run` 读出来拼成
        // `--auth-token`；那样做的后果是命令行压过配置文件 —— 有人在界面上
        // 改了口令、重启却没生效，而任何日志里都看不出来为什么。
        //
        // 现在反过来：配置文件里的 `auth_token` 赢，txt 只在配置没写这一项时
        // 兜底。老机器升级上来行为不变，界面一设就以界面为准。
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
        {
            if let Some(legacy) = crate::configfile::read_legacy_token(&dir) {
                cfg.auth_token = Some(legacy);
            }
        }

        let from_default_location = explicit.is_none();
        // `resolve` 而不是 `default_path`：整包分发里那份叫 `stream-config.toml`，
        // 而 `default_path` 只认 `config.toml`。用后者的话，**不带任何参数**双击
        // exe（或计划任务漏了 `--run`）会读不到配置、静默跑在 1080p30 默认值上
        // —— 一个"看起来正常、实际忽略了每一处调整"的故障。`--run` 与 `--ops`
        // 用的是同一份判断，三处合一。
        if let Some(path) = explicit.or_else(|| crate::configfile::resolve(None)) {
            // 显式 `--config` 是"我说了有这么一份"，找不到就是错；默认位置只是
            // 顺手看一眼，不存在属于常态。
            if !from_default_location || path.exists() {
                let file = crate::configfile::load(&path).map_err(ConfigError)?;
                let mut warnings = Vec::new();
                file.apply(&mut cfg, &mut warnings);
                // 用 eprintln 而不是 log：此刻 logger 还没初始化（它在 app::run
                // 里、`parse` 之后才建），`log::warn!` 会直接消失 —— 而这条恰恰
                // 是"我改了配置却没生效"的唯一线索。
                for warning in warnings {
                    eprintln!("[config] {warning}");
                }
                cfg.config_source = Some(path);
            }
        }

        let mut iter = argv.into_iter().peekable();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--host" => {
                    if let Some(v) = iter.next() {
                        cfg.host = v;
                    }
                }
                "--port" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u16>() {
                            cfg.port = n;
                        }
                    }
                }
                "--dist" => {
                    if let Some(v) = iter.next() {
                        cfg.dist = PathBuf::from(v);
                    }
                }
                "--app" => {
                    if let Some(v) = iter.next() {
                        cfg.app_name = v;
                    }
                }
                // #106: which transport a fresh browser starts on. Only the two
                // real ones are accepted — a typo must not leave the page in a
                // mode that has no core behind it, so it keeps the default and
                // says so.
                "--mode" => {
                    if let Some(v) = iter.next() {
                        match v.trim().to_ascii_lowercase().as_str() {
                            "webrtc" | "websockets" => cfg.default_mode = v.trim().to_ascii_lowercase(),
                            other => eprintln!(
                                "unknown --mode {other:?}, keeping {:?} (webrtc, websockets)",
                                cfg.default_mode
                            ),
                        }
                    }
                }
                "--width" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u32>() {
                            cfg.width = n;
                        }
                    }
                }
                "--height" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u32>() {
                            cfg.height = n;
                        }
                    }
                }
                "--fps" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u32>() {
                            cfg.fps = n;
                        }
                    }
                }
                "--bitrate" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u32>() {
                            cfg.bitrate_kbps = n;
                        }
                    }
                }
                "--tls-cert" => {
                    if let Some(v) = iter.next() {
                        cfg.tls_cert = Some(PathBuf::from(v));
                    }
                }
                "--tls-key" => {
                    if let Some(v) = iter.next() {
                        cfg.tls_key = Some(PathBuf::from(v));
                    }
                }
                "--log" => {
                    if let Some(v) = iter.next() {
                        if v.is_empty() {
                            cfg.log_file = None;
                        } else {
                            cfg.log_file = Some(PathBuf::from(v));
                        }
                    }
                }
                "--repeat-every" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u32>() {
                            cfg.repeat_every = n.clamp(1, 10);
                        }
                    }
                }
                "--keyframe-secs" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u32>() {
                            cfg.keyframe_secs = n.clamp(1, 120);
                        }
                    }
                }
                "--no-hdr" => cfg.no_hdr = true,
                // 一键卸载专用：还原默认录音设备后立即退出（见字段注释）。
                // 单独一个开关而不是复用 `--mic-default-device off`，是因为
                // 后者只决定"启动时要不要接管"，仍然会把整个服务拉起来。
                "--restore-mic" => cfg.restore_mic = true,
                "--video-codec" => {
                    if let Some(v) = iter.next() {
                        // 未知值保留默认：本项目的默认是 VP8，拼错不该变成
                        // "启动失败"，但也别静默接受 —— main 里会记一条 warn。
                        cfg.video_codec = v;
                    }
                }
                // #122: ICE 的 UDP 端口区间。两个都写才生效，且必须
                // `min >= 1024`、`max >= min`；不成立时 `webrtc_udp_ports()`
                // 返回 `None`，main 会记一条 warn（见该方法的注释：宁可退回
                // 随机端口，也不要给 ICE 一个绑不上的区间）。
                "--webrtc-udp-port-min" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u16>() {
                            cfg.webrtc_udp_port_min = n;
                        }
                    }
                }
                "--webrtc-udp-port-max" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u16>() {
                            cfg.webrtc_udp_port_max = n;
                        }
                    }
                }
                // #96: optional, like the two above — the default (5 s) is what
                // almost every host wants, so a launch script need not carry it.
                "--ghost-cursor-idle" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u64>() {
                            cfg.ghost_cursor_idle_secs = crate::dispatch::ghost::clamped(n);
                        }
                    }
                }
                // #124/B: 只在需要时才动真光标，所以是"出现即开"的开关，没有
                // `--pointer-recenter 0` 这种写法 —— 配置文件里才有显式的 false。
                "--pointer-recenter" => cfg.pointer_recenter = true,
                "--policy" => {
                    if let Some(v) = iter.next() {
                        // 未知名保留默认并在 main 里告警，不在这里吞掉 ——
                        // "策略名拼错了" 必须看得见，否则会以为鉴权已经生效。
                        cfg.policy = v;
                    }
                }
                // #101: 空串等于关掉（`--auth-token ""` 不该把机器锁死），
                // 与 `TokenGate::new` 的判定是同一套规则。
                "--auth-token" => {
                    if let Some(v) = iter.next() {
                        cfg.auth_token = if v.trim().is_empty() { None } else { Some(v) };
                    }
                }
                // #110: `--auth-grant <tenant>:<role>:<token>`，可重复。
                // 拼错只拒绝这一条并说明，不拖垮启动 —— 与其它参数同一套取舍。
                "--auth-grant" => {
                    if let Some(v) = iter.next() {
                        match crate::security::token::GrantSpec::parse(&v) {
                            Ok(spec) => cfg.auth_grants.push(spec),
                            Err(err) => eprintln!("--auth-grant rejected: {err}"),
                        }
                    }
                }
                // #95: 后端策略名。未知值保留 `auto` 并在 main 里告警 —— 与
                // `--video-codec` 同一套取舍：拼错不该让服务起不来，但要看得见。
                "--display" => {
                    if let Some(v) = iter.next() {
                        match v.trim().parse::<u32>() {
                            Ok(n) => cfg.display = n,
                            Err(_) => log::warn!(
                                "unknown --display {v} (expected a DXGI output index), keeping {}",
                                cfg.display
                            ),
                        }
                    }
                }
                "--encoder" => {
                    if let Some(v) = iter.next() {
                        cfg.encoder = v;
                    }
                }
                // #95: 软编参数组。`legacy` 只为同一台机器上复现旧数据而存在，
                // 默认组才是要发布的那一套。
                "--tune" => {
                    if let Some(v) = iter.next() {
                        cfg.tune = v;
                    }
                }
                // #102: silently keeping the default on a non-numeric value is
                // the same policy as every other numeric flag here — a typo in a
                // launcher must not stop the host from coming up. 0 is clamped to
                // 1 by `Admission::new` rather than accepted as "lock everyone
                // out", which is never what an operator meant.
                "--max-sessions" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<usize>() {
                            cfg.max_sessions = n;
                        }
                    }
                }
                // #105: 输入策略。默认 `shared`（谁能看见谁就能操作），
                // `exclusive` 恢复单主模式。拼错保留默认并告警，与 `--encoder`
                // 一致：启动参数写错不该让被控机起不来。
                "--input-policy" => {
                    if let Some(v) = iter.next() {
                        match InputPolicy::parse(&v) {
                            Some(policy) => cfg.input_policy = policy,
                            None => log::warn!(
                                "unknown --input-policy {v} (expected shared|exclusive), keeping {}",
                                cfg.input_policy.as_str()
                            ),
                        }
                    }
                }
                "--mic-device" => {
                    // #109：不做白名单校验。除了四个家族名，这里还接受任意端点名
                    // （Virtual Audio Cable 的 `Line 1` 之类），合法性由探测那一侧
                    // 判断——启动参数写错不该让被控机起不来，与 `--encoder` 一致。
                    if let Some(v) = iter.next() {
                        if !v.is_empty() {
                            cfg.mic_device = v;
                        }
                    }
                }
                "--mic-default-device" => {
                    // P1：白名单校验，但写错只告警不退出 —— 与 `--input-policy`
                    // 同一条规则（启动参数写错不该让被控机起不来）。
                    match iter.next() {
                        Some(v) => match crate::platform::default_mic::Policy::from_name(&v) {
                            Some(p) => cfg.mic_default_device = p.as_str().to_owned(),
                            None => log::warn!(
                                "unknown --mic-default-device {v:?} (expected auto|off), keeping {}",
                                cfg.mic_default_device
                            ),
                        },
                        None => log::warn!("--mic-default-device needs a value (auto|off)"),
                    }
                }
                "--upload-dir" => {
                    if let Some(v) = iter.next() {
                        if v.is_empty() {
                            cfg.upload_dir = None;
                        } else {
                            cfg.upload_dir = Some(PathBuf::from(v));
                        }
                    }
                }
                // #110: 每租户配额（MiB）。`0` 等于不限 —— 与其它数值参数同一
                // 套取舍：解析不了就保留默认并说一句，不拖垮启动。
                "--upload-quota-mb" => {
                    if let Some(v) = iter.next() {
                        match v.trim().parse::<u64>() {
                            Ok(n) => cfg.upload_quota_mb = n,
                            Err(_) => log::warn!(
                                "unknown --upload-quota-mb {v} (expected MiB, 0 = unlimited), keeping {}",
                                cfg.upload_quota_mb
                            ),
                        }
                    }
                }
                "--dump-frame" => {
                    if let Some(v) = iter.next() {
                        if !v.is_empty() {
                            cfg.dump_frame = Some(PathBuf::from(v));
                        }
                    }
                }
                // 游戏托管。四个开关都刻意做成"`--game` 开、`--no-game` 关"以外
                // 的单值形式：游戏是一个**有状态**的对象（进程、窗口、存档），
                // 拼错一个字母时"什么都没发生"比"起了一个不该起的窗口"好排查。
                "--game" => cfg.game_enabled = true,
                "--no-game" => cfg.game_enabled = false,
                "--game-instance" => {
                    if let Some(v) = iter.next() {
                        if v.trim().is_empty() {
                            log::warn!("--game-instance needs a non-empty id");
                        } else {
                            cfg.game_instance = v.trim().to_owned();
                        }
                    }
                }
                "--game-root" => {
                    if let Some(v) = iter.next() {
                        if v.is_empty() {
                            cfg.game_root = None;
                        } else {
                            cfg.game_root = Some(PathBuf::from(v));
                        }
                    }
                }
                "--game-autostart" => cfg.game_autostart = true,
                "--no-game-autostart" => cfg.game_autostart = false,
                "--print-game" => cfg.print_game = true,
                "--print-java" => cfg.print_java = true,
                "--kiosk" => cfg.kiosk = true,
                "--no-kiosk" => cfg.kiosk = false,
                "--kiosk-autolock" => cfg.kiosk_autolock = true,
                "--no-kiosk-autolock" => cfg.kiosk_autolock = false,
                "--kiosk-policy" => cfg.kiosk_policy = true,
                "--no-kiosk-policy" => cfg.kiosk_policy = false,
                "--kiosk-hook" => cfg.kiosk_hook = true,
                "--no-kiosk-hook" => cfg.kiosk_hook = false,
                "--kiosk-password" => {
                    if let Some(v) = iter.next() {
                        let v = v.trim();
                        cfg.kiosk_password = if v.is_empty() { None } else { Some(v.to_owned()) };
                    }
                }
                "--kiosk-hash" => {
                    cfg.kiosk_hash = true;
                    // 可选值：下一个 token 不是开关就当作明文口令。
                    if let Some(v) = iter.peek() {
                        if !v.starts_with('-') {
                            cfg.kiosk_hash_plain = Some((*v).clone());
                            let _ = iter.next();
                        }
                    }
                }
                "--unlock" => cfg.unlock = true,
                // 值已经在第一遍里读过并应用了，这里只是把它吞掉 —— 否则下面
                // 的 `other` 分支会把 `--config` 报成未知参数，而它明明生效了。
                "--config" => {
                    let _ = iter.next();
                }
                // 可选值：`--print-config` 或 `--print-config json`。用 peek
                // 而不是无条件吞下一个 token，否则 `--print-config --port 9000`
                // 会把 `--port` 吃掉，而那个参数本该生效。
                "--print-config" => {
                    let format = match iter.peek().map(|s| s.as_str()) {
                        Some("json") => {
                            let _ = iter.next();
                            "json"
                        }
                        Some("toml") => {
                            let _ = iter.next();
                            "toml"
                        }
                        _ => "toml",
                    };
                    cfg.print_config = Some(format.to_owned());
                }
                other => {
                    log::warn!("unknown CLI argument ignored: {other}");
                }
            }
        }
        Ok(cfg)
    }

    /// Spacing between pipeline beats, per the stage1 decision: duration is
    /// always exactly spf; repeat frames re-use that interval instead of a
    /// shorter or longer one (design 9.2 #4).
    pub fn spf(&self) -> std::time::Duration {
        let fps = self.fps.max(1) as f64;
        std::time::Duration::from_secs_f64(1.0 / fps)
    }

    /// True when both certificate and key paths are present.
    pub fn tls_enabled(&self) -> bool {
        self.tls_cert.is_some() && self.tls_key.is_some()
    }

    /// #86: wall-clock spacing between periodic keyframes. Clamped again here
    /// because `Config` can also be built as a struct literal (tests, benches),
    /// where `parse` never ran the clamp.
    pub fn keyframe_period(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.keyframe_secs.clamp(1, 120) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Config {
        Config::parse(args.iter().map(|s| (*s).to_owned()).collect())
            .expect("测试用的命令行应当总能解析")
    }

    /// The default dist must stay portable. A drive-qualified default would only
    /// work on the machine it was written on: the bundle is meant to be unpacked
    /// anywhere, and the launcher already `cd`s into the unpacked directory, so
    /// a bare relative name is the one form that is correct everywhere.
    #[test]
    fn the_default_dist_is_relative_to_the_working_directory() {
        let dist = parse(&[]).dist;
        assert_eq!(dist, PathBuf::from("selkies-web"));
        assert!(!dist.is_absolute(), "dist default must not be absolute");
        assert_eq!(dist.components().count(), 1, "no parent hops either");
    }

    #[test]
    fn keyframe_secs_defaults_to_ten_seconds() {
        let cfg = parse(&[]);
        assert_eq!(cfg.keyframe_secs, 10);
        assert_eq!(cfg.keyframe_period(), std::time::Duration::from_secs(10));
    }

    #[test]
    fn keyframe_secs_reads_the_flag() {
        assert_eq!(parse(&["--keyframe-secs", "4"]).keyframe_period().as_secs(), 4);
    }

    #[test]
    fn keyframe_secs_is_clamped_to_one_second_upwards() {
        // 0 would mean "force a keyframe on every beat" — one IDR per frame eats
        // the whole budget on a moving picture, so clamp instead of trusting it.
        assert_eq!(parse(&["--keyframe-secs", "0"]).keyframe_period().as_secs(), 1);
    }

    #[test]
    fn keyframe_secs_is_clamped_to_two_minutes_downwards() {
        // The other end matters too: past ~2 min the reference chain drifts
        // (the #53 quantizer-drift finding) and a late joiner waits for a PLI.
        assert_eq!(
            parse(&["--keyframe-secs", "999"]).keyframe_period().as_secs(),
            120
        );
    }

    #[test]
    fn a_bad_keyframe_value_keeps_the_default() {
        assert_eq!(parse(&["--keyframe-secs", "soon"]).keyframe_secs, 10);
    }

    /// 一键卸载的开关：默认关（正常启动不该走还原分支），带上才提前退出。
    ///
    /// 这条测试守的是"默认值"那一侧 —— 如果它变成 true，每个被控机启动时
    /// 都会先还一次默认录音设备再正常起，等于把 P1 的接管关掉了。
    #[test]
    fn restore_mic_is_off_unless_asked_for() {
        assert!(!parse(&[]).restore_mic);
        assert!(parse(&["--restore-mic"]).restore_mic);
        // 与真正的启动参数并存时仍然是它说了算（卸载脚本会带上 --log）。
        assert!(parse(&["--log", r"D:\s\logs\stream-server.log", "--restore-mic"]).restore_mic);
    }

    /// #96: the ghost pointer's fade time is optional on purpose — the default
    /// is what almost every host wants, so a launch script need not carry it.
    #[test]
    fn ghost_cursor_idle_defaults_to_five_seconds() {
        assert_eq!(parse(&[]).ghost_cursor_idle_secs, 5);
    }

    /// #117: 首屏默认是 WebSocket + VP9，这是一条产品决定，不是随手写的初值。
    ///
    /// 理由分开看：WebSocket 只占页面已经在用的那个 TCP 端口，新机器不必先
    /// 放通 UDP 段就能出画面；VP9 是面板编码器行会被回显的那个值（`encoder`
    /// 报的是**正在跑**的编码器），默认留在 VP8 会让干净配置的菜单写着 VP8。
    /// 两个旗标仍然能覆盖它们 —— 默认值是起点，不是锁。
    #[test]
    fn websockets_and_vp9_are_the_first_load_defaults() {
        let cfg = parse(&[]);
        assert_eq!(cfg.default_mode, "websockets");
        assert_eq!(cfg.video_codec, "vp9");

        let flipped = parse(&["--mode", "webrtc", "--video-codec", "vp8"]);
        assert_eq!(flipped.default_mode, "webrtc");
        assert_eq!(flipped.video_codec, "vp8");
    }

    /// #122: 默认区间就是防火墙里那条 UDP 规则要放行的区间（`--inbound` 建它）。
    ///
    /// 这两处必须一致 —— 它们描述的是同一件事，只是分别给 Windows 防火墙和
    /// webrtc 看。不一致的症状很隐蔽：ICE 的候选绑在 40500，规则放行的是
    /// 40000–40049，浏览器照样"媒体路径失败"，而脚本会打印"规则已在"。
    #[test]
    fn webrtc_udp_ports_default_to_the_range_the_firewall_rule_opens() {
        assert_eq!(parse(&[]).webrtc_udp_ports(), Some((40000, 40049)));
    }

    /// 区间反向 / 低于特权端口时退回随机端口，而不是把一个必然失败的区间
    /// 交给 ICE —— 那种失败是"所有 HOST 候选都绑不上"，比随机端口更难查。
    #[test]
    fn webrtc_udp_ports_read_the_flags_and_reject_a_broken_range() {
        let cfg = parse(&[
            "--webrtc-udp-port-min",
            "41000",
            "--webrtc-udp-port-max",
            "41009",
        ]);
        assert_eq!(cfg.webrtc_udp_ports(), Some((41000, 41009)));

        let inverted = parse(&[
            "--webrtc-udp-port-min",
            "41009",
            "--webrtc-udp-port-max",
            "41000",
        ]);
        assert_eq!(inverted.webrtc_udp_ports(), None);

        let privileged = parse(&["--webrtc-udp-port-min", "80"]);
        assert_eq!(privileged.webrtc_udp_ports(), None);
    }

    /// A value outside the range is clamped rather than announced: 0 would read
    /// as "never fade" on the client, which is the behavior the knob exists to
    /// remove.
    #[test]
    fn ghost_cursor_idle_reads_the_flag_and_clamps_it() {
        assert_eq!(parse(&["--ghost-cursor-idle", "8"]).ghost_cursor_idle_secs, 8);
        assert_eq!(parse(&["--ghost-cursor-idle", "0"]).ghost_cursor_idle_secs, 1);
        assert_eq!(
            parse(&["--ghost-cursor-idle", "99999"]).ghost_cursor_idle_secs,
            600
        );
        assert_eq!(
            parse(&["--ghost-cursor-idle", "soon"]).ghost_cursor_idle_secs,
            5,
            "a value that is not a number keeps the default"
        );
    }

    /// #124/B: 默认关闭，出现即开。它动的是被控机的真光标，所以"不说就是不动"
    /// 是唯一能接受的默认 —— 关掉的人不需要一个 `--no-pointer-recenter`，因为
    /// 不写这个参数就是关着的。
    #[test]
    fn pointer_recentering_is_off_unless_asked_for() {
        assert!(!parse(&[]).pointer_recenter);
        assert!(parse(&["--pointer-recenter"]).pointer_recenter);
        // 位置无关：它是个开关，不该因为写在别的参数前后而失效。
        assert!(
            parse(&["--port", "9000", "--pointer-recenter", "--fps", "60"]).pointer_recenter
        );
    }
}
