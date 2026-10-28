//! #54 real Windows input injection (`WinInputSink`).
//!
//! Replaces the stage1 `NullInputSink` stub: parses the selkies-webclient
//! wire input messages and injects them with Win32 `SendInput` /
//! `SetCursorPos`, matching RustDesk's native client interaction latency.
//!
//! Wire contract (input.js, verified):
//! * `m,<x>,<y>,<mask>,<mag>,<seq>`    absolute pointer; x/y are **stream frame
//!   pixels** (0..frame_w, 0..frame_h), not the 0-1000 normalisation that older
//!   selkies docs claimed — see `frame_to_screen`. #68: "frame" here means the
//!   frame the browser decoded (`videoWidth/videoHeight`), i.e.
//!   `StreamSettings::output()`, which after the #66 clamp can be *smaller* than
//!   the resolution the client requested.
//! * `m2,<dx>,<dy>,<mask>,<mag>,<seq>` relative pointer (server px); scroll
//!   pulses ride mask bits 3(down)/4(up)/6(left)/7(right) with `mag` notches
//! * `kd,<keysym>` / `ku,<keysym>`      key down / up (X11 keysym decimal)
//! * `kh,<k1>,<k2>,...`                 held-key heartbeat (ensure pressed)
//! * `kr`                               release all keys
//! * `p,0|1`                            pointer-lock state (informational)
//! * `SET_NATIVE_CURSOR_RENDERING,0|1`  cursor mode switch (informational)
//! * `sf,<dx>,<dy>` / `sfe`             smooth-wheel fallback
//! * `js,...`                           joystick (ignored)
//!
//! Button mask bits: 0=left, 1=middle, 2=right; 3/4 vertical wheel rising
//! edge, 6/7 horizontal wheel rising edge.

use std::os::raw::c_int;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MapVirtualKeyW,
    MOUSEINPUT, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, SendInput, VkKeyScanW, INPUT,
    INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

use crate::pipeline::StreamSettings;

use super::input_stub::InputSink;

/// One press / release produced from a keysym.
#[derive(Debug, Clone, Copy)]
struct VkAction {
    vk: u16,
    /// Set when the key must ride on a modifier (shift for `!`, `@`, ...).
    needs_shift: bool,
}

// #70 diagnostics: relative-pointer traffic as it reached the injector.
//
// Gaming mode is the only source of `m2`, and while it is on there is no
// on-screen cursor in the page to compare against, so "the pointer does not
// move" is otherwise unattributable. The pipeline's 5 s stats line reports the
// deltas of these two counters:
//
//   `m2` flat                      -> the frontend sent no delta at all
//   `m2` up, `dm` > 0, `paint` flat -> deltas arrive but the OS pointer stays
//                                     put (injection), not a client problem
//
// #72: the same split applies to absolute motion, which is what the page sends
// while *not* locked: `m=` is that counter. Both were flat for a different
// reason — the session had no `on_message` on the unordered `pointer` channel,
// so webrtc-rs read the messages off SCTP and dropped them.
//
// Process-wide atomics rather than per-sink fields: stage1 runs one session
// per process, and the pipeline reports from the beat, which holds no sink.
static MOUSE_REL_EVENTS: AtomicU64 = AtomicU64::new(0);
static MOUSE_REL_PIXELS: AtomicI64 = AtomicI64::new(0);
/// #72: absolute `m` messages applied since process start. Non-zero growth is
/// the proof that the `pointer` channel hook works: without it the server
/// received nothing from the unordered channel and this stayed flat too.
static MOUSE_ABS_EVENTS: AtomicU64 = AtomicU64::new(0);

/// #70: `(m2 messages injected, |dx| + |dy| summed)` since process start.
pub fn mouse_rel_totals() -> (u64, i64) {
    (
        MOUSE_REL_EVENTS.load(Ordering::Relaxed),
        MOUSE_REL_PIXELS.load(Ordering::Relaxed),
    )
}

