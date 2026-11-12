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
//! * `p,0|1`                            pointer-lock state. #124: no longer a
//!   no-op — it decides whether an absolute `m` is applied or held back
//!   ([`should_suppress_abs`]), whether the pipeline composites the OS cursor,
//!   and when a self-taken 1:1 pin is released.
//! * `SET_NATIVE_CURSOR_RENDERING,0|1`  cursor mode switch (informational)
//! * `sf,<dx>,<dy>` / `sfe`             smooth-wheel fallback
//! * `js,...`                           joystick; forwarded to `dispatch::gamepad`
//!   (#17), which owns the ViGEmBus handles. Dropped at debug level when no
//!   hub was supplied.
//!
//! Button mask bits: 0=left, 1=middle, 2=right; 3/4 vertical wheel rising
//! edge, 6/7 horizontal wheel rising edge.

use std::os::raw::c_int;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::time::Instant;

use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC,
    MapVirtualKeyW,
    MOUSEINPUT, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    SendInput, VkKeyScanW, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetCursorPos,
};

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
/// #124/C: absolute `m` messages dropped because the page was locked and
/// driving by deltas.
///
/// Without this counter `m=` climbing next to a motionless pointer reads as
/// "injection is broken" — the exact conclusion #70/#72 taught everyone to draw.
/// `ms=` says the messages arrived and were deliberately not applied.
static MOUSE_ABS_SUPPRESSED: AtomicU64 = AtomicU64::new(0);

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

