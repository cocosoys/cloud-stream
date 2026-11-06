//! P1：把系统默认**录音**设备切到虚拟麦，退出时还原。
//!
//! # 为什么这一步不能省
//!
//! [`super::virtual_mic`] 只解决了"往哪写"：PCM 进了虚拟线缆的 render 端，从
//! `CABLE Output` 能读到波形（2026-11-05 在 .148 上验过，rms −22 dBFS）。但
//! 游戏 / Discord / Steam 语音**读的是系统默认录音设备**，不是"某个你想选的
//! 设备"。用户不手动改，结果就是：health 一切正常、日志写着
//! `capture=CABLE Output`、对方却什么也听不见。所以这一步是"装好了"和"能用"
//! 的分界线。
//!
//! # 为什么是启动时切，而不是"第一个麦克风包到达时"
//!
//! 游戏在**启动时**读一次默认设备并缓存（Vibepollo 要用 2/5/10/20 s 重试绕开
//! 的就是这个缓存）。如果等用户点麦克风才切，游戏早就记住了旧的物理麦。切换
//! 因此放在启动、决议之后，让之后启动的任何程序读到的都是虚拟麦。
//!
//! 代价：服务运行期间，被控机本地的录音软件也会拿到虚拟麦。这是功能本身的
//! 语义（接管默认录音设备），所以退出时必须还原，`--mic-default-device off`
//! 留一个完全不动系统设置的开关。
//!
//! # 还原的边界
//!
//! Ctrl+C 与正常返回都会走 [`restore`]。被 `taskkill /F` 或崩溃杀掉则不会 ——
//! 那时默认录音会停在虚拟麦上，表现是"本地麦克风没声音"。这是已知代价：日志
//! 里写清楚，手动改回来，或带 `--mic-default-device off` 重跑一次。
//!
//! # 为什么是未文档化的 `IPolicyConfig`
//!
//! Windows 没有公开 API 能设置默认音频端点。所有能做这件事的工具（NirSoft
//! SoundVolumeView、AudioDeviceCmdlets、nircmd）用的都是 `PolicyConfig`
//! 这个 COM 对象上的 `IPolicyConfig::SetDefaultEndpoint`，它没有头文件里的
//! 声明，但接口是稳定的（Vista 到今天没变过方法顺序）。`windows` crate 是
//! 从 Windows 元数据生成的，元数据里没有这个接口，所以下面手写 vtable ——
//! 结构照抄该 crate 自己生成的写法，只给真正要调的那一个方法做 wrapper。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use super::virtual_mic::{Provider, Target};

/// `--mic-default-device` 的取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// 决议出虚拟麦就切过去（默认）。
    Auto,
    /// 不碰系统设置：玩家自己在游戏里选麦克风。
    Off,
}

impl Policy {
    /// 解析命令行。空串按默认走 —— `--mic-default-device ""` 不该等于"关"。
    pub fn from_name(name: &str) -> Option<Policy> {
        match name.trim().to_ascii_lowercase().as_str() {
            "" | "auto" | "on" | "1" | "true" | "yes" => Some(Policy::Auto),
            "off" | "keep" | "0" | "false" | "no" | "none" => Some(Policy::Off),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Policy::Auto => "auto",
            Policy::Off => "off",
        }
    }
}

/// 该不该切、切成什么。纯决策，不碰 COM —— 这样"机器上装没装 CABLE"之外
/// 的每个分支都能在单测里跑。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 开关关掉了。
    Disabled,
    /// 没有可切的端点，附原因（直接进日志）。
    NotApplicable(String),
    /// 系统默认录音已经是它了，不用动系统设置。
    Already(String),
    /// 切到这个录音端点（用友好名）。
    Switch(String),
}

/// 三条 role 都要切。Windows 的"默认设备"下拉同时写 console 与 multimedia，
/// "默认通信设备"单独写 communications；只切前两个的话，Discord、Steam 语音
/// 这类走通信角色的程序仍然指着物理麦。
pub const ROLES: [i32; 3] = [ROLE_CONSOLE, ROLE_MULTIMEDIA, ROLE_COMMUNICATIONS];
pub const ROLE_CONSOLE: i32 = 0;
pub const ROLE_MULTIMEDIA: i32 = 1;
pub const ROLE_COMMUNICATIONS: i32 = 2;