/// #72: `m` messages applied since process start.
pub fn mouse_abs_total() -> u64 {
    MOUSE_ABS_EVENTS.load(Ordering::Relaxed)
}

/// #73: messages actually handed to the `pointer` channel hook since process
/// start.
///
/// The #72 fix gave that channel an `on_message`, and the remaining question is
/// binary: does the unordered channel deliver anything at all? `m=` alone
/// cannot answer it, because `sendMotionMessage` falls back to `control` while
/// `pointer` is not open (webrtc.js:898) — so a working `m=` can still mean the
/// hook is dead and everything is arriving on the ordered channel. `pm=`
/// counts the hook, `m=`/`m2=` count what the injector saw; a gap between them
/// is the diagnosis.
static POINTER_CHANNEL_MSGS: AtomicU64 = AtomicU64::new(0);
/// #73: how many `pointer`-channel payloads have been echoed to the log. Bounded
/// so a 500 Hz mouse cannot turn the log into the problem it is diagnosing.
static POINTER_CHANNEL_ECHOED: AtomicU64 = AtomicU64::new(0);

/// #73: `pointer`-channel payloads seen by the session hook.
pub fn pointer_channel_total() -> u64 {
    POINTER_CHANNEL_MSGS.load(Ordering::Relaxed)
}

/// #73: record one payload arriving on the unordered channel, echoing the
/// first few verbatim.
///
/// Called from the session hook *before* classification: a payload that
/// classifies to something else still proves the channel is wired, which is
/// exactly the distinction that matters here.
pub fn note_pointer_channel_msg(line: &str) {
    POINTER_CHANNEL_MSGS.fetch_add(1, Ordering::Relaxed);
    let n = POINTER_CHANNEL_ECHOED.fetch_add(1, Ordering::Relaxed);
    if n < 8 {
        log::info!("pointer channel <- {line}");
    }
}

/// Translate an X11 keysym into a Windows VK (plus a shift hint for
/// printable characters whose layout position requires Shift).
fn keysym_to_vk(keysym: u32) -> Option<VkAction> {
    // Printable ASCII: let Windows resolve the layout (US/ISO) — VkKeyScanW
    // returns LOWORD=VK, HIWORD shift-state bits (1=shift, 2=ctrl, 4=alt).
    if (0x20..=0x7e).contains(&keysym) {
        let ch = keysym as u16;
        let res = unsafe { VkKeyScanW(ch) };
        if res == -1 {
            return None;
        }
        let vk = (res as u16) & 0xff;
        let needs_shift = ((res as u16) >> 8) & 1 != 0;
        return Some(VkAction { vk, needs_shift });
    }

    // X11 function-key block (0xff00-0xffff), incl. XK_* named constants.
    let vk = match keysym {
        0xff08 => 0x08u16,            // BackSpace
        0xff09 => 0x09,               // Tab
        0xff0b => 0x0c,               // Clear
        0xff0d => 0x0d,               // Return
        0xff13 => 0x13,               // Pause
        0xff14 => 0x91,               // Scroll_Lock
        0xff1b => 0x1b,               // Escape
        0xff50 => 0x24,               // Home
        0xff51 => 0x25,               // Left
        0xff52 => 0x26,               // Up
        0xff53 => 0x27,               // Right
        0xff54 => 0x28,               // Down
        0xff55 => 0x21,               // Prior (PageUp)
        0xff56 => 0x22,               // Next (PageDown)
        0xff57 => 0x23,               // End
        0xff61 => 0x2c,               // Print
        0xff63 => 0x2d,               // Insert
        0xff67 => 0x5d,               // Menu
        0xff7f => 0x90,               // Num_Lock
        0xffff => 0x2e,               // Delete
        0xffe1 => 0xa0,               // Shift_L
        0xffe2 => 0xa1,               // Shift_R
        0xffe3 => 0xa2,               // Control_L
        0xffe4 => 0xa3,               // Control_R
        0xffe5 => 0x14,               // Caps_Lock
        0xffe7 | 0xffeb => 0x5b,      // Meta_L / Super_L -> LWin
        0xffe8 | 0xffec => 0x5c,      // Meta_R / Super_R -> RWin
        0xffe9 => 0xa4,               // Alt_L
        0xffea => 0xa5,               // Alt_R
        0xffbe..=0xffd5 => 0x70u16 + (keysym - 0xffbe) as u16, // F1..F24 -> VK_F1..VK_F24
        0xffb0..=0xffb9 => 0x60u16 + (keysym - 0xffb0) as u16, // KP_0..KP_9
        0xff80 => 0x60,               // KP_Space (treated as numpad 0) — best effort
        0xffaa => 0x6a,               // KP_Multiply
        0xffab => 0x6b,               // KP_Add
        0xffad => 0x6d,               // KP_Subtract
        0xffae => 0x6e,               // KP_Decimal
        0xffaf => 0x6f,               // KP_Divide
        0xffbd => 0x6c,               // KP_Equal (separator-ish)
        _ => return None,
    };
    Some(VkAction { vk, needs_shift: false })
}