/// #124/C: `m` messages withheld because the page was driving by deltas.
pub fn mouse_abs_suppressed_total() -> u64 {
    MOUSE_ABS_SUPPRESSED.load(Ordering::Relaxed)
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

/// X11's explicit Unicode keysym range: `0x01000000 | codepoint`.
///
/// The frontend's `_keysymFor()` emits this for every character outside
/// Latin-1 (`input.js:965-971`), which is exactly the set a keyboard layout
/// cannot produce directly: `中`, `日本語`, `Привет`, emoji.
const XK_UNICODE_BASE: u32 = 0x0100_0000;

// #124: the three things a relative-motion client needs from us that #74 left
// open. A game (this repo's reference is a pointer-locked FPS, `temp/几猫…`) does
// not want "a mouse driving a desktop"; it wants raw counts, injected 1:1, with
// the pointer never running out of screen to move on.
//
// Process-wide for the same reason the counters above are: the pipeline's stats
// line reads them from the beat, which holds no sink. `p` and `m2` also describe
// the *host* pointer, which is a single thing — two tabs cannot each have their
// own, and `PipelineHandle::cursor_inject` (the other half of #124/D) is already
// shared per program rather than per tab.
/// `p,1|0` as last reported by a page: the browser cursor is captured.
static POINTER_LOCKED: AtomicBool = AtomicBool::new(false);
/// A relative delta has arrived since the current lock was taken.
///
/// Deliberately not just `m2 ever`: this is the answer to "is the client driving
/// by relative motion right now", and it gates two behaviours that are wrong for
/// an absolute pointer — suppressing `m` (#124/C) and not compositing the OS
/// cursor (#124/D). A page that sends `m2` without ever locking keeps the old,
/// conservative behaviour.
static POINTER_RELATIVE: AtomicBool = AtomicBool::new(false);
/// #124/B: `SetCursorPos` warps back to the middle of the target monitor.
static RECENTER_EVENTS: AtomicU64 = AtomicU64::new(0);
/// #124/E: largest single `|dx| + |dy|` of the current stats window.
///
/// The reference client drops any single delta with a component over 400 px
/// (its own anti-jump guard), so a legitimately fast flick can be thrown away
/// while the server has no idea it happened. `dmax` is the only number that
/// shows the cap being approached; the window is swapped to 0 by
/// [`rel_window_max`] so the line reports a maximum, not a running one.
static REL_WINDOW_MAX: AtomicI64 = AtomicI64::new(0);
/// #124/B: `--pointer-recenter`, off by default — it moves the host pointer.
static RECENTER_ENABLED: AtomicBool = AtomicBool::new(false);

/// #124/B: how close to an edge the pointer has to get before it is warped back.
///
/// Two pixels, not a percentage: the point is to catch "the pointer is against
/// the wall and every further count is discarded", which happens at 0–1 px.
/// A larger margin would fire early and lose real motion on a display whose
/// edges the user legitimately works in.
const RECENTER_MARGIN: i32 = 2;

/// #124/A: how long a self-taken 1:1 pin survives without a single `m2`.
///
/// The pin changes the host's pointer settings, so it may not outlive its
/// reason. Two paths already release it (`p,0` and session teardown); this one
/// covers the page that keeps the lock but stops sending — a tab suspended by
/// the browser is the usual case. Checked on every line the session feeds the
/// sink, so it costs nothing while input is flowing and cannot fire while the
/// user is actually playing.
const PIN_IDLE: std::time::Duration = std::time::Duration::from_secs(30);

/// #124: `ma,` has not spoken — the heuristics may pin and release on their own.
const DIRECT_AUTO: u8 = 0;
/// #124: `ma,1` — the client wants 1:1 and owns the release.
const DIRECT_ON: u8 = 1;
/// #124: `ma,0` — the client explicitly does not want 1:1. Never auto-pin again
/// in this session, otherwise the next `m2` would undo the client's choice
/// within one frame and the host would be left in a state the user cannot see.
const DIRECT_OFF: u8 = 2;

/// #124/A: whether the host pointer is pinned to 1:1 right now.
pub fn pointer_locked() -> bool {
    POINTER_LOCKED.load(Ordering::Relaxed)
}

/// #124/C+D: the client is locked *and* has sent a delta since locking.
pub fn pointer_is_relative() -> bool {
    POINTER_LOCKED.load(Ordering::Relaxed) && POINTER_RELATIVE.load(Ordering::Relaxed)
}

/// #124/B: enable edge recentering (`--pointer-recenter`). Process-wide, like
/// `dispatch::ghost::set_idle_secs` — set once at startup.
pub fn set_recenter(on: bool) {
    RECENTER_ENABLED.store(on, Ordering::Relaxed);
}

/// #124/B: whether edge recentering is enabled.
pub fn recenter_enabled() -> bool {
    RECENTER_ENABLED.load(Ordering::Relaxed)
}

/// #124/B: warps performed since process start.
pub fn recenter_total() -> u64 {
    RECENTER_EVENTS.load(Ordering::Relaxed)
}

/// #124/E: the largest single delta of the window that just ended, and reset.
pub fn rel_window_max() -> i64 {
    REL_WINDOW_MAX.swap(0, Ordering::Relaxed)
}

/// #124/B: where to put the pointer before the next delta, if it is boxed in.
///
/// `space` is `(x, y, w, h)` of the monitor being streamed, in virtual-desktop
/// coordinates. Returns the centre of that monitor when the pointer sits within
/// `margin` of any of its four edges — **or outside it entirely**, which on a
/// multi-monitor host means the pointer has wandered onto a screen nobody is
/// watching and every delta is now invisible to the player. `None` otherwise.
///
/// This is a pure function on purpose: the decision is the part that is easy to
/// get wrong (an off-by-one on the far edge means "the pointer stops 1 px early
/// and nothing says why"), and it is the only part testable without a desktop.
fn recenter_target(
    cursor: (i32, i32),
    space: (i32, i32, i32, i32),
    margin: i32,
) -> Option<(i32, i32)> {
    let (x, y, w, h) = space;
    if w <= 0 || h <= 0 {
        return None;
    }
    // Far edges are inclusive-minus-one: pixel `x + w - 1` is the last one that
    // exists, and that is where `SendInput` stops moving the pointer.
    let (first_x, last_x) = (x, x + w - 1);
    let (first_y, last_y) = (y, y + h - 1);
    let margin = margin.max(0);
    let boxed = cursor.0 - first_x <= margin
        || last_x - cursor.0 <= margin
        || cursor.1 - first_y <= margin
        || last_y - cursor.1 <= margin;
    if boxed {
        Some((x + w / 2, y + h / 2))
    } else {
        None
    }
}

/// #124/C: whether an absolute `m` must be dropped rather than injected.
///
/// While a page is locked the browser keeps sending both: `m` from its
/// (clipped, meaningless) cursor position and `m2` from `movementX/Y`. Applying
/// `m` drags the host pointer to a coordinate that has nothing to do with the
/// view the player is turning — every one of them undoes the accumulated
/// offsets from the deltas, which reads as "the mouse stutters back".
fn should_suppress_abs(locked: bool, relative: bool) -> bool {
    locked && relative
}

/// Resolve a keysym to a Unicode scalar that must be injected as text.
///
/// Two encodings reach us:
/// * `0x01000000 | codepoint` — everything the layout cannot type.
/// * Latin-1 (`0x00a0..=0x00ff`) — X11 makes the keysym *be* the codepoint
///   (`input.js:966`), e.g. `é` = 0xe9.
///
/// Both MUST bypass `VkKeyScanW`: no layout maps `é` or `中` to a virtual key,
/// so it returns -1 and the character is dropped. That was the entire reason
/// "Chinese cannot be typed" while plain ASCII worked — the pre-#76 mapping
/// only accepted `0x20..=0x7e` and the 0xff00 function-key block.
///
/// Printable ASCII is deliberately *not* routed here: those characters carry
/// layout semantics (`?` = Shift+`/`) that `VkKeyScanW` resolves correctly,
/// and `KEYEVENTF_UNICODE` would bypass dead keys and the host IME.
fn keysym_to_unicode(keysym: u32) -> Option<u32> {
    let cp = if keysym & 0xff00_0000 == XK_UNICODE_BASE {
        keysym & 0x00ff_ffff
    } else if (0xa0..=0xff).contains(&keysym) {
        keysym
    } else {
        return None;
    };
    char::from_u32(cp).map(|_| cp)
}

/// Presses that type one codepoint through `SendInput`.
///
/// A codepoint above the BMP has to travel as its two UTF-16 surrogate halves;
/// Windows composes them back into a single character. Only presses are
/// emitted — `KEYEVENTF_UNICODE` types the character on the down edge, so a
/// matching release would type it twice.
fn unicode_input(cp: u32) -> Vec<INPUT> {
    let ch = char::from_u32(cp).unwrap_or('\u{fffd}');
    let mut buf = [0u16; 2];
    ch.encode_utf16(&mut buf)
        .iter()
        .filter(|&&unit| unit != 0)
        .map(|&unit| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        })
        .collect()
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
    /// #124: `p,1` — the page says its cursor is captured.
    locked: bool,
    /// #124: a relative delta arrived in this lock epoch. Reset when a new lock
    /// starts, so "locked but idle" never suppresses the absolute stream of a
    /// page that turns out to be absolute after all.
    relative: bool,
    /// #124/C: the last `m` position seen while suppressed, applied once on
    /// `p,0` so the pointer lands where the page's cursor is rather than
    /// wherever the deltas left it.
    pending_abs: Option<(i32, i32)>,
    /// #124/A: when the last relative delta was injected (idle un-pin).
    last_rel: Option<Instant>,
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
    /// #17: the virtual gamepad. `None` keeps the pre-#17 behavior for `js`
    /// (debug log, drop) — which is what a build without ViGEmBus, and the unit
    /// tests, get.
    gamepad: Option<Arc<super::gamepad::GamepadHub>>,
    /// #109: the display these frames belong to. Default is the primary
    /// monitor; a session watching display N sets this after it attaches, and
    /// every absolute move is then expressed in that monitor's corner.
    target: Mutex<TargetSpace>,
    /// #124/A: this sink took the 1:1 pin itself, so it is the one that has to
    /// give it back. Ownership rather than a refcount: `hostui::pin_pointer` is
    /// idempotent and keeps a single restore point, so a sink that pinned only
    /// because it was the first to ask must not release someone else's pin
    /// (#101 made the same distinction for whole sessions).
    auto_pinned: AtomicBool,
    /// #124: what `ma,` said — [`DIRECT_AUTO`], [`DIRECT_ON`], [`DIRECT_OFF`].
    direct: AtomicU8,
}

