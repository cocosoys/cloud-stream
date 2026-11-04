//! Hardware-encoder capability probe — **reserved, not yet an encoder**.
//!
//! The aim is a single honest answer to "would this machine have a hardware
//! encoder if we asked for one", computed at startup and written to the log, so
//! that upgrading the GPU is a hardware change and not a code change. Today the
//! answer is only *advisory*: `scrap`'s `hwcodec` feature is not built, so
//! [`decide`] always lands on software and says why. When the feature is built,
//! the only thing that has to change is the `Backend::Hardware` arm below.
//!
//! # Why enumerate the adapter instead of probing the encoder API
//!
//! Opening `nvEncodeAPI64.dll` at startup to "see if it works" is exactly the
//! kind of probe that lies: it succeeds on a laptop whose dGPU is powered down,
//! and it fails on a machine where the driver is fine but no session exists yet.
//! The adapter list is static data the OS already has, so the probe is cheap,
//! side-effect free, and cannot be confused by power state. The real "does it
//! encode" question is answered where it belongs — at open time, when the
//! encoder is actually constructed.
//!
//! # Why the generation table is name-based
//!
//! The PCI device id (`VEN_10DE&DEV_1B81`) identifies the chip exactly, but
//! turning it into "NVENC yes/no" needs a per-chip table that goes stale the
//! moment a new generation ships. The adapter *name* ("NVIDIA GeForce GTX
//! 1060") carries the marketing tier, which is what NVIDIA/AMD/Intel use to
//! decide whether the part has an encoder, and it is what the user asked for
//! ("GTX 10 系及以上"). Anything the table does not recognise reports
//! [`Support::Unknown`] and is **not** silently promoted to "yes" — a wrong
//! "yes" would send a session to an encoder that fails at open time, while a
//! wrong "no" only costs the optimisation.
//!
//! # Thresholds (as specified, not as marketed)
//!
//! * NVIDIA GTX 10 series and up → NVENC.
//! * AMD RX 400 and up → AMF (VCE 3.4+ exposes an AMF encoder).
//! * Intel integrated (HD/UHD/Iris/Arc) → QSV, with the caveat that it is the
//!   weakest of the three at 1080p60 and is the last resort, not the first.

use std::path::PathBuf;

/// Who made the adapter. Taken from the PCI vendor id when the device is a
/// real PCI device, so it stays correct even when the driver's display name is
/// localised or missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    /// VMs (`VEN_15AD` VMware, `VEN_1234` QEMU), `Microsoft Basic Render
    /// Driver`, and anything unrecognised. No hardware encoder, ever.
    Other,
}

impl Vendor {
    fn from_device_id(device_id: &str) -> Self {
        // `PCI\VEN_10DE&DEV_...`; the vendor id is the only part that is
        // stable across drivers and locales.
        let upper = device_id.to_ascii_uppercase();
        if upper.contains("VEN_10DE") {
            Self::Nvidia
        } else if upper.contains("VEN_1002") || upper.contains("VEN_1022") {
            // 1002 = AMD/ATI graphics, 1022 = AMD CPU-rooted devices.
            Self::Amd
        } else if upper.contains("VEN_8086") {
            Self::Intel
        } else {
            Self::Other
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nvidia => "nvidia",
            Self::Amd => "amd",
            Self::Intel => "intel",
            Self::Other => "other",
        }
    }
}

/// The three hardware encoders this project would target, in preference order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoder {
    /// NVIDIA, GTX 10 series and up.
    Nvenc,
    /// AMD, RX 400 and up.
    Amf,
    /// Intel iGPU, weakest at 1080p60.
    Qsv,
}

impl Encoder {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nvenc => "nvenc",
            Self::Amf => "amf",
            Self::Qsv => "qsv",
        }
    }

    /// Parse a `--encoder` value. `auto` and `software` are handled by
    /// [`Requested`]; this only names the three hardware ones.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "nvenc" | "nvidia" => Some(Self::Nvenc),
            "amf" | "amd" => Some(Self::Amf),
            "qsv" | "intel" => Some(Self::Qsv),
            _ => None,
        }
    }

    /// The user-mode runtime that has to be on disk for this encoder to be
    /// openable. Presence proves the driver installed its media stack; absence
    /// is the difference between "no hardware" and "hardware, broken driver".
    fn runtime_files(self) -> &'static [&'static str] {
        match self {
            // Shipped by every NVIDIA driver; lives in System32.
            Self::Nvenc => &["nvEncodeAPI64.dll"],
            // AMD's AMF runtime, System32 on current drivers.
            Self::Amf => &["amfrt64.dll"],
            // Modern drivers expose oneVPL (`libvpl.dll`); older ones shipped
            // the Media SDK runtime. Either is enough to try.
            Self::Qsv => &["libvpl.dll", "libmfxhw64.dll", "mfx_mft_h264ve_64.dll"],
        }
    }
}

