//! #109 虚拟麦克风后端：探测本机有没有"虚拟线缆"，并决定往哪个端点写。
//!
//! 为什么需要这一层：`pipeline/audio.rs` 把麦克风注入**默认 render 端点**，
//! 而游戏声音的下行 loopback 采的正是同一个端点的后混音信号 —— 于是注入的
//! 声音被自己采回，玩家听到自己的声音延迟一个往返。这是拓扑决定的，改延迟、
//! 改码率都消不掉。
//!
//! 所有成熟产品都不这么做：Steam 用 `Steam Streaming Microphone`、Parsec 用
//! VUSB 虚拟 USB 麦克风、Selkies 用 PulseAudio 的 null-sink 虚拟 source。共同点
//! 是**麦克风落在输入设备上**，而不是扬声器上。本模块就是把这件事接上：找一个
//! "render 端写入、capture 端被程序读成麦克风"的虚拟线缆，把 PCM 写到它的
//! render 端，注入的信号从此不经过默认端点，回路不存在。
//!
//! 已知可用的虚拟线缆（按探测优先级，先命中的胜出）：
//!
//! | 提供方 | render 端（我们写入） | capture 端（程序选它当麦克风） | 需要装什么 |
//! |---|---|---|---|
//! | Steam | `Steam Streaming Microphone` | 同名 | Steam 自带，普通用户权限 |
//! | VB-CABLE | `CABLE Input` | `CABLE Output` | VB-Audio Virtual Cable |
//! | VoiceMeeter | `VoiceMeeter Input` | `VoiceMeeter Output` | VoiceMeeter |
//! | Parsec VUSB | 没有端点，走 IOCTL 造一个 USB 麦克风 | `Parsec Virtual Microphone` | VUSB 驱动 + 管理员；本版本未实现 |
//!
//! Steam 排第一不是随便排的：它的两个虚拟设备由 Steam 自己安装，玩家机器上
//! 本来就有，不需要管理员，也不需要额外驱动 —— 我们只往它的 render 端起一个
//! WASAPI 客户端，和写默认扬声器是同一段代码。
//!
//! 一条必须遵守的纪律（`steam-deck-routing-for-moonlight` 踩过并写在 README 里）：
//! **绝不能把虚拟线缆的 render 端设成默认播放设备**。一旦设成默认，下行 loopback
//! 采的就是它，自听立刻回来 —— 而且这次连"注入点选错了"都看不出来，因为一切
//! 都"正常"。所以 [`resolve`] 会把这种情况判成 `usable: false` 并说明原因。

use std::sync::OnceLock;

/// 虚拟麦克风的提供方。`Named` 是运维在 `--mic-device` 里直接给的设备名，
/// 不属于任何已知家族，但仍然按"虚拟线缆"规则使用（只要它既有 render 端
/// 又能被程序当成麦克风）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Steam,
    VbCable,
    VoiceMeeter,
    Parsec,
    /// 旧行为：写默认 render 端点。只服务"对讲"，必定自听，所以默认不用它。
    Default,
    /// 运维指定了一个不在已知家族里的设备名。
    Named,
    /// 没有可用的虚拟录音设备。
    None,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Steam => "steam",
            Provider::VbCable => "vb-cable",
            Provider::VoiceMeeter => "voicemeeter",
            Provider::Parsec => "parsec-vusb",
            Provider::Default => "default",
            Provider::Named => "named",
            Provider::None => "none",
        }
    }

    /// `--mic-device` 接受的名字。全部按小写比较，`cable` 是 `vb-cable` 的别名。
    pub fn from_name(name: &str) -> Option<Provider> {
        match name.trim().to_ascii_lowercase().as_str() {
            "steam" => Some(Provider::Steam),
            "vb-cable" | "vbcable" | "cable" => Some(Provider::VbCable),
            "voicemeeter" | "vm" => Some(Provider::VoiceMeeter),
            "parsec" | "parsec-vusb" | "vusb" => Some(Provider::Parsec),
            "default" => Some(Provider::Default),
            _ => None,
        }
    }
}

/// 已知的虚拟线缆：(提供方, render 端名字片段, capture 端名字片段)。
///
/// 顺序即优先级。Steam 第一，因为它不需要额外安装。
const CABLES: [(Provider, &str, &str); 3] = [
    (Provider::Steam, "Steam Streaming Microphone", "Steam Streaming Microphone"),
    (Provider::VbCable, "CABLE Input", "CABLE Output"),
    (Provider::VoiceMeeter, "VoiceMeeter Input", "VoiceMeeter Output"),
];