/// The monitor the injected pointer lives on, in virtual-desktop coordinates.
#[derive(Clone, Copy, Debug)]
struct TargetSpace {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    /// True when the target is not the primary monitor, so `SendInput` has to
    /// carry `MOUSEEVENTF_VIRTUALDESK` and normalise against the virtual
    /// desktop rather than the primary screen.
    virtual_desk: bool,
}

impl Default for TargetSpace {
    fn default() -> Self {
        let (w, h) = screen_size();
        Self {
            x: 0,
            y: 0,
            w,
            h,
            virtual_desk: false,
        }
    }
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
    ///
    /// No virtual gamepad: `js` lines are logged and dropped. The session uses
    /// [`WinInputSink::with_gamepad`] instead.
    pub fn new(frame: StreamSettings) -> Self {
        Self::with_gamepad(frame, None)
    }

    /// #17: the session's sink, with the virtual gamepad bus it should feed.
    pub fn with_gamepad(
        frame: StreamSettings,
        gamepad: Option<Arc<super::gamepad::GamepadHub>>,
    ) -> Self {
        Self {
            state: Mutex::new(InputState::default()),
            frame,
            mapping_logged: std::sync::atomic::AtomicBool::new(false),
            last_abs_seq: std::sync::atomic::AtomicU64::new(0),
            gamepad,
            target: Mutex::new(TargetSpace::default()),
            auto_pinned: AtomicBool::new(false),
            direct: AtomicU8::new(DIRECT_AUTO),
        }
    }