/// 角色名，只用于日志和 health —— `role=2` 没法读。
pub fn role_name(role: i32) -> &'static str {
    match role {
        ROLE_CONSOLE => "console",
        ROLE_MULTIMEDIA => "multimedia",
        ROLE_COMMUNICATIONS => "communications",
        _ => "unknown",
    }
}

/// 决议：要不要切，切到哪。
pub fn plan(policy: Policy, target: &Target, default_capture: Option<&str>) -> Decision {
    if policy == Policy::Off {
        return Decision::Disabled;
    }
    // 决议不可用（没装线缆 / 线缆被设成默认播放端点）就别动系统设置：
    // 切到一个不存在的设备只会让用户更难排障。
    if !target.usable {
        return Decision::NotApplicable(format!("麦克风决议不可用：{}", target.note));
    }
    // 写默认扬声器的对讲模式没有"另一个端点"可切。
    if target.provider == Provider::Default {
        return Decision::NotApplicable(
            "当前是 --mic-device default（写默认扬声器），没有虚拟录音设备可切".to_owned(),
        );
    }
    let Some(capture) = target.capture.as_deref() else {
        return Decision::NotApplicable(
            "找不到对应的录音端点，先看驱动是否被禁用".to_owned(),
        );
    };
    if default_capture.is_some_and(|current| current.eq_ignore_ascii_case(capture)) {
        return Decision::Already(capture.to_owned());
    }
    Decision::Switch(capture.to_owned())
}

/// 对外的一行状态，health 与启动日志共用。
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub policy: &'static str,
    /// `switched` / `already` / `disabled` / `skipped` / `failed`
    pub action: &'static str,
    /// 切到了哪个设备（友好名）。
    pub device: Option<String>,
    /// 切完之后**系统实际报出来的**默认录音（console 角色）。
    ///
    /// 和 `device` 分开是刻意的：`device` 是"我们请求切到哪"，`current` 是
    /// "系统说现在是谁"。只报前者的话，切换静默失败（权限、设备被独占）时
    /// health 会显示一切正常，而游戏收到的仍是物理麦。
    pub current: Option<String>,
    /// 切换前的默认录音，`console=Mic (Realtek)` 这种。
    pub previous: Vec<String>,
    pub note: String,
    /// [`restore`] 是否已经跑过。
    pub restored: bool,
}

impl Status {
    /// 给启动日志用的一句话。
    pub fn summary(&self) -> String {
        format!(
            "action={} policy={} device={:?} current={:?}{}",
            self.action,
            self.policy,
            self.device.as_deref().unwrap_or(""),
            self.current.as_deref().unwrap_or(""),
            if self.note.is_empty() {
                String::new()
            } else {
                format!(" | {}", self.note)
            }
        )
    }
}

// ---------------------------------------------------------------------------
// 进程级状态
// ---------------------------------------------------------------------------

struct State {
    status: Status,
    /// 切换前每个角色的原设备 id：`[(role, device_id)]`。
    saved: Vec<(i32, String)>,
    /// 原值文件的落点，[`restore`] 成功后要删掉它。
    path: Option<PathBuf>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            status: Status {
                policy: Policy::Auto.as_str(),
                action: "skipped",
                ..Status::default()
            },
            saved: Vec::new(),
            path: None,
        }
    }
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State::default()))
}