/// 一次探测的结果：本机有哪些端点、默认播放是谁、Parsec 驱动在不在。
#[derive(Debug, Clone, Default)]
pub struct Endpoints {
    pub render: Vec<String>,
    pub capture: Vec<String>,
    pub default_render: Option<String>,
    /// Parsec VUSB 驱动装没装。装了也还不能用（需要 IOCTL 后端），但要在
    /// 日志里说出来，否则运维会以为"装了 Parsec 就该能出声"。
    pub parsec_driver: bool,
}

impl Endpoints {
    pub fn summary(&self) -> String {
        format!(
            "render={} capture={} default={:?} parsec={}",
            self.render.len(),
            self.capture.len(),
            self.default_render.as_deref().unwrap_or(""),
            self.parsec_driver
        )
    }

    /// 逐条列出端点名，排障时"我装了 CABLE 为什么没认出来"全靠这一行。
    pub fn detail(&self) -> Vec<String> {
        let mut out = Vec::new();
        for name in &self.render {
            out.push(format!("playback | {name}"));
        }
        for name in &self.capture {
            out.push(format!("recording | {name}"));
        }
        out
    }
}

/// 决议结果：往哪写、程序该选哪个设备当麦克风、能不能用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub provider: Provider,
    /// `None` 表示写默认 render 端点（只有 `Provider::Default` 会这样）。
    pub sink: Option<String>,
    /// 让运维去程序里选的那个设备名。找不到时是 `None`，但注入仍然可用。
    pub capture: Option<String>,
    pub usable: bool,
    pub note: String,
}

impl Target {
    /// 这个目标会不会自听。只有写默认扬声器会。
    pub fn echoes(&self) -> bool {
        self.provider == Provider::Default
    }
}

/// 名字匹配：先找完全相等（忽略大小写），再找包含。
///
/// "包含"这一步是为了容忍厂商在名字前后加料（`CABLE Input (VB-Audio Virtual
/// Cable)`、`VoiceMeeter Input (VB-Audio VoiceMeeter VAIO)`），而这些后缀在不同
/// 驱动版本之间会变，写死全名等于给未来的自己埋一个"驱动一升级就失效"。
fn pick(names: &[String], needle: &str) -> Option<String> {
    if needle.is_empty() {
        return None;
    }
    let needle = needle.to_ascii_lowercase();
    names
        .iter()
        .find(|name| name.to_ascii_lowercase() == needle)
        .or_else(|| {
            names
                .iter()
                .find(|name| name.to_ascii_lowercase().contains(&needle))
        })
        .cloned()
}

/// 把"命中的 render 端名字"变成一个可执行的决议，并在这一步挡住自听陷阱。
fn cable_target(
    provider: Provider,
    sink: String,
    capture_hint: &str,
    capture_names: &[String],
    default_render: &str,
) -> Target {
    let capture = pick(capture_names, capture_hint);
    // 自听陷阱：虚拟线缆被设成默认播放设备。此时 loopback 采的就是我们写进去的
    // 东西，和"写默认扬声器"等价，但更难诊断 —— 一切看起来都装对了。
    if !sink.is_empty() && sink.eq_ignore_ascii_case(default_render) {
        return Target {
            provider,
            note: format!(
                "{sink} 同时是默认播放端点：下行 loopback 采的就是它，注入会被原样采回，\
                 自听又回来了。把 Windows 默认播放改回物理喇叭/耳机，麦克风不用改。"
            ),
            sink: Some(sink),
            capture,
            usable: false,
        };
    }
    let note = match &capture {
        Some(name) => format!("注入 {sink}，游戏/语音软件里把麦克风选成 {name}"),
        None => format!(
            "注入 {sink}，但录音端点里没找到 {capture_hint:?} —— 检查驱动的录音端是否被禁用"
        ),
    };
    Target {
        provider,
        sink: Some(sink),
        capture,
        usable: true,
        note,
    }
}

fn no_target(provider: Provider, note: String) -> Target {
    Target {
        provider,
        sink: None,
        capture: None,
        usable: false,
        note,
    }
}