    /// #109: move the pointer to a pixel in the *target monitor's* space.
    ///
    /// On the primary monitor this is the pre-#109 `SendInput` exactly:
    /// normalised against `SM_CXSCREEN/CYSCREEN`. anywhere else it has to carry
    /// `MOUSEEVENTF_VIRTUALDESK`, and the normalised value is measured against
    /// the whole virtual desktop — without the flag Windows interprets the
    /// coordinate in primary-screen space and the pointer lands on the wrong
    /// monitor.
    fn move_abs(&self, x: i32, y: i32) {
        let t = *self.target.lock().unwrap_or_else(|p| p.into_inner());
        let (dx, dy, flags) = space_to_send(x, y, &t, virtual_screen());
        send_inputs(&mut [INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }]);
    }

    /// #124/A: take the host pointer to 1:1 the first time a page shows it is
    /// driving by relative motion, instead of waiting for the client to send
    /// `ma,1`.
    ///
    /// #74 measured why this has to happen at all: `SendInput`'s relative
    /// displacement runs through Windows' "enhance pointer precision" curve,
    /// which is a *non-linear* function of hand speed (`dm/m2` landed at 9.55
    /// px/msg, inside the Threshold1/Threshold2 doubled band). A player cannot
    /// compensate for that with a sensitivity slider, because the curve is still
    /// there at every speed — the only fix is turning the curve off. That is
    /// exactly what `hostui::pin_pointer` does, and it restores the previous
    /// values on release.
    ///
    /// Inline rather than on a worker: it is two registry reads and at most two
    /// writes without a broadcast, it happens once per session (the flag below
    /// short-circuits every later delta), and the alternative — a thread hop in
    /// the middle of the input path — would put 100 µs of scheduling jitter on
    /// the frame where the player first moves the mouse.
    fn auto_pin(&self) {
        if self.direct.load(Ordering::Relaxed) == DIRECT_OFF {
            return;
        }
        if self.auto_pinned.load(Ordering::Relaxed) {
            return;
        }
        // Someone else (a second tab's `ma,1`, or an operator) already holds it:
        // record nothing, so we never release a pin we did not take.
        if super::hostui::pointer_pinned() {
            return;
        }
        // Claimed before the call so two deltas racing on the same session
        // cannot both decide to pin; `pin_pointer` is idempotent anyway, but the
        // flag is what decides who releases.
        self.auto_pinned.store(true, Ordering::Relaxed);
        match super::hostui::pin_pointer() {
            Ok(applied) => log::info!(
                "host pointer auto-pinned to 1:1 for relative input \
                 (speed={}, accel_off={}, strategy={})",
                applied.speed,
                applied.accel,
                applied.strategy
            ),
            Err(reason) => {
                self.auto_pinned.store(false, Ordering::Relaxed);
                log::warn!(
                    "relative input could not pin the host pointer to 1:1: {reason} \
                     — deltas still pass through, but the OS acceleration curve \
                     multiplies them"
                );
            }
        }
    }

    /// #124/A: give back a pin this sink took. Idempotent, and a no-op for a
    /// sink that never pinned.
    fn auto_unpin(&self, why: &str) {
        if !self.auto_pinned.swap(false, Ordering::Relaxed) {
            return;
        }
        match super::hostui::unpin_pointer() {
            Ok(()) => log::info!("host pointer released ({why})"),
            Err(reason) => log::warn!("host pointer not restored ({why}): {reason}"),
        }
    }

    /// #124/A: drop the pin when the page holds the lock but stopped sending.
    ///
    /// Called at the top of every line. A silent session is covered by teardown
    /// and by `p,0`; this covers the tab the browser suspended with the lock
    /// still nominally held — the host would otherwise keep acceleration off
    /// until someone closed it.
    fn reap_idle_pin(&self) {
        if !self.auto_pinned.load(Ordering::Relaxed) {
            return;
        }
        let idle = match self.state.lock() {
            Ok(st) => st.last_rel.map(|t| t.elapsed()),
            Err(p) => p.into_inner().last_rel.map(|t| t.elapsed()),
        };
        if idle.map(|d| d >= PIN_IDLE).unwrap_or(false) {
            self.auto_unpin("no relative motion for 30s");
        }
    }

    /// #124/B: put the pointer back in the middle of the monitor when it has
    /// been pushed against an edge.
    ///
    /// This is the actual "转不动" mechanism, and it is the same in both modes:
    /// `m2` is applied to the *real* cursor, so once the cursor is on the last
    /// pixel of the desktop every further relative move is discarded by the OS
    /// and the client's `movementX` becomes 0 as well. A 180° turn costs ~1500
    /// counts on the reference client, so a 1920-wide screen runs out before the
    /// second flick unless the page has taken a pointer lock (Chrome clips the
    /// cursor to the window, which is why this is invisible in a browser and
    /// only shows up through a remote one).
    ///
    /// `SetCursorPos` and not `SendInput`-with-absolute: the latter synthesises
    /// a move *event*, which the host app would read as a large real movement —
    /// in a game that is a flick. The former only relocates the pointer.
    fn recenter_if_needed(&self) {
        if !recenter_enabled() {
            return;
        }
        let t = *self.target.lock().unwrap_or_else(|p| p.into_inner());
        let mut at = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut at) } == 0 {
            return;
        }
        let Some((cx, cy)) = recenter_target((at.x, at.y), (t.x, t.y, t.w, t.h), RECENTER_MARGIN)
        else {
            return;
        };
        if unsafe { SetCursorPos(cx, cy) } == 0 {
            log::debug!("recenter refused by the host (SetCursorPos failed)");
            return;
        }
        RECENTER_EVENTS.fetch_add(1, Ordering::Relaxed);
    }

    /// #124: `p,<0|1>` from the page.
    ///
    /// Two things happen on the transition, and both matter:
    /// * `1` starts a new lock epoch. `relative` is cleared so a page that
    ///   locks but never sends a delta keeps the conservative absolute path.
    /// * `0` ends it. An absolute position that arrived while suppressed is
    ///   applied here, once, so the pointer ends up where the page's own cursor
    ///   is instead of wherever the deltas left it — the "unlock jumps to a
    ///   random corner" complaint.
    fn set_locked(&self, on: bool) {
        let pending = {
            let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            st.locked = on;
            st.relative = false;
            if on {
                None
            } else {
                st.pending_abs.take()
            }
        };
        POINTER_LOCKED.store(on, Ordering::Relaxed);
        POINTER_RELATIVE.store(false, Ordering::Relaxed);
        log::info!(
            "pointer lock -> {} (relative motion will {}absolute `m`)",
            if on { "on" } else { "off" },
            if on { "suppress " } else { "no longer suppress " }
        );
        if !on {
            if let Some((x, y)) = pending {
                let (sx, sy) = self.frame_to_screen(x, y);
                self.move_abs(sx, sy);
            }
            self.auto_unpin("pointer lock released");
        }
    }

    /// #124: a delta arrived — remember it, and pin if nobody has yet.
    fn note_relative(&self, dx: i32, dy: i32) {
        {
            let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            st.last_rel = Some(Instant::now());
            if st.locked {
                st.relative = true;
            }
        }
        if POINTER_LOCKED.load(Ordering::Relaxed) {
            POINTER_RELATIVE.store(true, Ordering::Relaxed);
        }
        REL_WINDOW_MAX.fetch_max(i64::from(dx.abs() + dy.abs()), Ordering::Relaxed);
        self.auto_pin();
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
fn virtual_screen() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// Frame pixel -> *virtual-desktop* pixel for a monitor whose corner is `origin`
/// (#109). `map_axis` handles the frame→monitor scale; the origin is the second
/// half and is what makes a secondary screen clickable at all.
fn place(v: i32, frame: u32, screen: i32, origin: i32) -> i32 {
    map_axis(v, frame, screen) + origin
}

/// A screen pixel in the target monitor's space -> the `(dx, dy, flags)` triple
/// `SendInput` wants for an absolute move (#109).
///
/// Split out from `move_abs` so the arithmetic can be tested without a second
/// monitor: the whole "点哪儿准哪儿" question on a dual-screen host is these two
/// branches, and a mistake here is invisible on a single-screen machine.
///
/// Two different normalisations, and they are not interchangeable:
/// * primary — `0..65535` across `SM_CXSCREEN/CYSCREEN`, no extra flag. This is
///   the pre-#109 path, kept byte-for-byte so a single-monitor session cannot
///   regress.
/// * anywhere else — `0..65535` across the **whole virtual desktop**, plus
///   `MOUSEEVENTF_VIRTUALDESK`. Without the flag Windows reads the value in
///   primary-screen space and the pointer lands on the primary monitor; with the
///   flag but primary-space values it lands offset by the monitor's origin.
fn space_to_send(
    x: i32,
    y: i32,
    space: &TargetSpace,
    desktop: (i32, i32, i32, i32),
) -> (i32, i32, u32) {
    if space.virtual_desk {
        let (vx, vy, vw, vh) = desktop;
        let last_x = (vw.max(2) - 1) as i64;
        let last_y = (vh.max(2) - 1) as i64;
        (
            (((x - vx) as i64).clamp(0, last_x) * 65535 / last_x) as i32,
            (((y - vy) as i64).clamp(0, last_y) * 65535 / last_y) as i32,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )
    } else {
        (
            absolute_norm(x, space.w),
            absolute_norm(y, space.h),
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
        )
    }
}

/// Does this target need `MOUSEEVENTF_VIRTUALDESK`?
///
/// The primary monitor is the only case that must avoid it: the flag changes how
/// Windows interprets *every* absolute move, and a single-screen box has no
/// virtual desktop to be wrong about.
fn needs_virtual_desk(origin: (i32, i32), size: (u32, u32), primary: (i32, i32)) -> bool {
    origin != (0, 0) || (size.0 as i32, size.1 as i32) != primary
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

/// The four scroll bits: 3 = down, 4 = up, 6 = left, 7 = right.
///
/// #101: this used to be `0b1100_1000`, which covers 3/6/7 and silently drops
/// bit 4. Scrolling up therefore never produced a rising edge — the pointer
/// moved, the mask was recorded, and no wheel event was ever injected, while
/// scrolling down worked. Both vertical bits have to be here for the diff
/// below to see them at all.
const SCROLL_BITS: u32 = 0b1101_1000;

/// Rising edges of `SCROLL_BITS` between `prev` and `mask`, as
/// `(vertical, notches)` pulses; a negative count scrolls down/left.
///
/// Split out of `sync_scroll` so the bit/direction table can be asserted
/// without injecting input — a wrong entry here is invisible everywhere else,
/// because the injection itself still "succeeds" and just scrolls nothing.
fn scroll_pulses(prev: u32, mask: u32, mag: i32) -> Vec<(bool, i32)> {
    let changed = (prev ^ mask) & SCROLL_BITS;
    let rising = changed & mask;
    let mut pulses = Vec::with_capacity(4);
    if rising == 0 {
        return pulses;
    }
    for (bit, vertical) in [
        (3u32, true),
        (4, true),
        (6, false),
        (7, false),
    ] {
        if rising & (1 << bit) != 0 {
            // bit3/6 = down/left (negative delta), bit4/7 = up/right (positive).
            let dir = if bit == 3 || bit == 6 { -1 } else { 1 };
            pulses.push((vertical, dir * mag.max(1)));
        }
    }
    pulses
}

/// Diff the scroll bits (3/4 vertical, 6/7 horizontal) against the previous
/// mask; each 0->1 rising edge emits one wheel pulse of `mag` notches.
fn sync_scroll(state: &mut InputState, mask: u32, mag: i32) {
    let pulses = scroll_pulses(state.scroll_bits_prev, mask, mag);
    state.scroll_bits_prev = mask & SCROLL_BITS;
    if pulses.is_empty() {
        return;
    }
    let mut inputs: Vec<INPUT> = pulses
        .iter()
        .map(|&(vertical, notches)| mouse_wheel_event(vertical, notches))
        .collect();
    send_inputs(&mut inputs);
}

/// Press or release one key. A printable char that needs Shift emits a
/// Shift-down first (and Shift-up after release) unless Shift is already held.
fn inject_key(state: &InputState, keysym: u32, down: bool) {
    // #76: text that no keyboard layout can produce (CJK, Cyrillic, emoji,
    // accented Latin) goes in as a Unicode scan code, not as a virtual key.
    if let Some(cp) = keysym_to_unicode(keysym) {
        if down {
            let mut inputs = unicode_input(cp);
            send_inputs(&mut inputs);
        }
        return;
    }
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
        let t = *self.target.lock().unwrap_or_else(|p| p.into_inner());
        let (sw, sh) = (t.w, t.h);
        if fw == 0 || fh == 0 {
            return (x, y);
        }
        // Endpoint-to-endpoint mapping (frame pixel 0 -> screen pixel 0, last
        // frame pixel -> last screen pixel) so a 1:1 stream is an exact
        // identity and no sub-pixel bias accumulates toward the far edge.
        self.log_mapping_once(fw, fh, sw, sh);
        // The origin is added last, after the frame->monitor mapping: the
        // frontend's pixel is relative to the frame, and the monitor's corner
        // is where that frame begins in the virtual desktop.
        (
            place(x, fw, sw, t.x),
            place(y, fh, sh, t.y),
        )
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
        // #124/A: a page that holds the lock and stopped sending must not keep
        // the host's pointer settings hostage. Cheap when nothing is pinned.
        self.reap_idle_pin();
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
                // #124/C: while the page is locked and has sent deltas, its `m`
                // is the position of a cursor the browser has already clipped to
                // the window — not a host coordinate. Injecting it drags the
                // pointer back on every frame, and the player sees the view
                // stutter. Remember it instead; `set_locked(false)` applies the
                // last one so the pointer ends up under the page's cursor.
                let suppress = {
                    let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    let suppress = should_suppress_abs(st.locked, st.relative);
                    if suppress {
                        st.pending_abs = Some((x, y));
                    }
                    sync_buttons(&mut st, mask);
                    let _ = mag;
                    suppress
                };
                if suppress {
                    MOUSE_ABS_SUPPRESSED.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                // #72: `m=` counts *applied* absolute moves; `ms=` counts the ones
                // #124/C held back, so their sum is what arrived.
                MOUSE_ABS_EVENTS.fetch_add(1, Ordering::Relaxed);
                // #62: `m` carries stream-frame pixels; map them onto the
                // physical screen before injecting (a 1:1 pass-through left the
                // right/bottom of the desktop unreachable whenever the stream
                // was scaled down).
                // #68: frame pixels = the *transmitted* size (see
                // `transmitted_frame`), never the client's requested resolution.
                let (sx, sy) = self.frame_to_screen(x, y);
                self.move_abs(sx, sy);
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
                // #124: this is the message that says "the client is a camera".
                // It arms the 1:1 pin (A), records the window's worst delta (E),
                // and — with `--pointer-recenter` — pulls the pointer off the
                // edge it is about to get stuck on (B).
                self.note_relative(sdx, sdy);
                self.recenter_if_needed();
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
            // #17: gamepad. Parsed here because this module owns the wire
            // grammar; injected in `gamepad` because that module owns the
            // ViGEm handles, which may only be touched from its own thread.
            // No hub (driver missing, or a caller that did not want one): keep
            // the pre-#17 behavior at debug level — nothing else changes.
            "js" => match &self.gamepad {
                Some(hub) => hub.handle_js(rest),
                None => log::debug!("[input] gamepad line dropped (no ViGEmBus): {line}"),
            },
            // #124: `p,<0|1>` stopped being informational. It decides whether
            // an absolute `m` is injected (see `should_suppress_abs`), whether
            // the server composites the OS cursor (`pipeline::cursor_inject_on`),
            // and when the 1:1 pin taken in `auto_pin` is given back.
            "p" => match rest.trim() {
                "1" => self.set_locked(true),
                "0" => self.set_locked(false),
                other => log::debug!("malformed p: {line} ({other:?})"),
            },
            // Informational / no-op on the injection side.
            "SET_NATIVE_CURSOR_RENDERING" | "sf" | "sfe" | "pointer" => {
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
        let t = *self.target.lock().unwrap_or_else(|p| p.into_inner());
        self.move_abs(
            normalized_to_px(x, t.w) + t.x,
            normalized_to_px(y, t.h) + t.y,
        );
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
        // #124: drop the lock epoch too — a reconnect starts from nothing, and a
        // stale `relative` would suppress the absolute stream of a page that
        // comes back unlocked.
        st.locked = false;
        st.relative = false;
        st.pending_abs = None;
        st.last_rel = None;
        drop(st);
        POINTER_LOCKED.store(false, Ordering::Relaxed);
        POINTER_RELATIVE.store(false, Ordering::Relaxed);
        // #124/A: a 1:1 pin changes the *host's* settings, so it may not
        // outlive the session that took it. Only our own: a viewer leaving must
        // not revert what the controller in the other tab is still using (#101).
        self.auto_unpin("session ended");
        log::info!("input state reset: held keys / buttons released");
    }

    fn set_target_display(&self, origin: (i32, i32), size: (u32, u32)) {
        if size.0 == 0 || size.1 == 0 {
            log::warn!(
                "input target display has zero size ({}x{}); keeping the primary",
                size.0,
                size.1
            );
            return;
        }
        // Primary keeps the exact pre-#109 path (no VIRTUALDESK) so a
        // single-monitor session cannot regress because of this feature.
        let virtual_desk = needs_virtual_desk(origin, size, screen_size());
        *self.target.lock().unwrap_or_else(|p| p.into_inner()) = TargetSpace {
            x: origin.0,
            y: origin.1,
            w: size.0 as i32,
            h: size.1 as i32,
            virtual_desk,
        };
        log::info!(
            "input target display: {}x{} at ({}, {}) virtual_desk={virtual_desk}",
            size.0,
            size.1,
            origin.0,
            origin.1
        );
    }

    /// #124: `ma,` — the client said what it wants, so the automatic pin must
    /// stop guessing in the opposite direction.
    ///
    /// The pin itself is still taken and released by the session (it needs the
    /// `AppliedPin` report to answer the client), so this only records intent.
    /// Both values clear `auto_pinned`: `ma,1` because the client now owns the
    /// release, `ma,0` because it just performed one.
    fn pointer_direct(&self, on: bool) {
        self.direct.store(
            if on { DIRECT_ON } else { DIRECT_OFF },
            Ordering::Relaxed,
        );
        self.auto_pinned.store(false, Ordering::Relaxed);
        log::info!(
            "pointer direct set by the client -> {on} (automatic 1:1 pin {} )",
            if on { "stays out of the way" } else { "disabled for this session" }
        );
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

    /// #76: the split between "let the layout type it" and "send it as text".
    /// Getting this wrong in either direction is user-visible: routing ASCII
    /// through Unicode breaks dead keys, and routing CJK through the layout
    /// drops the character entirely.
    #[test]
    fn keysyms_split_between_layout_and_text() {
        // Explicit Unicode range: 中 = U+4E2D.
        assert_eq!(keysym_to_unicode(0x0100_0000 | 0x4e2d), Some(0x4e2d));
        // Latin-1: X11 makes the keysym *be* the codepoint.
        assert_eq!(keysym_to_unicode(0xe9), Some(0xe9));
        // ASCII must stay on the VkKeyScanW path (Shift, dead keys, IME).
        assert_eq!(keysym_to_unicode(0x41), None);
        assert_eq!(keysym_to_unicode(0x3f), None);
        // Function keys are not text.
        assert_eq!(keysym_to_unicode(0xff0d), None);
        // Invalid scalar (surrogate) must not panic, just decline.
        assert_eq!(keysym_to_unicode(0x0100_d800), None);
    }

    /// A BMP character is one press; an astral one has to travel as two
    /// UTF-16 surrogate halves or Windows types a replacement character.
    #[test]
    fn unicode_presses_match_utf16_length() {
        assert_eq!(unicode_input(0x4e2d).len(), 1);
        assert_eq!(unicode_input(0x1f600).len(), 2);
        assert_eq!(unicode_input(0xfffd).len(), 1);
    }

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

    /// #101: scrolling up did nothing while scrolling down worked, because
    /// `SCROLL_BITS` omitted bit 4. Pin the exact bit set the protocol uses, so
    /// a typo in this constant can never again cost one whole direction
    /// without failing a test.
    #[test]
    fn scroll_edges_cover_both_vertical_directions() {
        assert_eq!(SCROLL_BITS, (1 << 3) | (1 << 4) | (1 << 6) | (1 << 7));
        // Each direction on a fresh mask is one pulse, signed as the wire
        // contract says: down/left negative, up/right positive.
        assert_eq!(scroll_pulses(0, 0x10, 1), vec![(true, 1)]); // up
        assert_eq!(scroll_pulses(0, 0x08, 1), vec![(true, -1)]); // down
        assert_eq!(scroll_pulses(0, 0x80, 1), vec![(false, 1)]); // right
        assert_eq!(scroll_pulses(0, 0x40, 1), vec![(false, -1)]); // left
        // `mag` rides through as notches, and is never zero.
        assert_eq!(scroll_pulses(0, 0x10, 3), vec![(true, 3)]);
        assert_eq!(scroll_pulses(0, 0x10, 0), vec![(true, 1)]);
    }

    /// The client sends cleared → set → restored around one pulse. Only the
    /// middle message may inject: a held bit must not repeat, or one notch of
    /// scroll would become three.
    #[test]
    fn a_scroll_pulse_fires_once_between_clear_and_release() {
        // Cleared baseline with nothing held: no edge.
        assert!(scroll_pulses(0, 0, 1).is_empty());
        // Rising edge: exactly one pulse.
        assert_eq!(scroll_pulses(0, 0x10, 1).len(), 1);
        // Still set (a second message on the held bit): nothing.
        assert!(scroll_pulses(0x10, 0x10, 1).is_empty());
        // Release: falling edge, nothing.
        assert!(scroll_pulses(0x10, 0, 1).is_empty());
        // Both vertical bits at once would be a broken client, but it must not
        // panic or drop one silently.
        assert_eq!(scroll_pulses(0, 0x18, 1), vec![(true, -1), (true, 1)]);
    }

    // ---- #109: multi-monitor pointer mapping ------------------------------
    //
    // These are the only parts of "点哪儿准哪儿" that can be checked on a host
    // with one screen. The live acceptance still needs two physical monitors
    // (see `toolchain/display-probe.ps1`), but the arithmetic below is exactly
    // what goes wrong there.

    fn space(origin: (i32, i32), size: (i32, i32), virtual_desk: bool) -> TargetSpace {
        TargetSpace {
            x: origin.0,
            y: origin.1,
            w: size.0,
            h: size.1,
            virtual_desk,
        }
    }

    /// The frame origin is *added*, so the first frame pixel lands on the
    /// monitor's top-left corner in virtual-desktop coordinates — not on (0,0),
    /// which is the bug this whole change exists to fix.
    #[test]
    fn a_secondary_frame_starts_at_its_own_corner() {
        // 1920x1080 monitor sitting to the right of the primary.
        assert_eq!(place(0, 1920, 1920, 1920), 1920);
        assert_eq!(place(1919, 1920, 1920, 1920), 3839);
        // Scaled stream: the far edge still reaches the far edge, plus origin.
        assert_eq!(place(1279, 1280, 1920, 1920), 3839);
        // A monitor to the left has a negative origin; the offset is signed.
        assert_eq!(place(0, 1920, 1920, -1920), -1920);
    }

    /// Primary: identical to the pre-#109 output, with no `VIRTUALDESK` — the
    /// guarantee that a single-screen machine cannot regress.
    #[test]
    fn the_primary_monitor_keeps_the_old_no_flag_path() {
        let t = space((0, 0), (1920, 1080), false);
        let desktop = (0, 0, 1920, 1080);
        assert_eq!(space_to_send(0, 0, &t, desktop), (0, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE));
        let (dx, dy, flags) = space_to_send(1919, 1079, &t, desktop);
        assert_eq!((dx, dy), (65535, 65535), "far corner must be the top of the range");
        assert_eq!(flags & MOUSEEVENTF_VIRTUALDESK, 0, "primary must not set VIRTUALDESK");
    }

    /// Secondary to the right: the value is measured against the whole virtual
    /// desktop, so its left edge is the midpoint of `0..65535`, not zero. Sending
    /// zero here is the classic "clicks land on the primary monitor".
    #[test]
    fn a_secondary_monitor_normalises_against_the_virtual_desktop() {
        let t = space((1920, 0), (1920, 1080), true);
        let desktop = (0, 0, 3840, 1080); // two 1080p side by side
        let (dx, dy, flags) = space_to_send(1920, 0, &t, desktop);
        assert_eq!(flags & MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_VIRTUALDESK, "flag missing");
        // 1920 * 65535 / 3839 = 32776 (integer division) — half of the desktop,
        // which is where that monitor's left edge lands. Zero would mean "the
        // pointer goes to the primary screen instead".
        assert_eq!(dx, 32776, "left edge of the right-hand screen is the middle of the desktop");
        assert_eq!(dy, 0, "top edge is the top of the desktop");
        let (dx, _, _) = space_to_send(3839, 1079, &t, desktop);
        assert_eq!(dx, 65535, "the far edge must reach the top of the range");
    }

    /// Secondary to the **left**: the virtual desktop starts at -1920, so that
    /// monitor's own left edge is virtual x = 0. A version that forgot to
    /// subtract the desktop origin would clamp every click on the left screen
    /// onto its right-hand neighbour.
    #[test]
    fn a_monitor_left_of_the_primary_uses_a_signed_desktop_origin() {
        let t = space((-1920, 0), (1920, 1080), true);
        let desktop = (-1920, 0, 3840, 1080);
        let (dx, _, _) = space_to_send(-1920, 0, &t, desktop);
        assert_eq!(dx, 0, "the left screen's first pixel is the desktop's first pixel");
        let (dx, _, _) = space_to_send(-1, 0, &t, desktop);
        // 1919 * 65535 / 3839 = 32758 — its last pixel, just short of the middle.
        assert_eq!(dx, 32758, "its last pixel is the middle of the desktop");
    }

    /// Out-of-range values clamp instead of wrapping into negative `dx`.
    #[test]
    fn out_of_range_points_clamp_onto_the_desktop() {
        let t = space((1920, 0), (1920, 1080), true);
        let desktop = (0, 0, 3840, 1080);
        assert_eq!(space_to_send(-500, -500, &t, desktop).0, 0);
        assert_eq!(space_to_send(99_999, 99_999, &t, desktop).0, 65535);
    }

    /// Only the primary avoids the flag; anything else gets it, including a
    /// same-size monitor at a non-zero origin.
    #[test]
    fn only_the_primary_skips_the_virtual_desk_flag() {
        assert!(!needs_virtual_desk((0, 0), (1920, 1080), (1920, 1080)));
        assert!(needs_virtual_desk((1920, 0), (1920, 1080), (1920, 1080)));
        assert!(needs_virtual_desk((-1920, 0), (1920, 1080), (1920, 1080)));
        // Same origin, different size: a rotated or non-native primary.
        assert!(needs_virtual_desk((0, 0), (1080, 1920), (1920, 1080)));
    }

    /// #124/C: the rule only fires when *both* halves hold. A locked page that
    /// never sent a delta is still driven by `m`, and dropping those would leave
    /// it unable to move the pointer at all.
    #[test]
    fn absolutes_are_suppressed_only_while_locked_and_relative() {
        assert!(!should_suppress_abs(false, false));
        assert!(!should_suppress_abs(true, false), "locked but absolute: `m` is the only motion");
        assert!(!should_suppress_abs(false, true), "deltas without a lock: keep the old path");
        assert!(should_suppress_abs(true, true));
    }

    /// #124/B: nothing to do in the middle of the display, and the far edge is
    /// `x + w - 1` — an off-by-one there means the pointer stops 1 px short and
    /// no further deltas are applied while the code believes it still has room.
    #[test]
    fn recentering_only_fires_within_the_margin_of_an_edge() {
        let screen = (0, 0, 1920, 1080);
        assert_eq!(recenter_target((960, 540), screen, 2), None);
        assert_eq!(recenter_target((500, 500), screen, 2), None);
        assert_eq!(recenter_target((0, 540), screen, 2), Some((960, 540)));
        assert_eq!(recenter_target((1, 540), screen, 2), Some((960, 540)));
        assert_eq!(recenter_target((1919, 540), screen, 2), Some((960, 540)));
        assert_eq!(recenter_target((1918, 540), screen, 2), Some((960, 540)), "last pixel - margin");
        assert_eq!(recenter_target((960, 0), screen, 2), Some((960, 540)));
        assert_eq!(recenter_target((960, 1079), screen, 2), Some((960, 540)));
        assert_eq!(recenter_target((960, 1076), screen, 2), None, "three pixels in is enough room");
    }

    /// #124/B: the target is the *monitor*, not the desktop. A session watching
    /// the right-hand screen must be recentred inside that screen: the centre of
    /// a 3840-wide desktop is the boundary between the two, i.e. an edge of the
    /// streamed one — recentring there would fire again on the next delta.
    #[test]
    fn recentering_uses_the_streamed_monitors_corner() {
        let second = (1920, 0, 1920, 1080);
        assert_eq!(recenter_target((1920, 0), second, 2), Some((2880, 540)));
        assert_eq!(
            recenter_target((1919, 0), second, 2),
            Some((2880, 540)),
            "a pointer that wandered onto the other screen is brought back to the watched one"
        );
    }

    /// #124/B: a zero-sized target (no capture yet) must not produce `(0, 0)` —
    /// that would teleport the pointer to the desktop origin on the first delta.
    #[test]
    fn recentering_refuses_a_degenerate_target() {
        assert_eq!(recenter_target((0, 0), (0, 0, 0, 0), 2), None);
        assert_eq!(recenter_target((0, 0), (0, 0, 1, 1080), 2), Some((0, 540)));
    }
}