/// 按决议动系统设置，并把原值记下来等 [`restore`]。
///
/// 任何一步失败都只降级成告警：麦克风上行、视频、输入、文件传输都不依赖它，
/// 让整个服务因为"改不了默认录音设备"起不来是本末倒置。
///
/// `state_path` 是原值的落盘位置。**必须给**：停止服务的方式是
/// `Stop-Process -Force`（部署脚本、看门狗、关机都如此），进程拿不到任何清理
/// 机会，`restore()` 根本不会执行。文件是唯一能把原值带到下一次启动的东西，
/// 没有它，一次强杀就让被控机的默认录音永远停在虚拟麦上 —— 本地麦克风彻底
/// 没声音，而且看起来像驱动坏了。
pub fn apply(policy: Policy, target: &Target, state_path: Option<&Path>) -> Status {
    let (default_capture, snapshot) = win::snapshot();

    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    st.status.policy = policy.as_str();
    st.status.restored = false;
    st.path = state_path.map(Path::to_path_buf);

    // 上次没还原？文件在就说明有。里面有**真正的原始设备**，比这次读到的
    // 当前值可信 —— 当前值可能已经是我们自己上次切过去的虚拟麦。
    let leftover = state_path.and_then(read_state);

    // `off` 兼作"撤销"：有遗留就还回去。这样文档里那句"带
    // --mic-default-device off 重跑一次"才真的成立，而不用另开一个子命令。
    if policy == Policy::Off {
        match leftover {
            Some(previous) => {
                let restored = restore_entries(&previous);
                if let Some(path) = state_path {
                    remove_state(path);
                }
                st.status.action = "restored";
                st.status.note = format!(
                    "--mic-default-device off：已把上次遗留的默认录音设备还原（{restored} 个角色）"
                );
            }
            None => {
                st.status.action = "disabled";
                st.status.note = "--mic-default-device off：不碰系统默认录音设备".to_owned();
            }
        }
        return st.status.clone();
    }

    let decision = plan(policy, target, default_capture.as_deref());

    match decision {
        Decision::Disabled => {
            st.status.action = "disabled";
            st.status.note = "--mic-default-device off：不碰系统默认录音设备".to_owned();
        }
        Decision::NotApplicable(reason) => {
            st.status.action = "skipped";
            st.status.note = reason;
        }
        Decision::Already(capture) => {
            // 已经是虚拟麦了。两种情况截然不同：
            //  - 文件在：是我们上次切的、进程没来得及还原 → 接管还原责任；
            //  - 文件不在：用户自己设的或本来就这样 → 不碰，退出也别乱改。
            st.status.action = "already";
            st.status.device = Some(capture.clone());
            st.status.current = Some(capture.clone());
            match leftover {
                Some(previous) => {
                    st.status.note = format!(
                        "默认录音已经是 {capture}，且发现上次未还原的记录：本次退出时还原到原始设备"
                    );
                    st.saved = previous;
                }
                None => {
                    st.status.note = format!("系统默认录音设备已经是 {capture}，未改动");
                }
            }
        }
        Decision::Switch(capture) => match win::switch_capture(&capture, &snapshot) {
            Ok(saved) => {
                st.status.action = "switched";
                st.status.device = Some(capture.clone());
                // 切换前的默认录音是排障最想知道的那一条（"还原该还原成谁"）。
                st.status.previous = default_capture
                    .clone()
                    .map(|name| vec![format!("console={name}")])
                    .unwrap_or_default();
                // 切完再读一次：这是"系统真的改了"的唯一证据。`device` 只是
                // 我们的意图，设备被独占或权限不足时两者会不一致。
                st.status.current = win::snapshot().0;
                // 有遗留记录时以它为准：那才是原始设备，这次读到的可能是
                // 上一次我们自己切过去的。
                let keep = leftover.unwrap_or(saved);
                if let Some(path) = state_path {
                    write_state(path, &capture, &keep);
                }
                st.status.note = format!(
                    "默认录音设备已切到 {capture}，退出或下次 --mic-default-device off 时还原"
                );
                st.saved = keep;
            }
            Err(err) => {
                st.status.action = "failed";
                st.status.note = err;
            }
        },
    }
    st.status.clone()
}

/// 还原到切换前的设备。幂等：没切过、已经还原过、进程没起来过都安全。
pub fn restore() {
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    if st.saved.is_empty() || st.status.restored {
        return;
    }
    let count = restore_entries(&st.saved);
    log::info!("microphone restore | 默认录音设备已还原（{count} 个角色）");
    if let Some(path) = st.path.clone() {
        remove_state(&path);
    }
    st.saved.clear();
    st.status.restored = true;
    st.status.note = format!("默认录音设备已还原（{count} 个角色）");
}