/// 决议：给定 `--mic-device` 与探测结果，选出写入端点。
///
/// 纯函数，不碰 COM —— 所有分支都能在单测里跑，因为"这台机器有没有装
/// CABLE"是最难在 CI 里复现的条件。
pub fn resolve(
    requested: &str,
    render: &[String],
    capture: &[String],
    default_render: &str,
    parsec_driver: bool,
) -> Target {
    let requested = requested.trim().to_ascii_lowercase();

    // 显式要求旧行为：写默认扬声器。允许，但说清楚代价。
    if requested == "default" {
        return Target {
            provider: Provider::Default,
            sink: None,
            capture: None,
            usable: true,
            note: "写默认 render 端点（对讲用）：注入会被 loopback 采回，开麦时能听到自己。\
                   要游戏内语音请改用虚拟设备"
                .to_owned(),
        };
    }

    // 指名要某个家族：只试那一个，缺了就是缺了，不静默降级到别的线缆。
    if let Some(want) = Provider::from_name(&requested) {
        if let Some((_, sink_hint, cap_hint)) = CABLES.iter().find(|(p, _, _)| *p == want) {
            return match pick(render, sink_hint) {
                Some(sink) => cable_target(want, sink, cap_hint, capture, default_render),
                None => no_target(
                    want,
                    format!("--mic-device {requested}: 播放端点里没有 {sink_hint:?}，驱动没装或被禁用"),
                ),
            };
        }
        // Parsec 没有端点，只能走 IOCTL。
        return no_target(
            Provider::Parsec,
            if parsec_driver {
                "检测到 Parsec VUSB 驱动，但本版本还没有 IOCTL 后端；先装 Steam 或 VB-CABLE".to_owned()
            } else {
                "Parsec VUSB 驱动未安装（也没有其他虚拟录音设备）".to_owned()
            },
        );
    }

    // 没给名字或给了 auto：按优先级挑一台现成的。
    if requested.is_empty() || requested == "auto" {
        for (provider, sink_hint, cap_hint) in CABLES {
            if let Some(sink) = pick(render, sink_hint) {
                return cable_target(provider, sink, cap_hint, capture, default_render);
            }
        }
        return no_target(
            if parsec_driver {
                Provider::Parsec
            } else {
                Provider::None
            },
            if parsec_driver {
                "只检测到 Parsec VUSB 驱动，本版本没有它的 IOCTL 后端；装 Steam（自带）或 VB-CABLE 即可".to_owned()
            } else {
                "未发现虚拟录音设备：装 Steam 会自带 Steam Streaming Microphone，或装 VB-CABLE / \
                 VoiceMeeter；只想对讲可用 --mic-device default（会自听）"
                    .to_owned()
            },
        );
    }

    // 兜底：当成一个具体设备名。运维自己的虚拟声卡（Virtual Audio Cable 的
    // "Line 1"、Voicemeeter Banana 的 AUX 等）都走这条。
    match pick(render, &requested) {
        Some(sink) => cable_target(Provider::Named, sink, "", capture, default_render),
        None => no_target(
            Provider::Named,
            format!("--mic-device {requested}: 播放端点里没有这个设备"),
        ),
    }
}

// ---------------------------------------------------------------------------
// 进程级决议：main 探测一次装进来，麦克风线程和 health 都读这一份。
// ---------------------------------------------------------------------------

static TARGET: OnceLock<Target> = OnceLock::new();

/// 装进进程。重复调用保留第一次的结果 —— 决议必须全进程一致，否则
/// 日志里的端点和实际写入的端点会各说各话。
pub fn install(target: Target) {
    let _ = TARGET.set(target);
}

pub fn target() -> Option<&'static Target> {
    TARGET.get()
}

/// 麦克风线程要写的端点名。`None` = 默认播放端点。
pub fn sink_name() -> Option<String> {
    TARGET.get().and_then(|t| t.sink.clone())
}

// ---------------------------------------------------------------------------
// Windows：端点枚举
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub use win::*;

