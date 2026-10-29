//! #90 host-side desktop knobs: UI scaling and mouse DPI.
//!
//! Both are properties of the *host*, not of the stream, so neither belongs in
//! `StreamSettings`: the pipeline neither reads nor clamps them, and the browser
//! never learns them from `server_settings` unless this module can read them.
//!
//! What "DPI" means here
//! --------------------
//! A mouse's DPI is a hardware property: the sensor reports N counts per inch
//! and no software can change it. What Windows *does* expose is a pointer-speed
//! multiplier — `SPI_SETMOUSESPEED`, steps 1..=20, whose multipliers are
//! 1/32, 1/16, 1/8, 1/4, 3/8, 1/2, 5/8, 3/4, 7/8, 1, 9/8, 5/4, 3/2, 7/4, 2,
//! 9/4, 5/2, 11/4, 3, 7/2. The panel calls that multiplier "DPI" against a
//! 400 DPI baseline (step 10 = 1x = 400 DPI), so the number a player types is
//! what the host ends up running at — not a gain the browser multiplies into
//! the deltas it sends. That distinction is the whole point: a gain in the
//! client only pulls the locked-pointer path, while the speed set here rules the
//! host's real pointer, so the desktop, the launcher and the game all agree.
//!
//! UI scaling
//! ----------
//! `SPI_SETLOGICALDPIOVERRIDE` (0x009F) is the undocumented call behind the
//! Settings > Display > Scale dropdown. Two readings of its `uiParam` are in
//! circulation — an absolute index into 100/125/150/175/200/225/250/300 %, and
//! an offset from the monitor's *recommended* step — and which one a given
//! Windows build takes is not documented ("Do not use" in the SDK). Rather than
//! pick one and be silently wrong on the other, `apply_scale` writes the
//! absolute index, reads the setting back, and only if that did not land tries
//! the relative form. The reply carries what the host actually reports, so the
//! panel can never claim a scale the desktop is not at.
//!
//! Both calls are per-session: they must run inside the interactive session the
//! desktop lives in (the server runs there, session 1), not from an SSH shell in
//! session 0, where `SPI_GETLOGICALDPIOVERRIDE` simply fails.

use std::ffi::c_void;

use windows_sys::Win32::UI::WindowsAndMessaging::SystemParametersInfoW;

// Documented (mouse) and undocumented-but-universal (logical DPI override)
// `SystemParametersInfo` actions. The SDK marks the DPI override "Do not use"
// and ships no constant for it in the Rust binding, so it is spelled out here.
const SPI_GETMOUSESPEED: u32 = 0x0070;
const SPI_SETMOUSESPEED: u32 = 0x0071;
const SPI_GETLOGICALDPIOVERRIDE: u32 = 0x009E;
const SPI_SETLOGICALDPIOVERRIDE: u32 = 0x009F;
/// Persist the change in the user profile.
const SPIF_UPDATEINIFILE: u32 = 0x0001;
/// Tell every top-level window the setting moved, so they re-read it.
const SPIF_SENDCHANGE: u32 = 0x0002;

/// The DPI a 1x pointer speed is reported as.
pub const BASE_DPI: u32 = 400;

/// Windows' pointer-speed steps 1..=20 as the DPI they mean against
/// [`BASE_DPI`]. `SPEED_DPI[n - 1]` is step `n`; step 10, the OS default, is
/// exactly `BASE_DPI`. The multipliers are Windows' own; 400 x them is this.
const SPEED_DPI: [u32; 20] = [
    13, 25, 50, 100, 150, 200, 250, 300, 350, 400, 450, 500, 600, 700, 800, 900, 1000, 1100, 1200,
    1400,
];

/// The scaling steps Windows offers, as DPI against the 96-DPI baseline the
/// panel's percentages are computed from (`percent = dpi / 96 * 100`). 275 % is
/// absent on purpose: Windows does not offer it, and an option the host cannot
/// land on is a dropdown entry that silently does nothing.
pub const SCALE_DPI: [u32; 8] = [96, 120, 144, 168, 192, 216, 240, 288];

/// What a pointer-DPI request ended as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedMouse {
    /// What the client asked for.
    pub requested: u32,
    /// The DPI the host is actually at afterwards.
    pub applied: u32,
    /// The Windows speed step that produced it, 1..=20.
    pub speed: u8,
    /// `applied == requested`. False means the nearest step was used.
    pub exact: bool,
}

/// What a UI-scaling request ended as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedScale {
    /// What the client asked for, in DPI against 96.
    pub requested: u32,
    /// What the host reports afterwards, in the same unit.
    pub applied: u32,
    /// Which `uiParam` reading landed it: `absolute`, `relative`, or `already`.
    pub strategy: &'static str,
    /// `applied == requested`. False means the nearest offered step was used.
    pub exact: bool,
}

