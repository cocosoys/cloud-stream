//! 监听面（surface）鉴权骨架 —— #88。
//!
//! 这个模块只做三件事，不含任何业务判断：
//!
//! 1. **给每个"能被外部触发的东西"一个稳定的名字**（[`EventKind`]）。名字是
//!    日志里可以直接 grep 的标签，也是未来策略里 switch 的键。
//! 2. **给触发者一个身份对象**（[`Principal`]）。当前字段很少 —— 只有 peer
//!    地址、会话号和凭据种类 —— 因为本项目的鉴权（2.5）还没做。留出结构，
//!    未来补 token 时不用改调用点。
//! 3. **给每个入口一次裁决**（[`check`]）。默认策略 [`policy::AllowAll`] 全部
//!    放行，所以插桩本身不改变现有行为；换成 [`policy::DenyAll`] 就能验证
//!    "哪些入口真的接上了骨架" —— 漏插桩的入口会照常工作，这正是它的用途。
//!
//! 设计上刻意**不**把策略做成可配置脚本或动态库：本项目只需要骨架，具体
//! 逻辑（谁能看画面、谁能动键鼠、谁能传文件）属于 webgame 等上层项目，
//! 它们要做的是实现 [`Policy`] 并在启动时 [`install`]。
//!
//! 一个约定：**插桩点必须在副作用之前**。检查放在 `match` 之后、执行之前，
//! 而不是执行之后补日志 —— 后者的"拒绝"没有意义。

use std::fmt;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

pub mod policy;

pub use policy::{AllowAll, DenyAll};

/// 外部可达的入口分类。
///
/// 变体带子类型而不是全部拍平，是因为策略的粒度就在这一层：真实需求几乎
/// 总是"键盘可以、文件不行"而不是"输入全部拒绝"。`Copy` 让它可以随手放进
/// `match` 与日志参数里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// 进程启动与关闭（`main` 的装配阶段）。
    Startup,
    /// HTTP 请求。端点见 [`Endpoint`]。
    Http(Endpoint),
    /// WebSocket 信令：`/api/<app>/signaling/` 的升级、SDP、ICE。
    Signaling,
    /// 控制通道上的非输入指令：设置、码率、统计开关、指针渲染交接。
    Control(&'static str),
    /// 输入注入，按设备细分。
    Input(Device),
    /// 剪贴板，按方向细分。
    Clipboard(ClipOp),
    /// 媒体，按方向细分。
    Media(MediaDir),
    /// 文件系统读写。
    File(FileOp),
    /// 诊断与观测（统计推送、状态查询）。
    Diagnostics(&'static str),
}

/// HTTP 端点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Endpoint {
    /// `GET /api/status` —— 纯文本 `ok`。
    Status,
    /// `POST /api/upload` —— 分片上传。
    Upload,
    /// `GET /api/files/<rel>` —— 目录列表与下载。
    Files,
    /// 静态资源：页面、bundle、jsdb、codec。
    Static,
    /// 信令升级路径（真正的裁决在 [`EventKind::Signaling`]）。
    Signaling,
    /// #93: `GET /api/v1/health` —— 主机 / 推流 / 进程 / 连接的全量报告。
    ///
    /// 单独立一个端点而不是复用 [`Endpoint::Status`]，是因为两者暴露的东西
    /// 差了一个量级：`status` 只有 `ok` 两个字，这个端点会报出谁连着、从哪个
    /// IP、什么设备码。将来装真策略时，"能探活"和"能看连接清单"完全可以是
    /// 两个不同的权限。
    Health,
    /// #93: `POST /api/v1/control/<action>` —— 少量控制（重建采集器、踢会话）。
    Control,
    /// `/api/` 下的未知路径。
    NotFound,
}

/// 输入设备。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Device {
    Keyboard,
    Pointer,
    Touch,
    Gamepad,
    Wheel,
    /// 文本输入（IME 提交、移动端辅助输入框）。
    Text,
    /// 无法归类的输入行（`kr`、`sfe` 这类裸命令）。
    Other,
}

/// 剪贴板方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClipOp {
    /// 读主机剪贴板（Ctrl+C 后浏览器发 `REQUEST_CLIPBOARD`）。
    Read,
    /// 写主机剪贴板（浏览器 Ctrl+V 上行）。
    Write,
    /// 清空/重置主机剪贴板。
    Clear,
}