/// Whether the adapter can encode, per the thresholds in the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    /// Meets the bar.
    Yes,
    /// Same vendor, older silicon — it has *an* encoder but below the bar the
    /// project targets, so it is reported and then ignored.
    TooOld,
    /// Vendor recognised, tier not in the table. Deliberately not promoted to
    /// `Yes`: see the module docs.
    Unknown,
}

impl Support {
    fn as_str(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::TooOld => "too-old",
            Self::Unknown => "unknown",
        }
    }
}

/// One display adapter as the OS reports it.
#[derive(Clone, Debug)]
pub struct Adapter {
    /// Driver display name, e.g. `NVIDIA GeForce RTX 3060`.
    pub name: String,
    /// `PCI\VEN_10DE&DEV_2504&...`, empty for non-PCI adapters.
    pub device_id: String,
    pub vendor: Vendor,
    pub support: Support,
    /// Attached to the interactive desktop. A headless dGPU still encodes, so
    /// this is informational — it is what explains "GPU present, 0 fps".
    pub attached: bool,
}

impl Adapter {
    /// The encoder this adapter would use, if its support allows it.
    fn encoder(&self) -> Option<Encoder> {
        if self.support != Support::Yes {
            return None;
        }
        match self.vendor {
            Vendor::Nvidia => Some(Encoder::Nvenc),
            Vendor::Amd => Some(Encoder::Amf),
            Vendor::Intel => Some(Encoder::Qsv),
            Vendor::Other => None,
        }
    }
}

/// What `--encoder` asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requested {
    /// Pick the best available; never fails.
    Auto,
    /// Never use hardware.
    Software,
    /// A specific encoder; fails loudly when it is not there.
    Hardware(Encoder),
}

impl Requested {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "software" | "sw" | "vpx" => Some(Self::Software),
            other => Encoder::from_name(other).map(Self::Hardware),
        }
    }
}

/// The full probe result: every adapter plus the one encoder it would pick.
#[derive(Clone, Debug)]
pub struct Report {
    pub adapters: Vec<Adapter>,
    /// `Some` when an adapter met the bar. Still advisory — see [`decide`].
    pub candidate: Option<(Encoder, String)>,
}