/// The speed step whose DPI is closest to `dpi`, 1..=20.
///
/// Ties go to the slower step: overshooting a player's DPI makes the pointer
/// feel loose, which is the complaint this setting exists to fix.
pub fn speed_for_dpi(dpi: u32) -> u8 {
    let mut best = 0usize;
    let mut best_gap = u32::MAX;
    for (index, candidate) in SPEED_DPI.iter().enumerate() {
        let gap = candidate.abs_diff(dpi);
        if gap < best_gap {
            best_gap = gap;
            best = index;
        }
    }
    (best + 1) as u8
}

/// The DPI a speed step stands for.
pub fn dpi_for_speed(speed: u32) -> u32 {
    let index = speed.clamp(1, SPEED_DPI.len() as u32) as usize - 1;
    SPEED_DPI[index]
}

/// The offered scaling step closest to `dpi`, in the same unit.
pub fn nearest_scale_dpi(dpi: u32) -> u32 {
    SCALE_DPI
        .iter()
        .copied()
        .min_by_key(|candidate| candidate.abs_diff(dpi))
        .unwrap_or(96)
}

/// Every pointer-speed step as DPI, slowest first. The panel builds its slider
/// stops from this, so a stop can never be a value the host cannot take.
pub fn speed_steps_dpi() -> [u32; 20] {
    SPEED_DPI
}

/// The host's pointer speed step right now, or `None` when the query fails
/// (a session with no interactive desktop).
pub fn current_speed() -> Option<u8> {
    let mut value: i32 = 0;
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETMOUSESPEED,
            0,
            &mut value as *mut i32 as *mut c_void,
            0,
        )
    };
    if ok == 0 || !(1..=20).contains(&value) {
        return None;
    }
    Some(value as u8)
}

/// The host's pointer DPI right now, from its speed step.
pub fn current_mouse_dpi() -> Option<u32> {
    current_speed().map(|speed| dpi_for_speed(u32::from(speed)))
}

/// Set the host's pointer speed so its DPI is as close to `dpi` as Windows can
/// express, then read it back.
///
/// `Err` carries a sentence for the log and the panel; a session that refuses
/// `SPI_SETMOUSESPEED` is a normal outcome (no interactive desktop), not a bug.
pub fn apply_mouse_dpi(dpi: u32) -> Result<AppliedMouse, String> {
    let lowest = SPEED_DPI[0];
    let highest = SPEED_DPI[SPEED_DPI.len() - 1];
    if !(lowest..=highest).contains(&dpi) {
        return Err(format!(
            "refusing {dpi} DPI: outside {lowest}..={highest}"
        ));
    }
    let speed = speed_for_dpi(dpi);
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_SETMOUSESPEED,
            u32::from(speed),
            std::ptr::null_mut(),
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        )
    };
    if ok == 0 {
        return Err(format!(
            "SPI_SETMOUSESPEED({speed}) refused: {}",
            std::io::Error::last_os_error()
        ));
    }
    let speed_now = current_speed().ok_or_else(|| {
        String::from("SPI_GETMOUSESPEED failed right after the write; the host pointer may not have moved")
    })?;
    let applied = dpi_for_speed(u32::from(speed_now));
    Ok(AppliedMouse {
        requested: dpi,
        applied,
        speed: speed_now,
        exact: applied == dpi,
    })
}

/// Read the raw `SPI_GETLOGICALDPIOVERRIDE` value. It is negative when the
/// session is off the monitor's recommended step, and its magnitude is the step
/// index either way.
fn read_override() -> Option<i32> {
    let mut value: i32 = 0;
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETLOGICALDPIOVERRIDE,
            0,
            &mut value as *mut i32 as *mut c_void,
            0,
        )
    };
    if ok == 0 {
        return None;
    }
    Some(value)
}

/// A raw override value as an index into [`SCALE_DPI`]. The value is negative
/// (or zero) and its magnitude is the step; anything past the list is clamped so
/// a host left at 350 % still reports the closest step this panel can show.
fn index_of(value: i32) -> usize {
    let magnitude = value.unsigned_abs() as usize;
    magnitude.min(SCALE_DPI.len() - 1)
}

/// The host's UI scaling right now, in DPI against 96, or `None` when the
/// session cannot be queried.
pub fn current_scale_dpi() -> Option<u32> {
    read_override().map(|value| SCALE_DPI[index_of(value)])
}

fn write_override(ui_param: i32) -> bool {
    // `uiParam` is a WPARAM: a negative offset is passed as its two's-complement
    // bit pattern, which is what the relative form needs for "below recommended".
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_SETLOGICALDPIOVERRIDE,
            ui_param as u32,
            std::ptr::null_mut(),
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        )
    };
    ok != 0
}

