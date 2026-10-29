//! #89 host display-mode control: make the captured desktop match the client's
//! viewport instead of scaling it down.
//!
//! Why this exists
//! ---------------
//! `r,<W>x<H>` (#50) only *scales* the captured frame: the host keeps its real
//! resolution and libyuv squeezes the 1920x1080 desktop into the requested
//! buffer. When the browser's stream box is 1714x922 CSS px, the frame the
//! browser draws is `1.12` frame pixels per CSS pixel, and the pointer math has
//! to divide by that ratio on the absolute path. The relative path (`m2`)
//! deliberately does not (see `input.js::_relativeToServer`), so a locked
//! pointer ends up ~10.7 % slower than the hand — and the desktop text is
//! resampled, which is visible.
//!
//! Changing the real display mode removes the ratio at the source: the host
//! desktop becomes 1714x922, capture is 1:1, the encoder does no scaling, and a
//! CSS pixel is a screen pixel on both pointer paths.
//!
//! Safety rules baked in here
//! --------------------------
//! 1. `ChangeDisplaySettingsW` is a *global* side effect — it reflows the host
//!    desktop — so it only runs when a client explicitly asks for it.
//! 2. Every candidate mode is `CDS_TEST`-ed first. A mode that fails the test
//!    never reaches the screen.
//! 3. Modes the adapter reports are preferred. `CDS_ENABLE_UNSAFE_MODES` (which
//!    most Intel/NVIDIA/AMD desktop drivers honour, and which is what "custom
//!    resolution" utilities use) is the fallback, never the first choice.
//! 4. If neither works, the largest reported mode that still fits inside the
//!    request is applied and reported as inexact, so the caller can decide to
//!    fall back to plain scaling.
//! 5. The pre-change mode is remembered once, so `restore()` can put the host
//!    back and a stray reset cannot reflow a desktop the stream never touched.
//!
//! Only the primary display is handled: `ChangeDisplaySettingsW` with a null
//! device name addresses it. Multi-monitor setups need
//! `EnumDisplayDevicesW` + `ChangeDisplaySettingsExW` per device, which nothing
//! in this stage needs yet.

use std::sync::Mutex;

use windows_sys::Win32::Graphics::Gdi::{
    CDS_ENABLE_UNSAFE_MODES, CDS_TEST, CDS_UPDATEREGISTRY, ChangeDisplaySettingsW, DEVMODEW,
    DISP_CHANGE_SUCCESSFUL, DM_PELSHEIGHT, DM_PELSWIDTH, EnumDisplaySettingsW,
    ENUM_CURRENT_SETTINGS,
};

/// Lowest side worth switching a desktop to; below this the host UI stops being
/// usable and the request is treated as nonsense.
const MIN_SIDE: u32 = 320;
/// Above any plausible panel: the desktop would be upscaled by the GPU and the
/// stream would carry invented pixels.
const MAX_SIDE: u32 = 8_192;

/// Result of a successful mode switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// What the client asked for.
    pub requested: (u32, u32),
    /// What the host is actually at afterwards.
    pub applied: (u32, u32),
    /// `applied == requested`. False means a nearby reported mode was used.
    pub exact: bool,
    /// Which strategy won, for the log line.
    pub strategy: &'static str,
}

/// The desktop mode in force *before* the first switch of this process.
/// `None` means nothing has been changed yet, so `restore()` has nothing to do.
static ORIGINAL: Mutex<Option<(u32, u32)>> = Mutex::new(None);

fn zeroed_devmode() -> DEVMODEW {
    // DEVMODEW has no `Default`; every field must start at zero so the driver
    // reads "unspecified" for everything we do not touch.
    unsafe { std::mem::zeroed() }
}

/// The desktop mode in force right now, or `None` when enumeration fails (a
/// headless / service session with no active console).
pub fn current() -> Option<(u32, u32)> {
    let mut dm = zeroed_devmode();
    dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    let ok = unsafe { EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut dm) };
    if ok == 0 {
        return None;
    }
    Some((dm.dmPelsWidth, dm.dmPelsHeight))
}

/// Every distinct `(width, height)` the adapter reports, largest first.
fn reported_modes() -> Vec<(u32, u32)> {
    let mut modes = Vec::new();
    // The mode index is a plain u32; enumeration ends at the first failure
    // rather than at a documented count. 512 is far above any real list.
    for index in 0u32..512 {
        let mut dm = zeroed_devmode();
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        let ok = unsafe { EnumDisplaySettingsW(std::ptr::null(), index, &mut dm) };
        if ok == 0 {
            break;
        }
        let size = (dm.dmPelsWidth, dm.dmPelsHeight);
        if size.0 >= MIN_SIDE && size.1 >= MIN_SIDE && !modes.contains(&size) {
            modes.push(size);
        }
    }
    modes.sort_by_key(|(w, h)| std::cmp::Reverse(*w as u64 * *h as u64));
    modes
}