/// 把一组 `(role, device_id)` 写回系统，返回成功的个数。
fn restore_entries(entries: &[(i32, String)]) -> usize {
    let mut ok = 0;
    for (role, id) in entries {
        match win::set_default_capture(id, *role) {
            Ok(()) => ok += 1,
            Err(err) => log::warn!("microphone restore | {}={id} 失败：{err}", role_name(*role)),
        }
    }
    ok
}

// ---------------------------------------------------------------------------
// 原值文件：`logs/mic-default.json`
// ---------------------------------------------------------------------------

/// 写原值。格式故意扁平，出问题时能直接看：
/// `{"device":"CABLE Output (...)","previous":[{"role":0,"id":"{0.0.0...}"}]}`
fn write_state(path: &Path, device: &str, entries: &[(i32, String)]) {
    let previous: Vec<serde_json::Value> = entries
        .iter()
        .map(|(role, id)| serde_json::json!({ "role": role, "id": id }))
        .collect();
    let body = serde_json::json!({ "device": device, "previous": previous });
    // 父目录不一定是 logs（`--log` 可以指到别处），缺了就建不出来也无所谓：
    // 写不成只影响"强杀后能不能自动还原"，不该拦住启动。
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(err) = std::fs::write(path, body.to_string()) {
        log::warn!(
            "microphone restore | 写不了 {}：{err}（强杀后不会自动还原）",
            path.display()
        );
    }
}

/// 读原值。文件不存在、坏掉、字段缺失一律返回 `None` —— 这个文件是"尽力而为"
/// 的备忘，不是配置，读不懂就当没有，绝不能让启动失败。
fn read_state(path: &Path) -> Option<Vec<(i32, String)>> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let list = value.get("previous")?.as_array()?;
    let mut out = Vec::with_capacity(list.len());
    for item in list {
        let role = item.get("role")?.as_i64()? as i32;
        let id = item.get("id")?.as_str()?.to_owned();
        out.push((role, id));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn remove_state(path: &Path) {
    let _ = std::fs::remove_file(path);
}

pub fn status() -> Option<Status> {
    STATE
        .get()
        .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()).status.clone())
}

// ---------------------------------------------------------------------------
// 非 Windows：保留接口，明确说没实现
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
mod win {
    pub fn snapshot() -> (Option<String>, Vec<(i32, String)>) {
        (None, Vec::new())
    }

    pub fn switch_capture(_name: &str, _saved: &[(i32, String)]) -> Result<Vec<(i32, String)>, String> {
        Err("默认录音设备切换目前只有 Windows 实现（Linux 侧是 pactl set-default-source）".to_owned())
    }

    pub fn set_default_capture(_id: &str, _role: i32) -> Result<(), String> {
        Err("只有 Windows 实现".to_owned())
    }
}