/// Pointer state kept across messages so we can emit button / wheel
/// transitions instead of absolute "current state" events.
#[derive(Default)]
struct InputState {
    buttons_held: u32,
    scroll_bits_prev: u32,
    held_keys: Vec<u32>,
}

/// The real injection sink. All methods are `&self` and cheap: heavy work is a
/// Win32 call, and the internal state is behind one `Mutex`.
pub struct WinInputSink {
    state: Mutex<InputState>,
    /// #62/#68: live stream settings, used purely as the *coordinate space* the
    /// frontend is talking in.
    ///
    /// READ THIS BEFORE CHANGING ANY COORDINATE MATH HERE:
    /// * The webclient sends `m,x,y` in **decoded frame pixels**. Its
    ///   `_applySinkCoordinates()` multiplies the element-relative position by
    ///   `sinkW / boxW`, where `sinkW = video.videoWidth` — i.e. the size of the
    ///   frames the server actually encoded.
    /// * Therefore the divisor below MUST be the *transmitted* size, which is
    ///   `StreamSettings::output()` (published by the capture loop) — never
    ///   `width()/height()`, which hold the raw client request. Since #66 the
    ///   pipeline clamps an over-large request to the native display, so the two
    ///   differ (3840x2160 requested vs. 1718x926 sent). Using the request makes
    ///   every click land at `actual/request` of the intended position — the
    ///   #68 "cursor offset" regression: the pointer moved only ~45 % as far as
    ///   the click, and the error grew toward the right/bottom edge.
    /// * `output()` is (0, 0) only until the capture loop opens its encoder;
    ///   falling back to the request for that one window is intentional.
    frame: StreamSettings,
    /// #68: log the mapping once per session (frame size vs. screen size).
    /// A "clicks land in the wrong place" report is otherwise impossible to
    /// diagnose from the server log: the only evidence needed is whether the
    /// divisor equals the transmitted frame and whether `GetSystemMetrics`
    /// agrees with the captured desktop.
    mapping_logged: std::sync::atomic::AtomicBool,
    /// #72: highest `seq` of an absolute `m` that was applied.
    ///
    /// Coalesced motion rides the unordered `pointer` channel, so a newer
    /// absolute position can overtake an older one and applying the older one
    /// afterwards would snap the cursor back for a frame. The frontend numbers
    /// every pointer message (`Input._pointerSeq`, input.js:3126) for exactly
    /// this purpose; a message with a lower-or-equal number than one already
    /// applied is stale and is dropped.
    ///
    /// Only `m` is filtered. Relative `m2` deltas are summed, so their arrival
    /// order carries no information and dropping one would lose real motion.
    last_abs_seq: std::sync::atomic::AtomicU64,
}

impl Default for WinInputSink {
    fn default() -> Self {
        Self::new(StreamSettings::default())
    }
}