impl Report {
    /// One line for the startup log. Deliberately includes the adapter name even
    /// when there is no candidate: "no hardware" and "hardware we do not
    /// recognise" are different problems and only the name tells them apart.
    pub fn summary(&self) -> String {
        let adapters = if self.adapters.is_empty() {
            "none visible to this session".to_owned()
        } else {
            self.adapters
                .iter()
                .map(|a| {
                    format!(
                        "{} [{} {}]",
                        a.name,
                        a.vendor.as_str(),
                        a.support.as_str()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        match &self.candidate {
            Some((enc, name)) => {
                format!("candidate={} on {:?}; adapters: {}", enc.as_str(), name, adapters)
            }
            None => format!("candidate=none; adapters: {}", adapters),
        }
    }

    /// Per-adapter detail, one line each, for the second log line. Kept separate
    /// from [`Self::summary`] so the common "one VMware adapter" case stays one
    /// readable line.
    pub fn detail(&self) -> Vec<String> {
        self.adapters
            .iter()
            .map(|a| {
                format!(
                    "adapter {:?} vendor={} support={} attached={} id={}",
                    a.name,
                    a.vendor.as_str(),
                    a.support.as_str(),
                    a.attached,
                    if a.device_id.is_empty() { "-" } else { &a.device_id },
                )
            })
            .collect()
    }
}

/// Enumerate the display adapters and pick a candidate.
///
/// Never fails: a machine with no enumerable adapter (`Display::all()` empty in
/// session 0, a bare VM) yields an empty list, which is a legitimate answer.
pub fn probe() -> Report {
    let adapters = enumerate();
    // Preference order is discrete-first: NVIDIA for the strongest NVENC, then
    // AMD, then Intel as the documented last resort. Within one vendor the
    // first adapter that qualifies wins — vendor ordering is what matters here,
    // not which slot the card is in.
    let mut candidate = None;
    for vendor in [Vendor::Nvidia, Vendor::Amd, Vendor::Intel] {
        if let Some(a) = adapters.iter().find(|a| a.vendor == vendor) {
            if let Some(enc) = a.encoder() {
                candidate = Some((enc, a.name.clone()));
                break;
            }
        }
    }
    Report { adapters, candidate }
}

/// The startup decision: which backend the session will actually use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// libvpx, the only encoder this build has.
    Software,
    /// Reserved for the `hwcodec` build; nothing constructs this yet.
    Hardware(Encoder),
}

/// Resolve `--encoder` against the probe. `note` is meant for the log: it says
/// which backend won *and* why the other one lost, so a support report contains
/// the answer without a second run.
pub fn decide(requested: Requested, report: &Report) -> anyhow::Result<(Backend, String)> {
    let unavailable = |enc: Encoder| -> String {
        match report.adapters.iter().find(|a| a.vendor == vendor_of(enc)) {
            None => format!("no {} adapter present", vendor_of(enc).as_str()),
            Some(a) => format!(
                "{} present as {:?} but support={} (need a newer GPU)",
                vendor_of(enc).as_str(),
                a.name,
                a.support.as_str()
            ),
        }
    };

    match requested {
        Requested::Software => Ok((
            Backend::Software,
            "software requested (hardware encoding disabled by --encoder)".to_owned(),
        )),
        Requested::Auto => match &report.candidate {
            // Detection succeeded but this build cannot open a hardware encoder.
            // Saying so is the whole point of the reservation: the operator sees
            // "GPU is fine, the binary is old" instead of a mystery.
            Some((enc, name)) => Ok((
                Backend::Software,
                format!(
                    "{} detected on {:?} but hardware encoding is not built into this binary \
                     (build with --features hwcodec); using software",
                    enc.as_str(),
                    name
                ),
            )),
            // An empty adapter list is not "no hardware" — it is "this session
            // cannot see any hardware". Session 0 (a Windows service, a task
            // started before logon, an ssh command) enumerates zero display
            // devices, and reporting that as "the box has no GPU" would be a
            // lie that sends someone shopping for a card they already own.
            None if report.adapters.is_empty() => Ok((
                Backend::Software,
                "no display adapter visible to this session (session 0 or headless) — hardware \
                 capability unknown; using software"
                    .to_owned(),
            )),
            None => Ok((
                Backend::Software,
                format!(
                    "no hardware encoder among {} adapter(s); using software",
                    report.adapters.len()
                ),
            )),
        },
        Requested::Hardware(enc) => {
            if report.candidate.as_ref().map(|(e, _)| *e) != Some(enc) {
                // An explicit request that cannot be honoured is a
                // configuration error, not a hint: fail loudly at startup
                // rather than quietly streaming at a quality the operator did
                // not choose.
                anyhow::bail!("--encoder {} unavailable: {}", enc.as_str(), unavailable(enc));
            }
            if !runtime_present(enc) {
                anyhow::bail!(
                    "--encoder {} unavailable: {} reports the GPU but the driver's runtime \
                     ({}) is missing — reinstall the graphics driver",
                    enc.as_str(),
                    vendor_of(enc).as_str(),
                    enc.runtime_files().join(" / "),
                );
            }
            Ok((
                Backend::Software,
                format!(
                    "--encoder {} requested: GPU and driver are ready, but hardware encoding is \
                     not built into this binary (build with --features hwcodec); using software",
                    enc.as_str()
                ),
            ))
        }
    }
}

fn vendor_of(enc: Encoder) -> Vendor {
    match enc {
        Encoder::Nvenc => Vendor::Nvidia,
        Encoder::Amf => Vendor::Amd,
        Encoder::Qsv => Vendor::Intel,
    }
}

/// Whether any of the encoder's user-mode runtime DLLs is on disk.
fn runtime_present(enc: Encoder) -> bool {
    let dir = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32");
    enc.runtime_files().iter().any(|f| dir.join(f).is_file())
}

/// Classify one adapter from its driver name and PCI id.
fn classify(name: &str, device_id: &str) -> Support {
    let vendor = Vendor::from_device_id(device_id);
    let upper = name.to_ascii_uppercase();
    match vendor {
        Vendor::Nvidia => {
            // Marketing tier, not chip codename: RTX covers Turing/Ampere/Ada,
            // GTX 16 is Turing without the RT cores and still has NVENC, GTX 10
            // is Pascal. Everything below that is the same hardware family the
            // user explicitly excluded.
            if upper.contains("RTX") || upper.contains("GTX 10") || upper.contains("GTX 16") {
                Support::Yes
            } else if upper.contains("MX") {
                // MX150/250/350/450/550 are laptop parts derived from Pascal
                // and later — they do have NVENC.
                Support::Yes
            } else if upper.contains("QUADRO") || upper.contains("TESLA") {
                // Workstation/server parts track the desktop generation.
                Support::Yes
            } else if upper.contains("GTX 9")
                || upper.contains("GTX 7")
                || upper.contains("GTX 6")
                || upper.contains("GT 7")
                || upper.contains("GT 6")
            {
                Support::TooOld
            } else {
                Support::Unknown
            }
        }
        Vendor::Amd => {
            // RX 400 (Polaris) is the floor for a usable AMF H.264 encoder;
            // Vega and every later RX clears it. The R5/R7/R9 rebrands do not.
            if upper.contains("RX 4")
                || upper.contains("RX 5")
                || upper.contains("RX 6")
                || upper.contains("RX 7")
                || upper.contains("RX 9")
                || upper.contains("VEGA")
                || upper.contains("RADEON VII")
                || upper.contains("RADEON PRO")
            {
                Support::Yes
            } else if upper.contains("R9 ") || upper.contains("R7 ") || upper.contains("R5 ") {
                Support::TooOld
            } else {
                Support::Unknown
            }
        }
        Vendor::Intel => {
            // Every Intel iGPU since Sandy Bridge has QuickSync; the bar here is
            // "usable at 1080p60", which the module docs already rank last.
            if upper.contains("ARC")
                || upper.contains("IRIS")
                || upper.contains("UHD")
                || upper.contains("HD GRAPHICS")
            {
                Support::Yes
            } else {
                Support::Unknown
            }
        }
        Vendor::Other => Support::TooOld,
    }
}

/// `EnumDisplayDevicesW(NULL, i, ..)` over the adapter list.
///
/// Adapters, not monitors: `DISPLAY_DEVICE_ATTACHED_TO_DESKTOP` is kept so the
/// list explains "no display" separately from "no GPU", and the mirroring
/// pseudo-driver (`DISPLAY_DEVICE_MIRRORING_DRIVER`) is dropped because it is
/// not hardware.
fn enumerate() -> Vec<Adapter> {
    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::Graphics::Gdi::{
        EnumDisplayDevicesW, DISPLAY_DEVICEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP,
        DISPLAY_DEVICE_MIRRORING_DRIVER,
    };

    let mut out = Vec::new();
    for index in 0..32u32 {
        let mut dd: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
        dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
        let ok: BOOL = unsafe { EnumDisplayDevicesW(std::ptr::null(), index, &mut dd, 0) };
        if ok == 0 {
            break;
        }
        if (dd.StateFlags & DISPLAY_DEVICE_MIRRORING_DRIVER) != 0 {
            continue;
        }
        let name = wide_to_string(&dd.DeviceString);
        let device_id = wide_to_string(&dd.DeviceID);
        if name.is_empty() {
            continue;
        }
        // One physical adapter shows up once per virtual display: a VMware guest
        // with 8 virtual monitors reports the same `VEN_15AD&DEV_0405` eight
        // times, which turned the startup line into eight identical entries.
        // The PCI id is the identity; the name is the fallback for adapters that
        // have no id at all.
        let duplicate = out.iter().any(|a: &Adapter| {
            if device_id.is_empty() {
                a.name == name && a.device_id.is_empty()
            } else {
                a.device_id == device_id
            }
        });
        if duplicate {
            continue;
        }
        out.push(Adapter {
            vendor: Vendor::from_device_id(&device_id),
            support: classify(&name, &device_id),
            attached: (dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP) != 0,
            name,
            device_id,
        });
    }
    out
}

/// Decode a fixed-size, NUL-terminated UTF-16 buffer into a `String`.
fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_comes_from_the_pci_id_not_the_name() {
        // A localised or blank display name must not change the vendor.
        assert_eq!(
            Vendor::from_device_id(r"PCI\VEN_10DE&DEV_2504&SUBSYS_00000000"),
            Vendor::Nvidia
        );
        assert_eq!(Vendor::from_device_id(r"PCI\VEN_1002&DEV_67DF"), Vendor::Amd);
        assert_eq!(Vendor::from_device_id(r"PCI\VEN_8086&DEV_4692"), Vendor::Intel);
        assert_eq!(Vendor::from_device_id(r"PCI\VEN_15AD&DEV_0405"), Vendor::Other);
        assert_eq!(Vendor::from_device_id(""), Vendor::Other);
    }

    #[test]
    fn thresholds_match_the_spec() {
        // Every case carries a real PCI id: the vendor comes from the id, so a
        // name-only fixture would test the wrong branch (see the module docs).
        const NV: &str = r"PCI\VEN_10DE&DEV_1B81";
        const AMD: &str = r"PCI\VEN_1002&DEV_67DF";
        const INTEL: &str = r"PCI\VEN_8086&DEV_3E9B";

        // NVIDIA: GTX 10 is the floor, and it is inclusive.
        assert_eq!(classify("NVIDIA GeForce GTX 1060", NV), Support::Yes);
        assert_eq!(classify("NVIDIA GeForce GTX 1660 Ti", NV), Support::Yes);
        assert_eq!(classify("NVIDIA GeForce RTX 4070", NV), Support::Yes);
        assert_eq!(classify("NVIDIA GeForce GTX 960", NV), Support::TooOld);

        // AMD: RX 400 is the floor.
        assert_eq!(classify("Radeon RX 480", AMD), Support::Yes);
        assert_eq!(classify("Radeon RX 7900 XTX", AMD), Support::Yes);
        assert_eq!(classify("Radeon R9 390", AMD), Support::TooOld);

        // Intel: any current iGPU qualifies for QSV.
        assert_eq!(classify("Intel(R) UHD Graphics 630", INTEL), Support::Yes);
        assert_eq!(classify("Intel(R) Arc(TM) A770 Graphics", INTEL), Support::Yes);
    }

    #[test]
    fn unknown_is_not_promoted_to_yes() {
        // The whole point of the third state: an unrecognised name must not send
        // a session to an encoder that would fail at open time.
        assert_eq!(
            classify("NVIDIA Graphics Device", r"PCI\VEN_10DE&DEV_9999"),
            Support::Unknown
        );
        assert_eq!(
            classify("Some Future GPU", r"PCI\VEN_10DE&DEV_FFFF"),
            Support::Unknown
        );
    }

    #[test]
    fn vm_adapter_is_not_a_candidate() {
        let report = Report {
            adapters: vec![Adapter {
                name: "VMware SVGA 3D".to_owned(),
                device_id: r"PCI\VEN_15AD&DEV_0405".to_owned(),
                vendor: Vendor::from_device_id(r"PCI\VEN_15AD&DEV_0405"),
                support: Support::TooOld,
                attached: true,
            }],
            candidate: None,
        };
        assert!(report.candidate.is_none());
        let (backend, note) = decide(Requested::Auto, &report).unwrap();
        assert_eq!(backend, Backend::Software);
        assert!(note.contains("no hardware encoder"), "note={note}");
    }

    #[test]
    fn auto_on_detected_hardware_still_says_software_is_why() {
        // Detection succeeding must not be mistaken for "hardware encoding is on".
        let adapters = vec![Adapter {
            name: "NVIDIA GeForce RTX 3060".to_owned(),
            device_id: r"PCI\VEN_10DE&DEV_2504".to_owned(),
            vendor: Vendor::Nvidia,
            support: Support::Yes,
            attached: true,
        }];
        let report = Report {
            candidate: Some((Encoder::Nvenc, "NVIDIA GeForce RTX 3060".to_owned())),
            adapters,
        };
        let (backend, note) = decide(Requested::Auto, &report).unwrap();
        assert_eq!(backend, Backend::Software);
        assert!(note.contains("not built into this binary"), "note={note}");
    }

    #[test]
    fn empty_adapter_list_says_unknown_not_absent() {
        // Session 0 sees no display devices at all. Reporting that as "this
        // machine has no GPU" would be wrong in the one case that matters
        // (a service started before logon), so the wording has to differ.
        let report = Report { adapters: vec![], candidate: None };
        let (backend, note) = decide(Requested::Auto, &report).unwrap();
        assert_eq!(backend, Backend::Software);
        assert!(note.contains("unknown"), "note={note}");
        assert!(note.contains("session 0"), "note={note}");
    }

    #[test]
    fn explicit_hardware_request_fails_loudly_when_absent() {
        let report = Report { adapters: vec![], candidate: None };
        let err = decide(Requested::Hardware(Encoder::Nvenc), &report)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--encoder nvenc unavailable"), "err={err}");
        assert!(err.contains("no nvidia adapter"), "err={err}");
    }
}