/// 媒体方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaDir {
    /// 屏幕画面下行（采集 + 编码 + 发送）。
    VideoOut,
    /// 桌面声音下行。
    AudioOut,
    /// 麦克风上行（`on_track` 收到的远端音频）。
    MicIn,
    /// 摄像头画面注入（前端 WebCodecs 上行）。
    WebcamIn,
}

/// 文件操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileOp {
    Upload,
    List,
    Download,
    /// 写入上传目录之外的位置（当前未开放，预留给未来的"另存为"）。
    WriteOutside,
}

impl EventKind {
    /// 稳定的日志标签。**这个字符串是契约**：策略按它匹配，日志按它 grep，
    /// 所以改名等于破坏兼容，新增变体只能追加。
    pub fn label(self) -> &'static str {
        match self {
            EventKind::Startup => "startup",
            EventKind::Http(ep) => match ep {
                Endpoint::Status => "http.status",
                Endpoint::Upload => "http.upload",
                Endpoint::Files => "http.files",
                Endpoint::Static => "http.static",
                Endpoint::Signaling => "http.signaling",
                Endpoint::Health => "http.health",
                Endpoint::Control => "http.control",
                Endpoint::NotFound => "http.not-found",
            },
            EventKind::Signaling => "signaling",
            EventKind::Control(what) => what,
            EventKind::Input(dev) => match dev {
                Device::Keyboard => "input.keyboard",
                Device::Pointer => "input.pointer",
                Device::Touch => "input.touch",
                Device::Gamepad => "input.gamepad",
                Device::Wheel => "input.wheel",
                Device::Text => "input.text",
                Device::Other => "input.other",
            },
            EventKind::Clipboard(op) => match op {
                ClipOp::Read => "clipboard.read",
                ClipOp::Write => "clipboard.write",
                ClipOp::Clear => "clipboard.clear",
            },
            EventKind::Media(dir) => match dir {
                MediaDir::VideoOut => "media.video-out",
                MediaDir::AudioOut => "media.audio-out",
                MediaDir::MicIn => "media.mic-in",
                MediaDir::WebcamIn => "media.webcam-in",
            },
            EventKind::File(op) => match op {
                FileOp::Upload => "file.upload",
                FileOp::List => "file.list",
                FileOp::Download => "file.download",
                FileOp::WriteOutside => "file.write-outside",
            },
            EventKind::Diagnostics(what) => what,
        }
    }

    /// 纯观测：不影响主机状态，只把已有状态读出去。
    ///
    /// 只读会话（未来的"观众"角色）应该允许这些、拒绝其余。判定写在这里而
    /// 不是各策略里，是为了让"什么算观测"只有一个答案。
    pub fn is_observational(self) -> bool {
        matches!(
            self,
            EventKind::Http(Endpoint::Status)
                | EventKind::Http(Endpoint::Static)
                | EventKind::Http(Endpoint::Files)
                // #93: `/health` 只读。它比 `status` 报得多（连接清单、进程表），
                // 但同样不改变任何状态 —— 所以归在观测里；与之相邻的
                // `Control` 故意**不在**这个列表：那条路径会重开采集器、踢会话。
                | EventKind::Http(Endpoint::Health)
                | EventKind::Http(Endpoint::NotFound)
                | EventKind::Media(MediaDir::VideoOut)
                | EventKind::Media(MediaDir::AudioOut)
                | EventKind::Diagnostics(_)
        )
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// 凭据种类。当前项目没有鉴权，所以实际上永远是 [`Credential::None`]；
/// 结构先留着，补 token 时只改 `principal_of` 一个函数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Credential {
    /// 没有出示任何凭据（局域网裸奔，也是当前唯一的情况）。
    #[default]
    None,
    /// `Authorization: Bearer <session token>`。
    Bearer,
    /// 主控令牌（可改设置、可提名其他会话）。
    Master,
}

impl Credential {
    pub fn label(self) -> &'static str {
        match self {
            Credential::None => "none",
            Credential::Bearer => "bearer",
            Credential::Master => "master",
        }
    }
}

/// 触发者身份。
///
/// 字段全部是 `Option`/默认值，因为四个入口能拿到的信息不一样：HTTP 有 peer
/// 地址，DataChannel 没有（连接早就建立了），信令只有一个会话号。与其在
/// 每个插桩点编一个假值，不如让缺失是显式的。
#[derive(Debug, Clone, Default)]
pub struct Principal {
    pub peer: Option<IpAddr>,
    pub session: Option<u64>,
    pub credential: Credential,
}