impl WinInputSink {
    /// Build a sink that maps frame-space pointer input onto the real screen
    /// using the live `StreamSettings` (which the pipeline re-seeds to the
    /// native display size and the client may override).
    pub fn new(frame: StreamSettings) -> Self {
        Self {
            state: Mutex::new(InputState::default()),
            frame,
            mapping_logged: std::sync::atomic::AtomicBool::new(false),
            last_abs_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// #68: one INFO line per session showing both sides of the pointer mapping.
    fn log_mapping_once(&self, fw: u32, fh: u32, sw: i32, sh: i32) {
        use std::sync::atomic::Ordering as O;
        if self.mapping_logged.swap(true, O::SeqCst) {
            return;
        }
        let (rw, rh) = (self.frame.width(), self.frame.height());
        log::info!(
            "input mapping: frame {fw}x{fh} -> screen {sw}x{sh} \
             (client requested {rw}x{rh}, transmitted size is authoritative)"
        );
        if (fw as i32, fh as i32) != (sw, sh) {
            log::info!(
                "input mapping is scaled by {:.3}x / {:.3}x — expected only when the \
                 stream is resized",
                sw as f32 / fw.max(1) as f32,
                sh as f32 / fh.max(1) as f32
            );
        }
    }
}

fn screen_size() -> (i32, i32) {
    (
        unsafe { GetSystemMetrics(SM_CXSCREEN) },
        unsafe { GetSystemMetrics(SM_CYSCREEN) },
    )
}

/// #68: map one axis value from transmitted-frame pixels onto physical screen
/// pixels. Endpoint-to-endpoint (`0 -> 0`, `frame-1 -> screen-1`) so a 1:1
/// stream is an exact identity.
///
/// `frame` MUST be the size the browser decoded (`StreamSettings::output()`).
/// Passing the client's *requested* resolution here is the #68 bug: with a
/// 3840x2160 request clamped to a 1718x926 wire, every click would be scaled by
/// 1718/3840 ≈ 0.447 and the pointer would stop at ~45 % of the way to the
/// click point (error growing linearly toward the edges).
fn map_axis(v: i32, frame: u32, screen: i32) -> i32 {
    let last_screen = screen.max(2) as i64 - 1;
    let last_frame = frame.max(2) as i64 - 1;
    (v as i64 * last_screen / last_frame).clamp(0, last_screen) as i32
}

/// Convert a 0-1000 normalized coordinate into screen pixels.
fn normalized_to_px(v: f64, span: i32) -> i32 {
    let span = span.max(1);
    ((v.clamp(0.0, 1000.0) / 1000.0) * (span as f64 - 1.0)).round() as i32
}

/// Screen pixel -> the 0..65535 absolute range `SendInput` expects. Without
/// `MOUSEEVENTF_VIRTUALDESK` this is the primary monitor, matching
/// `GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN)`.
fn absolute_norm(v: i32, span: i32) -> i32 {
    let last = span.max(2) - 1;
    ((v.clamp(0, last) as i64 * 65535) / last as i64) as i32
}

fn send_inputs(inputs: &mut [INPUT]) {
    if inputs.is_empty() {
        return;
    }
    let n = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_mut_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    if n != inputs.len() as u32 {
        log::warn!("SendInput injected {n}/{} events", inputs.len());
    }
}

fn key_input(vk: u16, down: bool) -> INPUT {
    let mut scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) };
    let mut flags: u32 = 0;
    // Extended-key virtual codes need the extended flag so the injected scan
    // code matches the physical key (RHS Ctrl/Alt, arrows, Insert/Delete...).
    let extended = matches!(vk, 0xa1 | 0xa3 | 0xa5 | 0x25 | 0x26 | 0x27 | 0x28 | 0x2d | 0x2e | 0x6f);
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
        scan = 0;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan as u16,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Absolute pointer move. #62: `SendInput` with
/// `MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE` instead of `SetCursorPos`:
/// `SetCursorPos` only moves the cursor, it produces no `WM_INPUT`, so any
/// raw-input consumer (pointer-locked games, several Unity/UE titles) saw
/// nothing at all. This path generates a real input event.
fn mouse_move_abs(x: i32, y: i32) {
    let (w, h) = screen_size();
    send_inputs(&mut [INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: absolute_norm(x, w),
                dy: absolute_norm(y, h),
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }]);
}