#[cfg(windows)]
mod win {
    use super::Endpoints;
    use windows::core::w;
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Media::Audio::{
        eCapture, eConsole, eRender, IMMDevice, IMMDeviceCollection, IMMDeviceEnumerator,
        MMDeviceEnumerator, DEVICE_STATE_ACTIVE, EDataFlow,
    };
    use windows::Win32::System::Com::StructuredStorage::{
        PropVariantClear, PROPVARIANT,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
    };
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    };
    use windows::Win32::System::Variant::{VT_BSTR, VT_LPWSTR};
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

    /// 端点枚举的一次快照。
    pub fn probe() -> Endpoints {
        unsafe { probe_inner().unwrap_or_default() }
    }

    unsafe fn probe_inner() -> windows::core::Result<Endpoints> {
        // 已经初始化过就沿用现成的 apartment：RPC_E_CHANGED_MODE 不是错误。
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        Ok(Endpoints {
            render: names(&enumerator, eRender)?,
            capture: names(&enumerator, eCapture)?,
            default_render: enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .ok()
                .and_then(|device| friendly_name(&device).ok().flatten()),
            parsec_driver: parsec_driver_present(),
        })
    }

    unsafe fn names(
        enumerator: &IMMDeviceEnumerator,
        flow: EDataFlow,
    ) -> windows::core::Result<Vec<String>> {
        let list: IMMDeviceCollection = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::with_capacity(list.GetCount().unwrap_or(0) as usize);
        for i in 0..list.GetCount()? {
            if let Ok(device) = list.Item(i) {
                if let Ok(Some(name)) = friendly_name(&device) {
                    out.push(name);
                }
            }
        }
        Ok(out)
    }

    /// 读端点的友好名（`Speakers (Realtek)`、`CABLE Input (VB-Audio ...)`）。
    ///
    /// 这个 crate 版本没有 `PropVariantToStringAlloc`，所以直接读联合体：设备
    /// 名只会是 `VT_LPWSTR` 或 `VT_BSTR`，两种都取，其余一律当没名字。取完
    /// 立刻 `PropVariantClear`，否则每次枚举都会漏掉一块 CoTaskMem 内存。
    unsafe fn friendly_name(device: &IMMDevice) -> windows::core::Result<Option<String>> {
        let store: IPropertyStore = device.OpenPropertyStore(STGM_READ)?;
        let mut value: PROPVARIANT = store.GetValue(&PKEY_Device_FriendlyName)?;
        let inner = &value.Anonymous.Anonymous;
        let name = match inner.vt {
            VT_LPWSTR => {
                let raw = inner.Anonymous.pwszVal;
                if raw.is_null() {
                    None
                } else {
                    raw.to_string().ok()
                }
            }
            VT_BSTR => {
                let text = inner.Anonymous.bstrVal.to_string();
                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
            _ => None,
        };
        let _ = PropVariantClear(&mut value);
        Ok(name)
    }

    /// 按友好名找一个 render 端点。找不到时把现有名字带进错误里，
    /// 好让"驱动装了但名字不一样"这件事一眼看出来。
    pub fn find_render(
        enumerator: &IMMDeviceEnumerator,
        want: &str,
    ) -> anyhow::Result<IMMDevice> {
        unsafe {
            let list: IMMDeviceCollection =
                enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            let mut seen = Vec::new();
            for i in 0..list.GetCount()? {
                let Ok(device) = list.Item(i) else { continue };
                let Ok(Some(name)) = friendly_name(&device) else {
                    continue;
                };
                if name.eq_ignore_ascii_case(want) {
                    return Ok(device);
                }
                seen.push(name);
            }
            anyhow::bail!(
                "render 端点里找不到 {want:?}（现有：{}）",
                seen.join(" / ")
            )
        }
    }

    /// Parsec VUSB 驱动（PVUD）在设备树里的位置是 `Root\Parsec\VUSBA`。
    /// 只读一次注册表，比打开设备句柄便宜，也不需要 SetupAPI。
    fn parsec_driver_present() -> bool {
        let mut key = HKEY::default();
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                w!("SYSTEM\\CurrentControlSet\\Enum\\Root\\Parsec\\VUSBA"),
                0,
                KEY_READ,
                &mut key,
            )
        };
        // 0.52 里 `RegOpenKeyExW` 返回 `Result<()>`，不是 WIN32_ERROR。
        if status.is_ok() {
            unsafe {
                let _ = RegCloseKey(key);
            }
            true
        } else {
            false
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn auto_takes_steam_when_two_cables_are_installed() {
        // Steam 排第一的理由是"不需要额外安装"：两台都装了也应该选它。
        let render = names(&["CABLE Input (VB-Audio Virtual Cable)", "Steam Streaming Microphone"]);
        let capture = names(&["CABLE Output (VB-Audio Virtual Cable)", "Steam Streaming Microphone"]);
        let t = resolve("auto", &render, &capture, "Speakers (Realtek)", false);
        assert_eq!(t.provider, Provider::Steam);
        assert_eq!(t.sink.as_deref(), Some("Steam Streaming Microphone"));
        assert_eq!(t.capture.as_deref(), Some("Steam Streaming Microphone"));
        assert!(t.usable && !t.echoes());
    }

    #[test]
    fn the_configured_name_tolerates_driver_suffixes() {
        // 厂商会在名字后面加括号后缀，而且换驱动版本就变。
        let render = names(&["CABLE Input (VB-Audio Virtual Cable)"]);
        let t = resolve("auto", &render, &names(&["CABLE Output (VB-Audio Virtual Cable)"]), "Speakers", false);
        assert_eq!(t.provider, Provider::VbCable);
        assert_eq!(t.capture.as_deref(), Some("CABLE Output (VB-Audio Virtual Cable)"));
    }

    #[test]
    fn no_cable_is_unusable_and_names_what_to_install() {
        let t = resolve("auto", &names(&["Speakers (Realtek)"]), &names(&["Mic (Realtek)"]), "Speakers (Realtek)", false);
        assert!(!t.usable);
        assert_eq!(t.provider, Provider::None);
        assert!(t.note.contains("VB-CABLE"), "note was {:?}", t.note);
        assert!(t.note.contains("Steam"), "note was {:?}", t.note);
    }

    #[test]
    fn the_echo_trap_is_refused_with_the_fix_in_the_message() {
        // CABLE Input 被设成默认播放：注入会被 loopback 原样采回。
        let render = names(&["CABLE Input (VB-Audio Virtual Cable)"]);
        let t = resolve("auto", &render, &names(&["CABLE Output (VB-Audio Virtual Cable)"]), "CABLE Input (VB-Audio Virtual Cable)", false);
        assert_eq!(t.provider, Provider::VbCable);
        assert!(!t.usable, "必须拒绝，否则自听会在'看起来都装对了'的情况下复发");
        assert!(t.note.contains("默认播放"), "note was {:?}", t.note);
    }

    #[test]
    fn naming_one_family_does_not_fall_back_to_another() {
        let render = names(&["Steam Streaming Microphone"]);
        let t = resolve("cable", &render, &[], "Speakers", false);
        assert!(!t.usable);
        assert_eq!(t.provider, Provider::VbCable);
        assert!(t.note.contains("CABLE Input"), "note was {:?}", t.note);
    }

    #[test]
    fn a_parsec_driver_alone_is_reported_but_not_used() {
        let t = resolve("auto", &names(&["Speakers"]), &[], "Speakers", true);
        assert_eq!(t.provider, Provider::Parsec);
        assert!(!t.usable);
        assert!(t.note.contains("IOCTL"), "note was {:?}", t.note);
    }

    #[test]
    fn default_keeps_the_old_behaviour_and_admits_the_echo() {
        let t = resolve("default", &names(&["Speakers"]), &[], "Speakers", false);
        assert_eq!(t.provider, Provider::Default);
        assert!(t.usable);
        assert!(t.echoes());
        assert!(t.note.contains("听到自己"), "note was {:?}", t.note);
    }

    #[test]
    fn an_arbitrary_device_name_is_used_as_a_cable() {
        // 运维自己的虚拟声卡：Virtual Audio Cable 的 "Line 1"。
        let render = names(&["Line 1 (Virtual Audio Cable)"]);
        let t = resolve("Line 1", &render, &names(&["Line 1 (Virtual Audio Cable)"]), "Speakers", false);
        assert_eq!(t.provider, Provider::Named);
        assert!(t.usable);
        assert_eq!(t.sink.as_deref(), Some("Line 1 (Virtual Audio Cable)"));
    }

    #[test]
    fn a_typo_in_the_device_name_says_which_devices_exist() {
        let t = resolve("CABLE Innput", &names(&["Speakers"]), &[], "Speakers", false);
        assert!(!t.usable);
        assert!(t.note.contains("没有这个设备"), "note was {:?}", t.note);
    }
}