impl Principal {
    /// 只知道来源地址的场景（HTTP）。
    pub fn from_peer(peer: IpAddr) -> Self {
        Self {
            peer: Some(peer),
            ..Default::default()
        }
    }

    /// 只知道会话号的场景（DataChannel）。
    pub fn from_session(session: u64) -> Self {
        Self {
            session: Some(session),
            ..Default::default()
        }
    }
}

/// 一次裁决的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 放行。
    Allow,
    /// 放行，但按只读处理（策略声明了只读，且该动作属观测类）。
    ReadOnly,
    /// 拒绝，附一个稳定的原因标签（进日志，不回给客户端 —— 不泄漏策略）。
    Deny(&'static str),
}

impl Verdict {
    pub fn allowed(self) -> bool {
        matches!(self, Verdict::Allow | Verdict::ReadOnly)
    }

    pub fn reason(self) -> Option<&'static str> {
        match self {
            Verdict::Deny(r) => Some(r),
            _ => None,
        }
    }
}

/// 一次待裁决的事件。
pub struct Event<'a> {
    pub kind: EventKind,
    pub principal: &'a Principal,
}

/// 策略。
///
/// 实现者应保持**纯函数**语义：同样的 `(kind, principal)` 给出同样的裁决，
/// 不做 IO、不加锁。原因是它会在采集循环和 DataChannel 回调里被调用，
/// 每次输入都过一遍 —— 在这里读数据库或等网络，键鼠就会卡。
///
/// 需要外部状态（比如"这个 token 属于哪个角色"）时，在 `install` 之前把它
/// 查好、放进策略结构体里。
pub trait Policy: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    fn check(&self, ev: &Event<'_>) -> Verdict;

    /// 策略说 `ReadOnly` 时，这个动作是否仍可执行。默认按
    /// [`EventKind::is_observational`] 判断。
    fn read_only_allows(&self, kind: EventKind) -> bool {
        kind.is_observational()
    }
}

static POLICY: OnceLock<Arc<dyn Policy>> = OnceLock::new();
static ALLOWED: AtomicU64 = AtomicU64::new(0);
static DENIED: AtomicU64 = AtomicU64::new(0);

/// 安装策略。全进程只生效一次；重复安装返回被拒绝的那一个。
pub fn install(p: Arc<dyn Policy>) -> Result<(), Arc<dyn Policy>> {
    POLICY.set(p)
}

/// 当前策略名（未安装时为 `allow-all`，与默认行为一致）。
pub fn policy_name() -> &'static str {
    POLICY.get().map(|p| p.name()).unwrap_or("allow-all")
}

/// 纯裁决，不计数、不记日志。测试与"策略预演"用。
pub fn check_with(policy: &dyn Policy, kind: EventKind, principal: &Principal) -> Verdict {
    let ev = Event { kind, principal };
    match policy.check(&ev) {
        Verdict::ReadOnly => {
            if policy.read_only_allows(kind) {
                Verdict::ReadOnly
            } else {
                Verdict::Deny("read-only session")
            }
        }
        other => other,
    }
}

/// 裁决一个事件，并记账。
///
/// 这是所有插桩点唯一该调用的函数。拒绝时打一条 warn —— 格式固定为
/// `authz deny surface=<label> ...`，这样现在就能用 grep 统计"有多少请求
/// 会被未来的鉴权挡住"，不必等真的做了鉴权才知道。
pub fn check(kind: EventKind, principal: &Principal) -> Verdict {
    let verdict = match POLICY.get() {
        Some(p) => check_with(p.as_ref(), kind, principal),
        None => Verdict::Allow,
    };

    if verdict.allowed() {
        ALLOWED.fetch_add(1, Ordering::Relaxed);
    } else {
        DENIED.fetch_add(1, Ordering::Relaxed);
        log::warn!(
            "authz deny surface={} peer={} session={} cred={} policy={} reason={}",
            kind.label(),
            principal
                .peer
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            principal
                .session
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            principal.credential.label(),
            policy_name(),
            verdict.reason().unwrap_or("-"),
        );
    }
    verdict
}

/// 便捷判断：允许就返回 true。
pub fn allow(kind: EventKind, principal: &Principal) -> bool {
    check(kind, principal).allowed()
}