fn mouse_move_rel(dx: i32, dy: i32) {
    if dx == 0 && dy == 0 {
        return;
    }
    send_inputs(&mut [INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }]);
}

fn mouse_button_event(flag_down: u32, flag_up: u32, down: bool) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: if down { flag_down } else { flag_up },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_wheel_event(vertical: bool, notches: i32) -> INPUT {
    // WHEEL_DELTA is a macro in winuser.h (120); windows-sys 0.52 does not
    // re-export it, so the literal stands.
    let delta = 120i32.saturating_mul(notches);
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: delta as u32,
                dwFlags: if vertical { MOUSEEVENTF_WHEEL } else { MOUSEEVENTF_HWHEEL },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Diff the incoming button mask against the held state and emit the
/// transitions (bit0 left, bit1 middle, bit2 right).
fn sync_buttons(state: &mut InputState, mask: u32) {
    let held = state.buttons_held & 0x7;
    let next = mask & 0x7;
    if held == next {
        return;
    }
    let mut inputs = Vec::with_capacity(2);
    for (bit, down_flag, up_flag) in [
        (0u32, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        (1, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        (2, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
    ] {
        let was = held & (1 << bit) != 0;
        let is = next & (1 << bit) != 0;
        if was && !is {
            inputs.push(mouse_button_event(down_flag, up_flag, false));
        } else if !was && is {
            inputs.push(mouse_button_event(down_flag, up_flag, true));
        }
    }
    state.buttons_held = next;
    if !inputs.is_empty() {
        send_inputs(&mut inputs);
    }
}

/// Diff the scroll bits (3/4 vertical, 6/7 horizontal) against the previous
/// mask; each 0->1 rising edge emits one wheel pulse of `mag` notches.
fn sync_scroll(state: &mut InputState, mask: u32, mag: i32) {
    const SCROLL_BITS: u32 = 0b1100_1000;
    let changed = (state.scroll_bits_prev ^ mask) & SCROLL_BITS;
    let rising = changed & mask;
    if rising == 0 {
        state.scroll_bits_prev = mask & SCROLL_BITS;
        return;
    }
    let mut inputs = Vec::with_capacity(4);
    for (bit, vertical) in [
        (3u32, true),
        (4, true),
        (6, false),
        (7, false),
    ] {
        if rising & (1 << bit) != 0 {
            // bit3/6 = down/left (negative delta), bit4/7 = up/right (positive).
            let dir = if bit == 3 || bit == 6 { -1 } else { 1 };
            inputs.push(mouse_wheel_event(vertical, dir * mag.max(1)));
        }
    }
    state.scroll_bits_prev = mask & SCROLL_BITS;
    if !inputs.is_empty() {
        send_inputs(&mut inputs);
    }
}

/// Press or release one key. A printable char that needs Shift emits a
/// Shift-down first (and Shift-up after release) unless Shift is already held.
fn inject_key(state: &InputState, keysym: u32, down: bool) {
    let Some(action) = keysym_to_vk(keysym) else {
        log::debug!("keysym 0x{keysym:x} not mapped; dropped");
        return;
    };
    let shift_held = state
        .held_keys
        .iter()
        .any(|&k| matches!(k, 0xffe1 | 0xffe2));
    let mut inputs = Vec::with_capacity(3);
    if down && action.needs_shift && !shift_held {
        inputs.push(key_input(0x10, true)); // VK_SHIFT
    }
    inputs.push(key_input(action.vk, down));
    if !down && action.needs_shift && !shift_held {
        inputs.push(key_input(0x10, false));
    }
    send_inputs(&mut inputs);
}

impl WinInputSink {
    /// #62: frame space -> screen space. The frontend's `m,x,y` are pixels in
    /// the **streamed frame** (`_clientToServerX` = element-relative *
    /// frame/element ratio), so a 1280x720 stream on a 1718x926 desktop sends at
    /// most (1280,720): the right/bottom quarter of the screen was unreachable.
    /// Scale by the live frame size; when it is unknown the values pass through
    /// unchanged.
    ///
    /// #68: "live frame size" means the size the browser is *decoding*
    /// (`StreamSettings::output()`), not the size the client requested. See the
    /// field doc on `frame` for why this distinction is load-bearing.
    fn transmitted_frame(&self) -> (u32, u32) {
        let (ow, oh) = self.frame.output();
        if ow > 0 && oh > 0 {
            return (ow, oh);
        }
        (self.frame.width(), self.frame.height())
    }

    fn frame_to_screen(&self, x: i32, y: i32) -> (i32, i32) {
        let (fw, fh) = self.transmitted_frame();
        let (sw, sh) = screen_size();
        if fw == 0 || fh == 0 {
            return (x, y);
        }
        // Endpoint-to-endpoint mapping (frame pixel 0 -> screen pixel 0, last
        // frame pixel -> last screen pixel) so a 1:1 stream is an exact
        // identity and no sub-pixel bias accumulates toward the far edge.
        self.log_mapping_once(fw, fh, sw, sh);
        (map_axis(x, fw, sw), map_axis(y, fh, sh))
    }

    /// #74: pointer-locked deltas pass through unchanged.
    ///
    /// This used to scale them by `screen / frame`, on the #62 reasoning that a
    /// locked pointer should keep a 1:1 feel "on the screen". That is the wrong
    /// frame of reference for a camera: a game multiplies the delta by its own
    /// sensitivity to get an angle, so any factor here becomes a sensitivity
    /// multiplier that changes with the stream geometry. Worse, a factor above
    /// 1 pushes each injected delta further up the host's mouse-acceleration
    /// curve, which is non-linear — a measured `dm/m2` of 9.55 px per message
    /// sat inside Windows' doubled band (Threshold1=6, Threshold2=10), so the
    /// pointer moved roughly twice as far as the hand did (#74).
    ///
    /// Streamers that get this right inject relative motion verbatim: Moonlight
    /// and Parsec hand the host a raw HID delta, and Sunshine's documentation is
    /// explicit that relative movement goes through a real HID device rather
    /// than being reconstructed from cursor positions. The gain a player wants
    /// belongs in the game's own sensitivity setting, or in the client's
    /// `pointerGain`, not here.
    ///
    /// The parameters stay so call sites and tests keep their shape; absolute
    /// mapping (`frame_to_screen`) is untouched and must stay that way.
    fn frame_to_screen_delta(&self, dx: i32, dy: i32) -> (i32, i32) {
        (dx, dy)
    }

    /// Full wire-message parser. Called by the session for every C-group
    /// line; unrecognized shapes are debug-logged and dropped.
    fn handle(&self, line: &str) {
        let (head, rest) = match line.split_once(',') {
            Some((h, r)) => (h, r),
            None => (line, ""),
        };
        match head {
            "m" => {
                let f: Vec<&str> = rest.splitn(5, ',').collect();
                if f.len() < 4 {
                    log::debug!("malformed m: {line}");
                    return;
                }
                let Ok(x) = f[0].parse::<i32>() else { return };
                let Ok(y) = f[1].parse::<i32>() else { return };
                let Ok(mask) = f[2].parse::<u32>() else { return };
                let Ok(mag) = f[3].parse::<f64>() else { return };
                // #72: absolute positions ride the unordered channel; see
                // `last_abs_seq`. No `seq` on the wire (older client) = accept.
                if let Some(seq) = f.get(4).and_then(|s| s.trim().parse::<u64>().ok()) {
                    if seq <= self.last_abs_seq.load(Ordering::Relaxed) {
                        return;
                    }
                    self.last_abs_seq.store(seq, Ordering::Relaxed);
                }
                MOUSE_ABS_EVENTS.fetch_add(1, Ordering::Relaxed);
                // #62: `m` carries stream-frame pixels; map them onto the
                // physical screen before injecting (a 1:1 pass-through left the
                // right/bottom of the desktop unreachable whenever the stream
                // was scaled down).
                // #68: frame pixels = the *transmitted* size (see
                // `transmitted_frame`), never the client's requested resolution.
                let (sx, sy) = self.frame_to_screen(x, y);
                mouse_move_abs(sx, sy);
                if let Ok(mut st) = self.state.lock() {
                    sync_buttons(&mut st, mask);
                    let _ = mag;
                }
            }
            "m2" => {
                let f: Vec<&str> = rest.splitn(5, ',').collect();
                if f.len() < 4 {
                    log::debug!("malformed m2: {line}");
                    return;
                }
                let Ok(dx) = f[0].parse::<i32>() else { return };
                let Ok(dy) = f[1].parse::<i32>() else { return };
                let Ok(mask) = f[2].parse::<u32>() else { return };
                let Ok(mag) = f[3].parse::<i32>() else { return };
                // #62: relative deltas are frame-space too; scale them so a
                // locked pointer keeps a 1:1 feel on a scaled-down stream.
                let (sdx, sdy) = self.frame_to_screen_delta(dx, dy);
                // #70: count what is actually injected, not what arrived — a
                // scaled-to-zero delta is exactly the failure being looked for.
                MOUSE_REL_EVENTS.fetch_add(1, Ordering::Relaxed);
                MOUSE_REL_PIXELS.fetch_add(i64::from(sdx.abs() + sdy.abs()), Ordering::Relaxed);
                mouse_move_rel(sdx, sdy);
                if let Ok(mut st) = self.state.lock() {
                    sync_buttons(&mut st, mask);
                    sync_scroll(&mut st, mask, mag);
                }
            }
            "kd" | "ku" => {
                let Ok(keysym) = rest.parse::<u32>() else {
                    log::debug!("malformed {head}: {line}");
                    return;
                };
                let down = head == "kd";
                let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
                inject_key(&st, keysym, down);
                if down {
                    if !st.held_keys.contains(&keysym) {
                        st.held_keys.push(keysym);
                    }
                } else {
                    st.held_keys.retain(|&k| k != keysym);
                }
            }
            "kh" => {
                // Held-key heartbeat: make sure every listed keysym is down.
                let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
                for ks in rest.split(',') {
                    let Ok(keysym) = ks.parse::<u32>() else { continue };
                    if !st.held_keys.contains(&keysym) {
                        inject_key(&st, keysym, true);
                        st.held_keys.push(keysym);
                    }
                }
            }
            "kr" => {
                let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
                for &ks in &st.held_keys.clone() {
                    inject_key(&st, ks, false);
                }
                st.held_keys.clear();
            }
            // Informational / no-op on the injection side.
            "p" | "SET_NATIVE_CURSOR_RENDERING" | "js" | "sf" | "sfe" | "pointer" => {
                log::debug!("[input] informational line: {line}");
            }
            _ => {
                log::debug!("[input] unhandled line: {line}");
            }
        }
    }
}

impl InputSink for WinInputSink {
    fn mouse_absolute(&self, x: f64, y: f64, mask: u32, _magnitude: f64) {
        let (w, h) = screen_size();
        mouse_move_abs(normalized_to_px(x, w), normalized_to_px(y, h));
        if let Ok(mut st) = self.state.lock() {
            sync_buttons(&mut st, mask);
        }
    }

    fn mouse_relative(&self, dx: f64, dy: f64, mask: u32, magnitude: f64) {
        mouse_move_rel(dx as i32, dy as i32);
        if let Ok(mut st) = self.state.lock() {
            sync_buttons(&mut st, mask);
            sync_scroll(&mut st, mask, magnitude as i32);
        }
    }

    fn mouse_button(&self, pressed: bool, mask: u32) {
        if let Ok(mut st) = self.state.lock() {
            let next = if pressed { st.buttons_held | mask } else { st.buttons_held & !mask };
            sync_buttons(&mut st, next);
        }
    }

    fn mouse_wheel(&self, _dx: f64, dy: f64, _mask: u32) {
        let notches = dy.round() as i32;
        if notches != 0 {
            send_inputs(&mut [mouse_wheel_event(true, notches)]);
        }
    }

    fn key_down(&self, keysym: u32) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !st.held_keys.contains(&keysym) {
            inject_key(&st, keysym, true);
            st.held_keys.push(keysym);
        }
    }

    fn key_up(&self, keysym: u32) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        inject_key(&st, keysym, false);
        st.held_keys.retain(|&k| k != keysym);
    }

    fn keys_sync(&self, held: Vec<u32>) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        for ks in held {
            if !st.held_keys.contains(&ks) {
                inject_key(&st, ks, true);
                st.held_keys.push(ks);
            }
        }
    }

    fn keys_release_all(&self) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        for &ks in &st.held_keys.clone() {
            inject_key(&st, ks, false);
        }
        st.held_keys.clear();
    }

    fn reset(&self) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        for &ks in &st.held_keys.clone() {
            inject_key(&st, ks, false);
        }
        st.held_keys.clear();
        // #62: a held mouse button must be released too, otherwise the host
        // app keeps receiving a "pressed" state after the tab goes away.
        sync_buttons(&mut st, 0);
        st.scroll_bits_prev = 0;
        log::info!("input state reset: held keys / buttons released");
    }

    fn note_stub(&self, line: &str) {
        log::debug!("[input] note_stub (should not happen with WinInputSink): {line}");
    }

    fn handle_line(&self, line: &str) {
        self.handle(line);
    }
}