// ---------------------------------------------------------------------------
// Windows：IPolicyConfig
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    use windows::core::{ComInterface, GUID, HRESULT, PCWSTR, PWSTR};
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{
        eCapture, eCommunications, eConsole, eMultimedia, ERole, IMMDeviceCollection,
        IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    use super::{ROLE_COMMUNICATIONS, ROLE_CONSOLE, ROLE_MULTIMEDIA};

    /// `CPolicyConfigClient` 的 CLSID。`IPolicyConfig` 就挂在它上面。
    const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

    /// 未文档化的 `IPolicyConfig`。方法顺序即 vtable 顺序，**不能改**：
    /// 少一个、换一个位置，`SetDefaultEndpoint` 就会跳到别的函数上，直接崩。
    /// 前 11 个我们从不调用，只用来占位。
    #[repr(C)]
    #[allow(non_snake_case)]
    pub struct IPolicyConfig_Vtbl {
        pub base__: windows::core::IUnknown_Vtbl,
        pub GetMixFormat: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut *mut c_void) -> HRESULT,
        pub GetDeviceFormat:
            unsafe extern "system" fn(*mut c_void, PCWSTR, i32, *mut *mut c_void) -> HRESULT,
        pub ResetDeviceFormat: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
        pub SetDeviceFormat:
            unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void, *mut c_void) -> HRESULT,
        pub GetProcessingPeriod:
            unsafe extern "system" fn(*mut c_void, PCWSTR, i32, *mut i64, *mut i64) -> HRESULT,
        pub SetProcessingPeriod: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut i64) -> HRESULT,
        pub GetShareMode: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void) -> HRESULT,
        pub SetShareMode: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void) -> HRESULT,
        pub GetPropertyValue:
            unsafe extern "system" fn(*mut c_void, PCWSTR, *const c_void, *mut c_void) -> HRESULT,
        pub SetPropertyValue:
            unsafe extern "system" fn(*mut c_void, PCWSTR, *const c_void, *const c_void) -> HRESULT,
        pub SetDefaultEndpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT,
        pub SetEndpointVisibility: unsafe extern "system" fn(*mut c_void, PCWSTR, i32) -> HRESULT,
    }

    /// `#[repr(transparent)]` 套在 `IUnknown` 上：引用计数与释放全部沿用
    /// `IUnknown` 的实现（它的 `Drop` 会调 `Release`），不需要我们写一行。
    #[repr(transparent)]
    pub struct IPolicyConfig(windows::core::IUnknown);

    unsafe impl Interface for IPolicyConfig {
        type Vtable = IPolicyConfig_Vtbl;
    }

    /// `ComInterface` 要求 `Clone`。直接克隆内部的 `IUnknown`，它自己的
    /// `Clone` 就是一次 `AddRef` —— 不要在这里再手写一遍，会重复计数。
    impl Clone for IPolicyConfig {
        fn clone(&self) -> Self {
            Self(self.0.clone())
        }
    }

    unsafe impl ComInterface for IPolicyConfig {
        const IID: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
    }

    impl IPolicyConfig {
        /// 唯一的实调用。设备 id 是宽字符、以 NUL 结尾。
        unsafe fn set_default(&self, id: &[u16], role: i32) -> HRESULT {
            (self.vtable().SetDefaultEndpoint)(self.as_raw(), PCWSTR(id.as_ptr()), role_of(role))
        }
    }

    /// 一次 COM 会话。`CoInitializeEx` 在最外层调一次，失败（已经是别的
    /// apartment）不影响 —— 这里只要求"能 CoCreateInstance"。
    fn with_com<T>(f: impl FnOnce(&IMMDeviceEnumerator) -> Result<T, String>) -> Result<T, String> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|err| format!("CoCreateInstance(MMDeviceEnumerator): {err}"))?;
            f(&enumerator)
        }
    }

    fn policy_config() -> Result<IPolicyConfig, String> {
        unsafe {
            CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL)
                .map_err(|err| format!("CoCreateInstance(IPolicyConfig): {err}"))
        }
    }

    /// 读端点的友好名。与 `virtual_mic` 里那段一致：这个 crate 版本没有
    /// `PropVariantToStringAlloc`，所以直接读联合体的两种字符串类型。
    unsafe fn friendly_name(
        device: &windows::Win32::Media::Audio::IMMDevice,
    ) -> Option<String> {
        use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
        use windows::Win32::System::Com::StructuredStorage::{
            PropVariantClear, PROPVARIANT,
        };
        use windows::Win32::System::Com::STGM_READ;
        use windows::Win32::System::Variant::{VT_BSTR, VT_LPWSTR};
        use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

        let store: IPropertyStore = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut value: PROPVARIANT = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
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
        name
    }

    /// 端点 id（`{0.0.0.00000000}.{guid}`）。`GetId` 分配在 CoTaskMem 上，
    /// 必须自己释放 —— 这条路径每次启动只走几次，但循环里不释放会稳定泄漏。
    unsafe fn device_id(device: &windows::Win32::Media::Audio::IMMDevice) -> Option<String> {
        let raw: PWSTR = device.GetId().ok()?;
        let text = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0 as *const c_void));
        text
    }

    /// 当前默认录音（友好名）+ 三个角色各自的 id。
    pub fn snapshot() -> (Option<String>, Vec<(i32, String)>) {
        with_com(|enumerator| {
            let mut ids = Vec::new();
            let mut name = None;
            for role in super::ROLES {
                let Ok(device) = (unsafe {
                    enumerator.GetDefaultAudioEndpoint(eCapture, role_of(role))
                }) else {
                    continue;
                };
                if let Some(id) = unsafe { device_id(&device) } {
                    if role == ROLE_CONSOLE {
                        name = unsafe { friendly_name(&device) };
                    }
                    ids.push((role, id));
                }
            }
            Ok((name, ids))
        })
        .unwrap_or((None, Vec::new()))
    }

    fn role_of(role: i32) -> ERole {
        match role {
            ROLE_CONSOLE => eConsole,
            ROLE_MULTIMEDIA => eMultimedia,
            ROLE_COMMUNICATIONS => eCommunications,
            _ => eConsole,
        }
    }

    /// 按友好名找录音端点并返回 id。
    fn capture_id(enumerator: &IMMDeviceEnumerator, want: &str) -> Result<String, String> {
        unsafe {
            let list: IMMDeviceCollection = enumerator
                .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
                .map_err(|err| format!("EnumAudioEndpoints(capture): {err}"))?;
            let mut seen = Vec::new();
            for i in 0..list.GetCount().unwrap_or(0) {
                let Ok(device) = list.Item(i) else { continue };
                let Some(name) = friendly_name(&device) else { continue };
                if name.eq_ignore_ascii_case(want) {
                    return device_id(&device)
                        .ok_or_else(|| format!("{name} 的 GetId 失败"));
                }
                seen.push(name);
            }
            Err(format!("录音端点里找不到 {want:?}（现有：{}）", seen.join(" / ")))
        }
    }

    /// 切到指定录音端点，返回切换前的 `[(role, id)]` 供还原。
    ///
    /// 中途失败时把**已经改掉的**角色立刻还原：半切状态（多媒体切了、通信没切）
    /// 比完全不切更难排障 —— 游戏听不见、Discord 却能听见。
    pub fn switch_capture(name: &str, saved: &[(i32, String)]) -> Result<Vec<(i32, String)>, String> {
        let config = policy_config()?;
        let id = with_com(|enumerator| capture_id(enumerator, name))?;
        let wide = wide(id.as_str());

        let mut applied: Vec<i32> = Vec::new();
        for role in super::ROLES {
            let hr = unsafe { config.set_default(&wide, role) };
            if hr.is_err() {
                let mut rolled_back = 0usize;
                for done in &applied {
                    if let Some((_, old)) = saved.iter().find(|(r, _)| r == done) {
                        if set_default_capture(old, *done).is_ok() {
                            rolled_back += 1;
                        }
                    }
                }
                return Err(format!(
                    "SetDefaultEndpoint({name}, role={}) 失败：{}；已回滚 {rolled_back}/{} 个角色",
                    super::role_name(role),
                    hr.message(),
                    applied.len()
                ));
            }
            applied.push(role);
        }
        Ok(saved.to_vec())
    }

    /// 把某个角色还原到指定 id。
    pub fn set_default_capture(id: &str, role: i32) -> Result<(), String> {
        let config = policy_config()?;
        let wide = wide(id);
        let hr = unsafe { config.set_default(&wide, role) };
        if hr.is_err() {
            return Err(format!("SetDefaultEndpoint({id}): {}", hr.message()));
        }
        Ok(())
    }

    /// Rust 1.75 没有 `let ... else` 的管道；这里只需要一个把字符串转
    /// NUL 结尾宽字符的小工具。
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `HRESULT::message()` 在 0.52 里是自由函数形式，包一层省得每处写
    /// `Error::from(hr).message()`。
    trait HResultMessage {
        fn message(&self) -> String;
    }

    impl HResultMessage for HRESULT {
        fn message(&self) -> String {
            windows::core::Error::from(*self).message().to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::virtual_mic::Target;

    fn cable() -> Target {
        Target {
            provider: Provider::VbCable,
            sink: Some("CABLE Input (VB-Audio Virtual Cable)".to_owned()),
            capture: Some("CABLE Output (VB-Audio Virtual Cable)".to_owned()),
            usable: true,
            note: String::new(),
        }
    }

    #[test]
    fn off_never_touches_system_settings() {
        assert_eq!(plan(Policy::Off, &cable(), None), Decision::Disabled);
    }

    #[test]
    fn an_unusable_target_is_not_applied() {
        // 没装线缆时把默认录音切到"不存在的设备"只会让排障更难。
        let mut t = cable();
        t.usable = false;
        let decision = plan(Policy::Auto, &t, None);
        assert!(matches!(decision, Decision::NotApplicable(_)), "{decision:?}");
    }

    #[test]
    fn talkback_mode_has_nothing_to_switch_to() {
        let t = Target {
            provider: Provider::Default,
            sink: None,
            capture: None,
            usable: true,
            note: String::new(),
        };
        let decision = plan(Policy::Auto, &t, Some("Mic (Realtek)"));
        assert!(matches!(decision, Decision::NotApplicable(_)), "{decision:?}");
    }

    #[test]
    fn a_missing_capture_endpoint_is_reported_not_guessed() {
        // 线缆的 render 端在、录音端被禁用：不能瞎切。
        let mut t = cable();
        t.capture = None;
        let decision = plan(Policy::Auto, &t, None);
        assert!(matches!(decision, Decision::NotApplicable(_)), "{decision:?}");
    }

    #[test]
    fn already_the_default_is_left_alone() {
        let t = cable();
        let decision = plan(
            Policy::Auto,
            &t,
            Some("CABLE Output (VB-Audio Virtual Cable)"),
        );
        assert_eq!(
            decision,
            Decision::Already("CABLE Output (VB-Audio Virtual Cable)".to_owned())
        );
    }

    #[test]
    fn the_comparison_ignores_driver_case_differences() {
        let t = cable();
        let decision = plan(Policy::Auto, &t, Some("cable output (vb-audio virtual cable)"));
        assert!(matches!(decision, Decision::Already(_)), "{decision:?}");
    }

    #[test]
    fn a_physical_default_is_switched() {
        let t = cable();
        let decision = plan(Policy::Auto, &t, Some("Microphone (Realtek Audio)"));
        assert_eq!(
            decision,
            Decision::Switch("CABLE Output (VB-Audio Virtual Cable)".to_owned())
        );
    }

    #[test]
    fn policy_parsing_covers_the_names_operators_actually_type() {
        assert_eq!(Policy::from_name("auto"), Some(Policy::Auto));
        assert_eq!(Policy::from_name(""), Some(Policy::Auto));
        assert_eq!(Policy::from_name("off"), Some(Policy::Off));
        assert_eq!(Policy::from_name("keep"), Some(Policy::Off));
        assert_eq!(Policy::from_name("nonsense"), None);
    }

    #[test]
    fn role_names_are_readable_in_logs() {
        assert_eq!(role_name(ROLE_CONSOLE), "console");
        assert_eq!(role_name(ROLE_COMMUNICATIONS), "communications");
        assert_eq!(role_name(99), "unknown");
    }

    /// 测试用临时文件。并行测试会同时跑，名字里必须带不同的后缀。
    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("mic-default-{tag}-{}.json", std::process::id()))
    }

    #[test]
    fn the_restore_file_round_trips_both_roles() {
        let path = temp_path("round-trip");
        let entries = vec![
            (ROLE_CONSOLE, "{0.0.0.00000000}.{aaa}".to_owned()),
            (ROLE_COMMUNICATIONS, "{0.0.0.00000000}.{bbb}".to_owned()),
        ];
        write_state(&path, "CABLE Output (VB-Audio Virtual Cable)", &entries);
        assert_eq!(read_state(&path), Some(entries));
        remove_state(&path);
        assert!(read_state(&path).is_none(), "删掉之后不能再读到");
    }

    #[test]
    fn a_corrupt_restore_file_is_ignored_not_fatal() {
        // 这个文件是备忘，不是配置：读不懂就当没有，绝不能拦住启动。
        let path = temp_path("corrupt");
        std::fs::write(&path, "{not json at all").unwrap();
        assert!(read_state(&path).is_none());

        std::fs::write(&path, r#"{"previous":[]}"#).unwrap();
        assert!(read_state(&path).is_none(), "空列表等于没有原值");

        std::fs::write(&path, r#"{"previous":[{"role":0}]}"#).unwrap();
        assert!(read_state(&path).is_none(), "缺 id 的条目不能瞎猜");
        remove_state(&path);
    }
}