/// Put the desktop into `width` x `height`.
///
/// `Ok(Applied)` means the desktop is at `applied` afterwards — check `exact`
/// before claiming it is the requested size. `Err` carries a sentence for the
/// log/UI; a driver refusing a mode is a normal outcome, not a bug.
pub fn apply(width: u32, height: u32) -> Result<Applied, String> {
    if !(MIN_SIDE..=MAX_SIDE).contains(&width) || !(MIN_SIDE..=MAX_SIDE).contains(&height) {
        return Err(format!(
            "refusing {width}x{height}: outside {MIN_SIDE}..={MAX_SIDE}"
        ));
    }

    let now = current();
    remember_original(now);
    if now == Some((width, height)) {
        return Ok(Applied {
            requested: (width, height),
            applied: (width, height),
            exact: true,
            strategy: "already",
        });
    }

    let reported = reported_modes();
    let mut last_err = String::from("no candidate mode");

    // Order matters: a mode the adapter advertises needs no unsafe flag and is
    // the only kind guaranteed to survive an EDID re-read.
    if reported.contains(&(width, height)) {
        match try_mode(width, height, false) {
            Ok(()) => return Ok(finish(width, height, "reported")),
            Err(err) => last_err = err,
        }
    } else {
        // Not in the advertised list: ask the driver for it anyway. Most desktop
        // drivers scale internally and accept, which is the whole point.
        match try_mode(width, height, true) {
            Ok(()) => return Ok(finish(width, height, "unsafe")),
            Err(err) => last_err = err,
        }
    }

    // Last resort: the largest reported mode that still fits inside the
    // request. Scaling comes back, but only by what the adapter forces.
    if let Some((w, h)) = reported
        .iter()
        .copied()
        .find(|(w, h)| *w <= width && *h <= height)
    {
        if try_mode(w, h, false).is_ok() {
            return Ok(finish(width, height, "nearest-reported"));
        }
    }

    Err(last_err)
}

/// Put the desktop back to the mode it had before the first `apply()`.
///
/// `Err` when nothing was ever changed, so a stray `dr,reset` cannot reflow a
/// desktop this process never touched.
pub fn restore() -> Result<(u32, u32), String> {
    let original = {
        let slot = ORIGINAL
            .lock()
            .map_err(|_| String::from("display mode lock poisoned"))?;
        (*slot).ok_or_else(|| String::from("no original mode recorded yet"))?
    };
    try_mode(original.0, original.1, false)
        .map_err(|err| format!("restore to {}x{} failed: {err}", original.0, original.1))?;
    Ok(original)
}

fn finish(width: u32, height: u32, strategy: &'static str) -> Applied {
    let applied = current().unwrap_or((width, height));
    Applied {
        requested: (width, height),
        applied,
        exact: applied == (width, height),
        strategy,
    }
}

/// `CDS_TEST` then apply. The test is not optional: it is the only thing
/// standing between a mistyped resolution and a black screen.
fn try_mode(width: u32, height: u32, unsafe_modes: bool) -> Result<(), String> {
    let mut dm = zeroed_devmode();
    dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    // Seed from the current mode so refresh rate, bit depth and orientation are
    // carried over; only the two size fields are declared as changed.
    if unsafe { EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut dm) } == 0 {
        return Err(String::from("EnumDisplaySettingsW(current) failed"));
    }
    dm.dmPelsWidth = width;
    dm.dmPelsHeight = height;
    dm.dmFields |= DM_PELSWIDTH | DM_PELSHEIGHT;

    let flag = if unsafe_modes {
        CDS_ENABLE_UNSAFE_MODES
    } else {
        0
    };

    let test = unsafe { ChangeDisplaySettingsW(&dm, CDS_TEST | flag) };
    if test != DISP_CHANGE_SUCCESSFUL {
        return Err(format!(
            "{width}x{height} rejected by the driver (CDS_TEST={test}{})",
            if unsafe_modes { ", unsafe" } else { "" }
        ));
    }

    let rc = unsafe { ChangeDisplaySettingsW(&dm, CDS_UPDATEREGISTRY | flag) };
    if rc != DISP_CHANGE_SUCCESSFUL {
        return Err(format!("{width}x{height} failed to apply (rc={rc})"));
    }
    Ok(())
}

/// Remember the pre-change mode exactly once per process. A failed enumeration
/// is not recorded: `restore()` then reports "nothing recorded" instead of
/// blindly restoring a guessed size.
fn remember_original(mode: Option<(u32, u32)>) {
    let Some(mode) = mode else { return };
    if let Ok(mut slot) = ORIGINAL.lock() {
        if slot.is_none() {
            *slot = Some(mode);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the bounds only — no Win32 call, so this runs on any machine.
    #[test]
    fn refuses_sizes_outside_the_useful_range() {
        assert!(apply(10, 10).is_err());
        assert!(apply(0, 0).is_err());
        assert!(apply(99_999, 1080).is_err());
        assert!(apply(1714, 0).is_err());
    }

    /// `restore()` before any switch must not touch the desktop: reporting an
    /// error is the whole contract.
    #[test]
    fn restore_without_a_recorded_mode_is_an_error() {
        // Only valid while nothing else in this test binary applied a mode;
        // `apply()`'s own assertions above never succeed, so the cell is None.
        assert!(restore().is_err());
    }
}