/// Re-export for wiring; keeps the alias stable for the session.
pub type WinInputSinkRef = std::sync::Arc<dyn InputSink>;

#[allow(dead_code)]
fn _assert_c_int() -> c_int {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1:1 stream (the #66 default path: no client request, no scaling) must
    /// be a pixel-exact identity, including the last pixel.
    #[test]
    fn map_axis_is_identity_for_unscaled_stream() {
        assert_eq!(map_axis(0, 1718, 1718), 0);
        assert_eq!(map_axis(900, 1718, 1718), 900);
        assert_eq!(map_axis(1717, 1718, 1718), 1717);
    }

    /// A downscaled stream must still reach the far screen edge, and an
    /// out-of-range frame coordinate must clamp instead of overshooting.
    #[test]
    fn map_axis_reaches_far_edge_for_downscaled_stream() {
        assert_eq!(map_axis(0, 1280, 1718), 0);
        assert_eq!(map_axis(1279, 1280, 1718), 1717);
        assert_eq!(map_axis(4096, 1280, 1718), 1717);
    }

    /// #68 regression guard: pointer mapping must divide by the *transmitted*
    /// geometry. If someone reverts `transmitted_frame()` to `width()/height()`
    /// this fails — that revert is exactly what put every click at ~45 % of the
    /// intended position when a 3840x2160 request was clamped to 1718x926.
    #[test]
    fn transmitted_frame_prefers_output_over_request() {
        let s = StreamSettings::from_config(&crate::config::Config::default());
        s.set_resolution(3840, 2160);
        let sink = WinInputSink::new(s.clone());
        // Before the capture loop reports, the request is the only value known.
        assert_eq!(sink.transmitted_frame(), (3840, 2160));
        s.set_output(1718, 926);
        assert_eq!(sink.transmitted_frame(), (1718, 926));
    }
}