/// Set the host's UI scaling to `dpi` (96 = 100 %), then read it back.
///
/// The absolute-index reading is tried first; if the host does not report the
/// step afterwards, the relative-to-recommended reading is tried from the same
/// starting point. Only a host that reports the requested step afterwards is
/// called `exact`.
pub fn apply_scale(dpi: u32) -> Result<AppliedScale, String> {
    let target_index = SCALE_DPI
        .iter()
        .position(|candidate| *candidate == dpi)
        .ok_or_else(|| format!("refusing {dpi} DPI: not one of {SCALE_DPI:?}"))?;

    let before = read_override().ok_or_else(|| {
        String::from("SPI_GETLOGICALDPIOVERRIDE failed: no interactive desktop in this session?")
    })?;
    if index_of(before) == target_index {
        return Ok(AppliedScale {
            requested: dpi,
            applied: SCALE_DPI[target_index],
            strategy: "already",
            exact: true,
        });
    }

    // Reading 1: `uiParam` is the absolute step index.
    if write_override(target_index as i32) {
        if let Some(after) = read_override() {
            if index_of(after) == target_index {
                return Ok(AppliedScale {
                    requested: dpi,
                    applied: SCALE_DPI[target_index],
                    strategy: "absolute",
                    exact: true,
                });
            }
        }
    }

    // Reading 2: `uiParam` is a step offset from the monitor's recommended one,
    // which the getter reports as the negative of its index.
    let recommended = index_of(before) as i32;
    let offset = target_index as i32 - recommended;
    if write_override(offset) {
        if let Some(after) = read_override() {
            return Ok(AppliedScale {
                requested: dpi,
                applied: SCALE_DPI[index_of(after)],
                strategy: "relative",
                exact: index_of(after) == target_index,
            });
        }
    }

    Err(String::from(
        "the host refused the scaling change (SPI_SETLOGICALDPIOVERRIDE)",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panel's default and the OS default have to be the same step, or a
    /// freshly-opened session would "change" the host's speed on connect.
    #[test]
    fn the_default_dpi_is_the_os_default_step() {
        assert_eq!(speed_for_dpi(BASE_DPI), 10);
        assert_eq!(dpi_for_speed(10), BASE_DPI);
    }

    /// Every step round-trips, so a host read back always reports a DPI the
    /// panel can show and a slider stop can land on.
    #[test]
    fn every_speed_step_round_trips() {
        for speed in 1..=20u32 {
            let dpi = dpi_for_speed(speed);
            assert_eq!(speed_for_dpi(dpi), speed as u8, "step {speed} -> {dpi} DPI");
        }
    }

    /// A typed value between two steps takes the nearer one; the reply then
    /// says it was not exact rather than pretending.
    #[test]
    fn typed_values_snap_to_the_nearest_step() {
        assert_eq!(speed_for_dpi(780), 15); // 800 DPI is the closest step
        assert_eq!(dpi_for_speed(15), 800);
        assert_eq!(speed_for_dpi(9999), 20); // clamped to the fastest step
        assert_eq!(speed_for_dpi(0), 1); // and the slowest the other way
    }

    /// Out-of-range requests are refused before touching the host.
    #[test]
    fn refuses_dpi_outside_the_expressible_range() {
        assert!(apply_mouse_dpi(0).is_err());
        assert!(apply_mouse_dpi(10).is_err());
        assert!(apply_mouse_dpi(99_999).is_err());
    }

    /// The scaling steps are exactly the ones the panel can name, and 275 % is
    /// not among them (Windows does not offer it).
    #[test]
    fn scaling_steps_match_the_panels_percentages() {
        let percents: Vec<u32> = SCALE_DPI.iter().map(|dpi| dpi * 100 / 96).collect();
        assert_eq!(percents, vec![100, 125, 150, 175, 200, 225, 250, 300]);
        assert_eq!(nearest_scale_dpi(264), 240);
        assert_eq!(nearest_scale_dpi(97), 96);
        assert_eq!(nearest_scale_dpi(4000), 288);
    }

    /// An override value is read as `-magnitude`, and anything past the list
    /// clamps instead of panicking on an index.
    #[test]
    fn override_values_map_onto_the_step_list() {
        assert_eq!(index_of(0), 0);
        assert_eq!(index_of(-1), 1);
        assert_eq!(index_of(-3), 3);
        assert_eq!(index_of(-99), SCALE_DPI.len() - 1);
    }

    /// A scale the host does not offer is refused by name, not snapped behind
    /// the caller's back.
    #[test]
    fn refuses_a_scale_the_host_does_not_offer() {
        assert!(apply_scale(264).is_err());
        assert!(apply_scale(96 * 7).is_err());
    }
}