/// `(放行数, 拒绝数)`，进程级累计。
pub fn counts() -> (u64, u64) {
    (
        ALLOWED.load(Ordering::Relaxed),
        DENIED.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nobody() -> Principal {
        Principal::default()
    }

    #[test]
    fn allow_all_lets_everything_through() {
        let p = AllowAll;
        for kind in [
            EventKind::Startup,
            EventKind::Http(Endpoint::Upload),
            EventKind::Input(Device::Keyboard),
            EventKind::Clipboard(ClipOp::Write),
            EventKind::Media(MediaDir::MicIn),
            EventKind::File(FileOp::Download),
        ] {
            assert_eq!(
                check_with(&p, kind, &nobody()),
                Verdict::Allow,
                "{} should pass under allow-all",
                kind.label()
            );
        }
    }

    /// `deny-all` 的存在意义：把它装上跑一遍，凡是还能用的入口就是漏插桩的。
    #[test]
    fn deny_all_blocks_everything() {
        let p = DenyAll;
        assert_eq!(
            check_with(&p, EventKind::Http(Endpoint::Status), &nobody()),
            Verdict::Deny("deny-all policy")
        );
    }

    /// 只读策略：观测类降级为 `ReadOnly`，动作类直接拒绝。
    #[test]
    fn read_only_downgrades_observational_and_denies_the_rest() {
        struct Viewer;
        impl Policy for Viewer {
            fn name(&self) -> &'static str {
                "viewer"
            }
            fn check(&self, _ev: &Event<'_>) -> Verdict {
                Verdict::ReadOnly
            }
        }

        assert_eq!(
            check_with(&Viewer, EventKind::Http(Endpoint::Status), &nobody()),
            Verdict::ReadOnly
        );
        assert!(check_with(&Viewer, EventKind::Media(MediaDir::VideoOut), &nobody()).allowed());
        assert_eq!(
            check_with(&Viewer, EventKind::Input(Device::Keyboard), &nobody()),
            Verdict::Deny("read-only session")
        );
        assert!(!check_with(&Viewer, EventKind::Clipboard(ClipOp::Write), &nobody()).allowed());
    }

    /// 标签是契约的一部分：策略按它匹配，日志按它 grep。
    #[test]
    fn labels_are_stable_and_unique_per_kind() {
        let kinds = [
            EventKind::Startup,
            EventKind::Http(Endpoint::Status),
            EventKind::Http(Endpoint::Upload),
            EventKind::Http(Endpoint::Files),
            EventKind::Http(Endpoint::Static),
            EventKind::Http(Endpoint::Signaling),
            EventKind::Http(Endpoint::Health),
            EventKind::Http(Endpoint::Control),
            EventKind::Http(Endpoint::NotFound),
            EventKind::Signaling,
            EventKind::Input(Device::Keyboard),
            EventKind::Input(Device::Pointer),
            EventKind::Input(Device::Touch),
            EventKind::Input(Device::Gamepad),
            EventKind::Input(Device::Wheel),
            EventKind::Input(Device::Text),
            EventKind::Input(Device::Other),
            EventKind::Clipboard(ClipOp::Read),
            EventKind::Clipboard(ClipOp::Write),
            EventKind::Clipboard(ClipOp::Clear),
            EventKind::Media(MediaDir::VideoOut),
            EventKind::Media(MediaDir::AudioOut),
            EventKind::Media(MediaDir::MicIn),
            EventKind::Media(MediaDir::WebcamIn),
            EventKind::File(FileOp::Upload),
            EventKind::File(FileOp::List),
            EventKind::File(FileOp::Download),
            EventKind::File(FileOp::WriteOutside),
        ];
        let mut seen = std::collections::HashSet::new();
        for kind in kinds {
            let label = kind.label();
            assert!(seen.insert(label), "duplicate label {label}");
            assert!(
                label.chars().all(|c| c.is_ascii_lowercase() || c == '.' || c == '-'),
                "{label} is not grep-friendly"
            );
        }
    }

    /// `Control` 的标签由调用方给，但必须非空 —— 否则日志里会出现
    /// `surface=` 后面什么都没有，比不记还糟。
    #[test]
    fn control_labels_are_caller_supplied() {
        assert_eq!(
            EventKind::Control("control.set-fps").label(),
            "control.set-fps"
        );
    }
}
