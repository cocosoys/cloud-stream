//! #17 virtual gamepad: ViGEmBus + `ViGEmClient.dll`.
//!
//! The browser has sent the full `js,*` vocabulary since the WR core was
//! written (`input.js:5384-5469`); this module is the missing server half.
//! Windows has no user-mode gamepad injection API — `SendInput` only reaches
//! the keyboard/mouse queue — so the industry route (Moonlight, Sunshine,
//! DS4Windows) is a signed kernel driver that enumerates a virtual Xbox 360
//! pad. ViGEmBus is that driver; `ViGEmClient.dll` is its user-mode client.
//!
//! Design constraints, all of them load-bearing:
//!
//! * **The DLL is loaded dynamically**, never linked. The deploy machine must
//!   not need a C++ toolchain, and a missing DLL has to degrade to "no gamepad"
//!   instead of "server does not start".
//! * **Every ViGEm call happens on one owned thread.** The library states it is
//!   not thread-safe, and its handles are opaque pointers.
//! * **The state lock is never held across an FFI call.** Reports are snapshotted
//!   under the lock, then pushed with the lock released.
//! * **The rumble callback owns no lock.** It runs on a thread ViGEmClient
//!   detaches internally (`ViGEmClient.cpp:790`), so it only touches atomics and
//!   an unbounded channel.
//!
//! Wire contract (`js,<verb>,...`), verified against `input.js`:
//!
//! * `js,c,<slot>,<b64 id>,<axes>,<buttons>`  slot announced. The id and the two
//!   counts are advisory — the server presents a fixed Xbox 360 pad, so a client
//!   with six axes and a client with four get the same device.
//! * `js,d,<slot>`                            slot released.
//! * `js,b,<slot>,<button>,<value>`           0..1. Buttons 6/7 are the analog
//!   triggers (`gamepad.js` maps Firefox's axes 4/5 onto them as `(v+1)/2`).
//! * `js,a,<slot>,<axis>,<value>`             -1..1, axes 0..3 = LX/LY/RX/RY.
//! * `js,h,<slot>`                            heartbeat, only while a control is
//!   held (`gamepad.js:352`). Deliberately *not* a liveness signal: a player
//!   resting their thumb sends nothing.
//!
//! Downstream, rumble leaves as `system` + `data.action = "rumble,<slot>,<strong>,
//! <weak>,<ms>"`, which `selkies-wr-core.js:3649` parses into
//! `input.rumble()` → `playEffect('dual-rumble')`.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// XInput's slot count. A fifth target is refused with
/// `VIGEM_ERROR_NO_FREE_SLOT`, which is why the frontend's `controllerSlots`
/// cannot exceed four either.
const MAX_PADS: usize = 4;

/// Rumble is a *state*, not an event stream: games re-issue the same motor
/// values every frame, and ViGEm already de-duplicates. This floor only stops a
/// title that ramps the motors smoothly from turning into 60 JSON messages a
/// second. A change to zero is never throttled — dropping the stop would leave
/// the client's pad vibrating.
const RUMBLE_MIN_GAP_MS: u64 = 50;

/// How long the pad thread may sleep before re-checking, in case a notification
/// was missed. Purely a safety net; the condvar wakes it on every change.
const IDLE_TICK: Duration = Duration::from_millis(1000);

/// `XUSB_REPORT` (`include/ViGEm/Common.h:70`), 12 bytes, 2-byte aligned.
///
/// Passed **by value** to `vigem_target_x360_update`. On x64 MSVC that is a
/// hidden pointer to a caller-allocated copy, and rustc implements the same
/// rule for `repr(C)` aggregates, so the declaration matches the header.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct XusbReport {
    buttons: u16,
    left_trigger: u8,
    right_trigger: u8,
    lx: i16,
    ly: i16,
    rx: i16,
    ry: i16,
}

// A layout drift here would corrupt every report silently, so it is checked at
// compile time rather than discovered in a game.
const _: () = assert!(std::mem::size_of::<XusbReport>() == 12);

// `XUSB_BUTTON` (`Common.h:47`).
const XUSB_DPAD_UP: u16 = 0x0001;
const XUSB_DPAD_DOWN: u16 = 0x0002;
const XUSB_DPAD_LEFT: u16 = 0x0004;
const XUSB_DPAD_RIGHT: u16 = 0x0008;
const XUSB_START: u16 = 0x0010;
const XUSB_BACK: u16 = 0x0020;
const XUSB_LEFT_THUMB: u16 = 0x0040;
const XUSB_RIGHT_THUMB: u16 = 0x0080;
const XUSB_LEFT_SHOULDER: u16 = 0x0100;
const XUSB_RIGHT_SHOULDER: u16 = 0x0200;
const XUSB_GUIDE: u16 = 0x0400;
const XUSB_A: u16 = 0x1000;
const XUSB_B: u16 = 0x2000;
const XUSB_X: u16 = 0x4000;
const XUSB_Y: u16 = 0x8000;

/// `VIGEM_ERROR_NONE` is **not** zero (`Client.h:66`). Comparing against zero
/// turns every success into a failure, which is the single easiest way to get a
/// module that "runs but never works".
const VIGEM_ERROR_NONE: u32 = 0x2000_0000;

#[inline]
fn vigem_ok(rc: u32) -> bool {
    rc == VIGEM_ERROR_NONE
}

/// Human-readable form of a `VIGEM_ERROR` for the log. Values from
/// `Client.h:66`; unknown codes print the raw hex so a newer driver is still
/// diagnosable.
fn error_text(rc: u32) -> String {
    let name = match rc {
        0xE000_0001 => "VIGEM_ERROR_BUS_NOT_FOUND (ViGEmBus driver not installed)",
        0xE000_0002 => "VIGEM_ERROR_NO_FREE_SLOT (all four XInput slots are taken)",
        0xE000_0003 => "VIGEM_ERROR_INVALID_TARGET",
        0xE000_0004 => "VIGEM_ERROR_REMOVAL_FAILED",
        0xE000_0005 => "VIGEM_ERROR_ALREADY_CONNECTED",
        0xE000_0006 => "VIGEM_ERROR_TARGET_UNINITIALIZED",
        0xE000_0007 => "VIGEM_ERROR_TARGET_NOT_PLUGGED_IN",
        0xE000_0008 => "VIGEM_ERROR_BUS_VERSION_MISMATCH",
        0xE000_0009 => "VIGEM_ERROR_BUS_ACCESS_FAILED",
        0xE000_0010 => "VIGEM_ERROR_CALLBACK_ALREADY_REGISTERED",
        0xE000_0011 => "VIGEM_ERROR_CALLBACK_NOT_FOUND",
        0xE000_0012 => "VIGEM_ERROR_BUS_ALREADY_CONNECTED",
        0xE000_0013 => "VIGEM_ERROR_BUS_INVALID_HANDLE",
        0xE000_0014 => "VIGEM_ERROR_XUSB_USERINDEX_OUT_OF_RANGE",
        0xE000_0015 => "VIGEM_ERROR_INVALID_PARAMETER",
        0xE000_0016 => "VIGEM_ERROR_NOT_SUPPORTED",
        0xE000_0017 => "VIGEM_ERROR_WINAPI",
        0xE000_0018 => "VIGEM_ERROR_TIMED_OUT",
        0xE000_0019 => "VIGEM_ERROR_IS_DISPOSING",
        other => return format!("VIGEM error 0x{other:08X}"),
    };
    name.to_owned()
}

// ---------------------------------------------------------------------------
// Wire → XUSB mapping (pure, unit-tested without a driver)
// ---------------------------------------------------------------------------

/// One report field per standard `Gamepad` button index. `None` for indexes the
/// Xbox 360 pad has no bit for (17+, and 6/7 which are analog triggers).
fn button_bit(button: u32) -> Option<u16> {
    Some(match button {
        0 => XUSB_A,
        1 => XUSB_B,
        2 => XUSB_X,
        3 => XUSB_Y,
        4 => XUSB_LEFT_SHOULDER,
        5 => XUSB_RIGHT_SHOULDER,
        8 => XUSB_BACK,
        9 => XUSB_START,
        10 => XUSB_LEFT_THUMB,
        11 => XUSB_RIGHT_THUMB,
        12 => XUSB_DPAD_UP,
        13 => XUSB_DPAD_DOWN,
        14 => XUSB_DPAD_LEFT,
        15 => XUSB_DPAD_RIGHT,
        16 => XUSB_GUIDE,
        _ => return None,
    })
}

/// Analog 0..1 → 0..255. NaN is treated as released rather than propagated: a
/// `NaN` reaching the report becomes 0 anyway, but saying so keeps the intent
/// visible.
fn trigger_byte(value: f64) -> u8 {
    if value.is_nan() {
        return 0;
    }
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// -1..1 → ±32767. Symmetric on purpose: clamping to -32768 would bias the
/// centre by one LSB, and games do compare against zero.
fn axis_value(value: f64) -> i16 {
    if value.is_nan() {
        return 0;
    }
    (value.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

fn apply_button(report: &mut XusbReport, button: u32, value: f64) {
    match button {
        // 6/7 are the triggers in the standard mapping, and are *analog*: the
        // frontend sends 0..1, so a threshold here would turn a half-pulled
        // trigger into a full one.
        6 => report.left_trigger = trigger_byte(value),
        7 => report.right_trigger = trigger_byte(value),
        _ => {
            let Some(bit) = button_bit(button) else {
                return;
            };
            if value >= 0.5 {
                report.buttons |= bit;
            } else {
                report.buttons &= !bit;
            }
        }
    }
}

fn apply_axis(report: &mut XusbReport, axis: u32, value: f64) {
    let v = axis_value(value);
    match axis {
        0 => report.lx = v,
        // Y is inverted between the two worlds: the Gamepad API reports "up" as
        // -1, XInput reports it as +32767.
        1 => report.ly = -v,
        2 => report.rx = v,
        3 => report.ry = -v,
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// FFI surface
// ---------------------------------------------------------------------------

type Notification =
    unsafe extern "system" fn(*mut c_void, *mut c_void, u8, u8, u8, *mut c_void);

/// Resolved entry points. Only the eleven this module uses; a DLL missing any of
/// them is rejected whole rather than half-usable.
struct Api {
    alloc: unsafe extern "system" fn() -> *mut c_void,
    free: unsafe extern "system" fn(*mut c_void),
    connect: unsafe extern "system" fn(*mut c_void) -> u32,
    disconnect: unsafe extern "system" fn(*mut c_void),
    x360_alloc: unsafe extern "system" fn() -> *mut c_void,
    target_free: unsafe extern "system" fn(*mut c_void),
    target_add: unsafe extern "system" fn(*mut c_void, *mut c_void) -> u32,
    target_remove: unsafe extern "system" fn(*mut c_void, *mut c_void) -> u32,
    x360_update: unsafe extern "system" fn(*mut c_void, *mut c_void, XusbReport) -> u32,
    x360_register: unsafe extern "system" fn(*mut c_void, *mut c_void, Notification, *mut c_void) -> u32,
    x360_unregister: unsafe extern "system" fn(*mut c_void),
}

impl Api {
    /// Load `ViGEmClient.dll` from the executable's directory and resolve the
    /// entry points. The DLL is vendored (`vendor/vigem/`) and deployed next to
    /// `stream-server.exe`, so a bare `LoadLibraryW` finds it: the application
    /// directory is first in the default search order.
    fn load() -> Result<Api, String> {
        const DLL: &str = "ViGEmClient.dll";
        let name: Vec<u16> = DLL.encode_utf16().chain(std::iter::once(0)).collect();
        let module = unsafe { LoadLibraryW(name.as_ptr()) };
        if module == 0 {
            return Err(format!(
                "{DLL} not loadable next to the executable (Win32 error {})",
                unsafe { GetLastError() }
            ));
        }

        macro_rules! sym {
            ($sym:literal, $ty:ty) => {{
                let proc = unsafe {
                    GetProcAddress(module, concat!($sym, "\0").as_ptr() as *const u8)
                };
                match proc {
                    // Function pointers of different signatures are the same
                    // size; the header is the only authority on which is which.
                    Some(f) => unsafe { std::mem::transmute_copy::<_, $ty>(&f) },
                    None => return Err(format!("{DLL} does not export {}", $sym)),
                }
            }};
        }

        Ok(Api {
            alloc: sym!("vigem_alloc", unsafe extern "system" fn() -> *mut c_void),
            free: sym!("vigem_free", unsafe extern "system" fn(*mut c_void)),
            connect: sym!("vigem_connect", unsafe extern "system" fn(*mut c_void) -> u32),
            disconnect: sym!("vigem_disconnect", unsafe extern "system" fn(*mut c_void)),
            x360_alloc: sym!("vigem_target_x360_alloc", unsafe extern "system" fn() -> *mut c_void),
            target_free: sym!("vigem_target_free", unsafe extern "system" fn(*mut c_void)),
            target_add: sym!(
                "vigem_target_add",
                unsafe extern "system" fn(*mut c_void, *mut c_void) -> u32
            ),
            target_remove: sym!(
                "vigem_target_remove",
                unsafe extern "system" fn(*mut c_void, *mut c_void) -> u32
            ),
            x360_update: sym!(
                "vigem_target_x360_update",
                unsafe extern "system" fn(*mut c_void, *mut c_void, XusbReport) -> u32
            ),
            x360_register: sym!(
                "vigem_target_x360_register_notification",
                unsafe extern "system" fn(*mut c_void, *mut c_void, Notification, *mut c_void) -> u32
            ),
            x360_unregister: sym!(
                "vigem_target_x360_unregister_notification",
                unsafe extern "system" fn(*mut c_void)
            ),
        })
    }
}

/// What the notification callback needs: which slot it speaks for, and a way
/// back to the session that owns the pad.
///
/// **Never freed.** `vigem_target_x360_register_notification` detaches an
/// internal thread that pumps the notification IOCTL
/// (`ViGEmClient.cpp:790-834`) and invokes the callback from it; nothing tells
/// the caller when that thread is done, so releasing this on `js,d` (or on
/// session teardown) is a use-after-free waiting for the right interleaving.
/// One 24-byte allocation per slot per session is the cheap side of that trade.
///
/// The `Weak` is what keeps it honest: a callback that fires after its session
/// is gone upgrades to nothing and returns.
struct CallbackCtx {
    inner: Weak<Inner>,
    slot: u8,
}

unsafe extern "system" fn on_rumble(
    _client: *mut c_void,
    _target: *mut c_void,
    large: u8,
    small: u8,
    _led: u8,
    user_data: *mut c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: only ever set to a `Box::leak`ed `CallbackCtx`, which lives for
    // the rest of the process.
    let ctx = unsafe { &*(user_data as *const CallbackCtx) };
    if let Some(inner) = ctx.inner.upgrade() {
        inner.note_rumble(ctx.slot, large, small);
    }
}

// ---------------------------------------------------------------------------
// Hub
// ---------------------------------------------------------------------------

/// Per-slot virtual pad state, written by the input path and drained by the pad
/// thread.
#[derive(Clone, Copy, Default)]
struct Pad {
    /// The client says a pad drives this slot (`js,c` seen, `js,d` not yet).
    want: bool,
    /// The report changed since the pad thread last pushed it.
    dirty: bool,
    report: XusbReport,
}

/// One rumble edge worth forwarding to the browser.
pub struct Rumble {
    pub slot: u8,
    /// Strong (left, low-frequency) motor, 0..1.
    pub large: f32,
    /// Weak (right, high-frequency) motor, 0..1.
    pub small: f32,
}

struct Inner {
    pads: Mutex<[Pad; MAX_PADS]>,
    cv: Condvar,
    shutdown: AtomicBool,
    rumble_tx: UnboundedSender<Rumble>,
    rumble_rx: Mutex<Option<UnboundedReceiver<Rumble>>>,
    /// Last motor pair sent per slot, packed `large << 8 | small`.
    prev_rumble: [AtomicU16; MAX_PADS],
    /// When the last non-zero rumble went out, in milliseconds since `started`,
    /// biased by one so 0 means "never".
    last_rumble_ms: [AtomicU64; MAX_PADS],
    started: Instant,
    /// `Some(reason)` once loading or connecting failed; the session logs it once.
    unavailable: Mutex<Option<String>>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, [Pad; MAX_PADS]> {
        // A poisoned lock means the pad thread panicked mid-update; the state is
        // still structurally valid (plain data), so recovering beats wedging the
        // input path.
        self.pads.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Called from ViGEmClient's detached notification thread.
    ///
    /// Lock-free by construction: atomics plus an unbounded send. Taking
    /// `pads` here would deadlock against the pad thread holding it while
    /// calling `x360_update`, because that call is what pumps this thread's
    /// IOCTL in the first place.
    fn note_rumble(&self, slot: u8, large: u8, small: u8) {
        let idx = slot as usize;
        if idx >= MAX_PADS {
            return;
        }
        let packed = u16::from(large) << 8 | u16::from(small);
        if self.prev_rumble[idx].swap(packed, Ordering::Relaxed) == packed {
            return;
        }
        // A stop is never dropped: the frontend's `playEffect` runs for five
        // seconds, so swallowing "0,0" leaves the pad buzzing.
        if packed != 0 {
            // Milliseconds since the hub started, biased by one so that 0 can
            // mean "never sent" — without the bias the very first rumble of a
            // session would be throttled against the epoch.
            let now = self.started.elapsed().as_millis() as u64 + 1;
            let last = self.last_rumble_ms[idx].load(Ordering::Relaxed);
            if last != 0 && now.saturating_sub(last) < RUMBLE_MIN_GAP_MS {
                return;
            }
            self.last_rumble_ms[idx].store(now, Ordering::Relaxed);
        }
        let _ = self.rumble_tx.send(Rumble {
            slot,
            large: f32::from(large) / 255.0,
            small: f32::from(small) / 255.0,
        });
    }
}

/// The virtual gamepad bus for one session.
///
/// Cheap to construct: the DLL is loaded and the bus connected on the owned
/// thread, so a missing driver never delays or fails a connection.
pub struct GamepadHub {
    inner: Arc<Inner>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl GamepadHub {
    pub fn spawn() -> Arc<Self> {
        let (rumble_tx, rumble_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            pads: Mutex::new([Pad::default(); MAX_PADS]),
            cv: Condvar::new(),
            shutdown: AtomicBool::new(false),
            rumble_tx,
            rumble_rx: Mutex::new(Some(rumble_rx)),
            prev_rumble: [
                AtomicU16::new(0),
                AtomicU16::new(0),
                AtomicU16::new(0),
                AtomicU16::new(0),
            ],
            last_rumble_ms: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            started: Instant::now(),
            unavailable: Mutex::new(None),
        });

        let thread_inner = inner.clone();
        let thread = std::thread::Builder::new()
            .name("gamepad".to_owned())
            .spawn(move || pad_thread(thread_inner))
            .map_err(|err| {
                log::warn!("gamepad thread could not start: {err}");
                err
            })
            .ok();

        Arc::new(Self {
            inner,
            thread: Mutex::new(thread),
        })
    }

    /// The rumble receiver, handed to the control-channel pump once per session.
    /// `None` after the first call: an ICE reconnect re-arms the notices but
    /// must not install a second pump.
    pub fn take_rumble_rx(&self) -> Option<UnboundedReceiver<Rumble>> {
        self.inner
            .rumble_rx
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
    }

    /// Why the pads are not available — `None` while usable or before the thread
    /// has decided.
    ///
    /// The pad thread already logs the same reason once at startup, so nothing
    /// in the session calls this today; it is kept for `/api/v1/health` and for
    /// a caller that wants to answer "why is my controller not working" without
    /// grepping the log. `dead_code` is expected and deliberate.
    #[allow(dead_code)]
    pub fn unavailable_reason(&self) -> Option<String> {
        self.inner
            .unavailable
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Stop the pad thread and disconnect the bus **now**, without waiting for
    /// the last `Arc<GamepadHub>` to go away.
    ///
    /// [`Drop`] does the same thing, but `Drop` only runs when the final holder
    /// of the `Arc` lets go — and one of those holders is the input sink, which
    /// the data-channel closures close over. A teardown path that leaves a
    /// channel behind therefore keeps the pad thread, the ViGEm bus client and
    /// the DLL mapping alive until that clone drops, which may be never. This
    /// was measured on the target host as four `connected` lines against two
    /// `bus disconnected` over four sessions, with one extra thread per reload.
    ///
    /// Calling it where the session actually ends makes the release
    /// unconditional: the pads are unplugged and the bus client freed there, and
    /// a lingering `Arc` afterwards is an idle struct with a latch set, not a
    /// thread holding a driver connection. Idempotent, and safe after the thread
    /// has already exited.
    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        self.inner.cv.notify_all();
    }

    /// Drop every pad at once (session teardown).
    ///
    /// A closed tab never sends `js,d`, and a virtual pad is an OS-level device
    /// that outlives the page: without this the next connection finds a ghost
    /// controller holding XInput slot 0 and the game reads nothing.
    pub fn release_all(&self) {
        let mut changed = false;
        {
            let mut pads = self.inner.lock();
            for pad in pads.iter_mut() {
                if pad.want {
                    pad.want = false;
                    changed = true;
                }
            }
        }
        if changed {
            self.inner.cv.notify_all();
        }
    }

    /// Parse one `js,...` line. `rest` is everything after `js,`.
    pub fn handle_js(&self, rest: &str) {
        let mut fields = rest.splitn(4, ',');
        let verb = fields.next().unwrap_or("");
        match verb {
            "c" => {
                let Some(slot) = fields.next().and_then(parse_slot) else {
                    return log::debug!("[gamepad] malformed js,c: js,{rest}");
                };
                self.attach(slot);
            }
            "d" => {
                let Some(slot) = fields.next().and_then(parse_slot) else {
                    return log::debug!("[gamepad] malformed js,d: js,{rest}");
                };
                self.detach(slot);
            }
            "b" => {
                let (Some(slot), Some(button)) =
                    (fields.next().and_then(parse_slot), fields.next().and_then(parse_u32))
                else {
                    return log::debug!("[gamepad] malformed js,b: js,{rest}");
                };
                let Some(value) = fields.next().and_then(parse_f64) else {
                    return log::debug!("[gamepad] malformed js,b value: js,{rest}");
                };
                self.mutate(slot, |report| apply_button(report, button, value));
            }
            "a" => {
                let (Some(slot), Some(axis)) =
                    (fields.next().and_then(parse_slot), fields.next().and_then(parse_u32))
                else {
                    return log::debug!("[gamepad] malformed js,a: js,{rest}");
                };
                let Some(value) = fields.next().and_then(parse_f64) else {
                    return log::debug!("[gamepad] malformed js,a value: js,{rest}");
                };
                self.mutate(slot, |report| apply_axis(report, axis, value));
            }
            // Heartbeat: only sent while a control is held, so it says nothing
            // about whether the pad is still plugged in. Ignored deliberately.
            "h" => {}
            other => log::debug!("[gamepad] unknown js verb {other:?}: js,{rest}"),
        }
    }

    fn attach(&self, slot: u32) {
        {
            let mut pads = self.inner.lock();
            let pad = &mut pads[slot as usize];
            pad.want = true;
            // A slot can be re-announced by a different physical pad; starting
            // from a zeroed report keeps the previous pad's held buttons from
            // sticking on the host.
            pad.report = XusbReport::default();
            pad.dirty = true;
        }
        self.inner.cv.notify_all();
    }

    fn detach(&self, slot: u32) {
        {
            let mut pads = self.inner.lock();
            let pad = &mut pads[slot as usize];
            pad.want = false;
            pad.report = XusbReport::default();
        }
        self.inner.cv.notify_all();
    }

    /// Apply a report change. A report for a slot that never announced itself
    /// still creates the pad: `js,c` and the first `js,b` can straddle a control
    /// channel that has only just opened, and dropping the button press would
    /// look like a broken controller.
    fn mutate(&self, slot: u32, apply: impl FnOnce(&mut XusbReport)) {
        {
            let mut pads = self.inner.lock();
            let pad = &mut pads[slot as usize];
            pad.want = true;
            apply(&mut pad.report);
            pad.dirty = true;
        }
        self.inner.cv.notify_all();
    }
}

impl Drop for GamepadHub {
    fn drop(&mut self) {
        // Same latch the session sets in `shutdown`; setting it twice is what
        // makes the explicit call and the drop-order path interchangeable.
        self.shutdown();
        let handle = self
            .thread
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        if let Some(handle) = handle {
            // The thread unplugs every pad and disconnects before returning, so
            // joining here is what guarantees "no ghost controller" even when
            // the process keeps running for the next session.
            let _ = handle.join();
        }
    }
}

fn parse_slot(field: &str) -> Option<u32> {
    field
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|slot| (*slot as usize) < MAX_PADS)
}

fn parse_u32(field: &str) -> Option<u32> {
    field.trim().parse::<u32>().ok()
}

fn parse_f64(field: &str) -> Option<f64> {
    field.trim().parse::<f64>().ok()
}

/// The owned thread: load → connect → drain the state machine → unplug.
fn pad_thread(inner: Arc<Inner>) {
    let api = match Api::load() {
        Ok(api) => api,
        Err(reason) => {
            log::info!("gamepad: unavailable — {reason}");
            *inner
                .unavailable
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(reason);
            return;
        }
    };

    let client = unsafe { (api.alloc)() };
    if client.is_null() {
        let reason = "vigem_alloc returned null".to_owned();
        log::info!("gamepad: unavailable — {reason}");
        *inner
            .unavailable
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(reason);
        return;
    }

    let connected = unsafe { (api.connect)(client) };
    if !vigem_ok(connected) {
        let reason = error_text(connected);
        log::info!("gamepad: unavailable — {reason}");
        *inner
            .unavailable
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(reason);
        unsafe { (api.free)(client) };
        return;
    }
    log::info!("gamepad: ViGEmBus connected; virtual Xbox 360 pads are created on `js,c`");

    // Raw targets are plain OS handles owned by this thread only.
    let mut targets: [*mut c_void; MAX_PADS] = [std::ptr::null_mut(); MAX_PADS];

    loop {
        for slot in 0..MAX_PADS {
            // Snapshot, then release: the notification thread may run during the
            // update call below and must never find this lock held.
            let (want, dirty, report) = {
                let mut pads = inner.lock();
                let pad = &mut pads[slot];
                let snapshot = (pad.want, pad.dirty, pad.report);
                pad.dirty = false;
                snapshot
            };

            let attached = !targets[slot].is_null();

            if want && !attached {
                targets[slot] = attach_pad(&api, client, &inner, slot);
                // The report that arrived before the pad existed is applied on
                // the next beat rather than lost.
                continue;
            }

            if !want && attached {
                detach_pad(&api, client, &mut targets[slot]);
                continue;
            }

            if attached && dirty {
                let rc = unsafe { (api.x360_update)(client, targets[slot], report) };
                if !vigem_ok(rc) {
                    log::warn!("gamepad: slot {slot} update failed: {}", error_text(rc));
                }
            }
        }

        if inner.shutting_down() && targets.iter().all(|target| target.is_null()) {
            break;
        }

        // Sleep until something changes. The predicate is re-checked under the
        // lock, so a notify that lands between the last check and the wait is
        // not lost.
        let mut pads = inner.lock();
        while !inner.shutting_down()
            && !pads
                .iter()
                .enumerate()
                .any(|(slot, pad)| pad.dirty || pad.want != !targets[slot].is_null())
        {
            let (guard, _timeout) = inner
                .cv
                .wait_timeout(pads, IDLE_TICK)
                .unwrap_or_else(|poison| poison.into_inner());
            pads = guard;
        }
        drop(pads);

        // Shutdown with pads still attached: unplug them before the break.
        if inner.shutting_down() {
            for slot in 0..MAX_PADS {
                if !targets[slot].is_null() {
                    detach_pad(&api, client, &mut targets[slot]);
                }
            }
            break;
        }
    }

    unsafe { (api.disconnect)(client) };
    unsafe { (api.free)(client) };
    log::info!("gamepad: bus disconnected, virtual pads removed");
}

/// Create and plug in one virtual pad. Returns the target, or null on failure
/// (a failure is logged, not fatal: the rest of the session is unaffected).
fn attach_pad(api: &Api, client: *mut c_void, inner: &Arc<Inner>, slot: usize) -> *mut c_void {
    let target = unsafe { (api.x360_alloc)() };
    if target.is_null() {
        log::warn!("gamepad: slot {slot} target allocation failed");
        return std::ptr::null_mut();
    }

    let added = unsafe { (api.target_add)(client, target) };
    if !vigem_ok(added) {
        // `VIGEM_ERROR_NO_FREE_SLOT` lands here when four other pads (virtual or
        // physical) already hold every XInput position.
        log::warn!("gamepad: slot {slot} add failed: {}", error_text(added));
        unsafe { (api.target_free)(target) };
        return std::ptr::null_mut();
    }

    // Rumble is a bonus: the pad works without it, so a registration failure
    // must not tear the device down.
    let ctx = Box::leak(Box::new(CallbackCtx {
        inner: Arc::downgrade(inner),
        slot: slot as u8,
    }));
    let registered = unsafe {
        (api.x360_register)(
            client,
            target,
            on_rumble,
            ctx as *mut CallbackCtx as *mut c_void,
        )
    };
    if !vigem_ok(registered) {
        log::warn!(
            "gamepad: slot {slot} rumble subscription failed (input still works): {}",
            error_text(registered)
        );
    }

    // The XInput index is assigned by insertion order, so a slot announced out
    // of order lands on a different position than the client assumes. Worth a
    // line in the log rather than silent misrouting.
    log::info!("gamepad: slot {slot} plugged in");

    target
}

fn detach_pad(api: &Api, client: *mut c_void, target: &mut *mut c_void) {
    if target.is_null() {
        return;
    }
    unsafe { (api.x360_unregister)(*target) };
    let removed = unsafe { (api.target_remove)(client, *target) };
    if !vigem_ok(removed) {
        log::warn!("gamepad: target remove failed: {}", error_text(removed));
    }
    unsafe { (api.target_free)(*target) };
    *target = std::ptr::null_mut();
    log::info!("gamepad: pad unplugged");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hub with no thread: the wire parser and the report mapping are what is
    /// under test, and neither needs a driver.
    fn detached() -> GamepadHub {
        let (tx, rx) = mpsc::unbounded_channel();
        GamepadHub {
            inner: Arc::new(Inner {
                pads: Mutex::new([Pad::default(); MAX_PADS]),
                cv: Condvar::new(),
                shutdown: AtomicBool::new(false),
                rumble_tx: tx,
                rumble_rx: Mutex::new(Some(rx)),
                prev_rumble: [
                    AtomicU16::new(0),
                    AtomicU16::new(0),
                    AtomicU16::new(0),
                    AtomicU16::new(0),
                ],
                last_rumble_ms: [
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                ],
                started: Instant::now(),
                unavailable: Mutex::new(None),
            }),
            thread: Mutex::new(None),
        }
    }

    fn report_of(hub: &GamepadHub, slot: usize) -> XusbReport {
        hub.inner.lock()[slot].report
    }

    /// The C struct is 12 bytes; anything else and every report is garbage.
    #[test]
    fn report_matches_the_c_struct() {
        assert_eq!(std::mem::size_of::<XusbReport>(), 12);
        assert_eq!(std::mem::align_of::<XusbReport>(), 2);
    }

    /// The whole standard layout, button by button. A transposed pair here is
    /// the classic "A and B are swapped" report.
    #[test]
    fn every_standard_button_lands_on_its_bit() {
        let expected = [
            (0, XUSB_A),
            (1, XUSB_B),
            (2, XUSB_X),
            (3, XUSB_Y),
            (4, XUSB_LEFT_SHOULDER),
            (5, XUSB_RIGHT_SHOULDER),
            (8, XUSB_BACK),
            (9, XUSB_START),
            (10, XUSB_LEFT_THUMB),
            (11, XUSB_RIGHT_THUMB),
            (12, XUSB_DPAD_UP),
            (13, XUSB_DPAD_DOWN),
            (14, XUSB_DPAD_LEFT),
            (15, XUSB_DPAD_RIGHT),
            (16, XUSB_GUIDE),
        ];
        for (index, bit) in expected {
            let mut report = XusbReport::default();
            apply_button(&mut report, index, 1.0);
            assert_eq!(report.buttons, bit, "button {index} mapped wrong");
            apply_button(&mut report, index, 0.0);
            assert_eq!(report.buttons, 0, "button {index} did not release");
        }
    }

    /// Triggers are analog: half a pull must be half the byte, not a press.
    #[test]
    fn triggers_are_analog_not_switches() {
        let mut report = XusbReport::default();
        apply_button(&mut report, 6, 0.5);
        assert_eq!(report.left_trigger, 128);
        assert_eq!(report.buttons & 0xC000, 0, "trigger leaked into a button bit");
        apply_button(&mut report, 7, 1.0);
        assert_eq!(report.right_trigger, 255);
        apply_button(&mut report, 6, 0.0);
        assert_eq!(report.left_trigger, 0);
    }

    /// Browser "+Y" is up, XInput "+Y" is up too — but the *values* are mirrored
    /// (browser up = -1), so the axis has to be negated or every right stick
    /// camera is inverted.
    #[test]
    fn thumbsticks_are_inverted_on_y() {
        let mut report = XusbReport::default();
        apply_axis(&mut report, 0, -1.0);
        apply_axis(&mut report, 1, -1.0);
        apply_axis(&mut report, 2, 1.0);
        apply_axis(&mut report, 3, 0.5);
        assert_eq!(report.lx, -32767);
        assert_eq!(report.ly, 32767);
        assert_eq!(report.rx, 32767);
        assert_eq!(report.ry, -16384);
    }

    /// Out-of-range values clamp instead of wrapping; NaN reads as centred.
    #[test]
    fn out_of_range_values_clamp() {
        let mut report = XusbReport::default();
        apply_axis(&mut report, 0, 9.0);
        assert_eq!(report.lx, 32767);
        apply_axis(&mut report, 0, -9.0);
        assert_eq!(report.lx, -32767);
        apply_axis(&mut report, 0, f64::NAN);
        assert_eq!(report.lx, 0);
        apply_button(&mut report, 6, 4.0);
        assert_eq!(report.left_trigger, 255);
    }

    /// Button 17+ has no XUSB bit; it must not touch the report at all.
    #[test]
    fn unknown_buttons_are_ignored() {
        let mut report = XusbReport { buttons: XUSB_A, ..Default::default() };
        apply_button(&mut report, 17, 1.0);
        apply_button(&mut report, 6, 0.0);
        assert_eq!(report.buttons, XUSB_A);
        assert_eq!(report.left_trigger, 0);
    }

    /// Axis 4+ is not a thumbstick (Firefox reports triggers there, and the
    /// frontend already converts them to buttons 6/7).
    #[test]
    fn unknown_axes_are_ignored() {
        let mut report = XusbReport::default();
        apply_axis(&mut report, 4, 0.75);
        apply_axis(&mut report, 5, -0.75);
        assert_eq!(report, XusbReport::default());
    }

    /// `js,c` / `js,b` / `js,a` / `js,d` in the shapes `input.js` actually
    /// emits, including the base64 id that must not be parsed as a number.
    #[test]
    fn wire_lines_drive_the_slot_state() {
        let hub = detached();
        hub.handle_js("c,0,WGJveCAzNjA=,6,17");
        assert!(hub.inner.lock()[0].want);

        hub.handle_js("a,0,1,-1");
        assert_eq!(report_of(&hub, 0).ly, 32767);

        hub.handle_js("b,0,6,1");
        assert_eq!(report_of(&hub, 0).left_trigger, 255);

        hub.handle_js("h,0");
        assert!(hub.inner.lock()[0].want, "heartbeat must not create or drop a pad");

        hub.handle_js("d,0");
        assert!(!hub.inner.lock()[0].want);
        assert_eq!(report_of(&hub, 0), XusbReport::default());
    }

    /// A report before `js,c` still creates the pad: the two can straddle a
    /// channel that just opened, and a dropped press looks like a dead pad.
    #[test]
    fn a_report_before_the_announcement_still_attaches() {
        let hub = detached();
        hub.handle_js("b,1,0,1");
        assert!(hub.inner.lock()[1].want);
        assert_eq!(report_of(&hub, 1).buttons, XUSB_A);
    }

    /// Garbage must be dropped, not applied to slot 0.
    #[test]
    fn malformed_lines_change_nothing() {
        let hub = detached();
        for line in [
            "c", "c,", "c,9", "c,x", "d,", "b,0", "b,0,", "b,0,6", "b,0,6,x", "a,0,1",
            "a,0,", "z,0,1", "",
        ] {
            hub.handle_js(line);
        }
        let pads = hub.inner.lock();
        for pad in pads.iter() {
            assert!(!pad.want, "malformed input created a pad");
            assert_eq!(pad.report, XusbReport::default());
        }
    }

    /// `release_all` is what stops a closed tab from leaving a ghost controller
    /// on XInput slot 0.
    #[test]
    fn release_all_drops_every_slot() {
        let hub = detached();
        hub.handle_js("c,0,QUJD,6,17");
        hub.handle_js("c,1,QUJD,6,17");
        hub.release_all();
        for pad in hub.inner.lock().iter() {
            assert!(!pad.want);
        }
    }

    /// The rumble pump is taken once per session: an ICE reconnect re-arms the
    /// notices and must not install a second consumer.
    #[test]
    fn the_rumble_receiver_is_handed_out_once() {
        let hub = detached();
        assert!(hub.take_rumble_rx().is_some());
        assert!(hub.take_rumble_rx().is_none());
    }

    /// The session calls `shutdown`; `Drop` may run much later — or never, if a
    /// clone of the input sink outlives it. Both must agree on the same latch,
    /// and a second call must be harmless (`Drop` sets it again).
    #[test]
    fn shutdown_latches_once_and_twice() {
        let hub = detached();
        assert!(!hub.inner.shutting_down());
        hub.shutdown();
        assert!(hub.inner.shutting_down(), "teardown must stop the thread");
        hub.shutdown();
        assert!(
            hub.inner.shutting_down(),
            "Drop's second latch must not undo the first"
        );
    }

    /// A repeated motor value is not re-sent, and a stop is never throttled.
    #[test]
    fn rumble_is_deduplicated_but_stops_always_go_through() {
        let hub = detached();
        let mut rx = hub.take_rumble_rx().expect("receiver");
        hub.inner.note_rumble(0, 200, 100);
        hub.inner.note_rumble(0, 200, 100);
        assert!(rx.try_recv().is_ok(), "first change must be forwarded");
        assert!(rx.try_recv().is_err(), "repeat must be dropped");

        hub.inner.note_rumble(0, 0, 0);
        let stop = rx.try_recv().expect("stop must be forwarded");
        assert_eq!((stop.large, stop.small), (0.0, 0.0));
    }

    /// A game that ramps the motors smoothly must not produce one JSON message
    /// per frame; the floor is `RUMBLE_MIN_GAP_MS`. The *first* edge of a
    /// session is exempt — throttling it against the epoch would silently drop
    /// the opening rumble of every session.
    #[test]
    fn the_first_rumble_survives_the_throttle() {
        let hub = detached();
        let mut rx = hub.take_rumble_rx().expect("receiver");
        hub.inner.note_rumble(0, 10, 0);
        assert!(rx.try_recv().is_ok(), "the first edge must not be throttled");

        hub.inner.note_rumble(0, 20, 0);
        assert!(
            rx.try_recv().is_err(),
            "a change inside the gap must be coalesced away"
        );
    }

    // -----------------------------------------------------------------------
    // Hardware probe: ignored by default, needs ViGEmBus + ViGEmClient.dll.
    //
    //   cargo test --release -p stream-server hardware_roundtrip -- --ignored --nocapture
    //
    // This is the only test that proves the *ABI*: it plugs a pad in, pushes a
    // distinctive report, then reads the same state back through XInput — the
    // exact API a game uses. A wrong `XusbReport` layout or a wrong by-value
    // calling convention shows up here as "the game never sees the input",
    // which is otherwise a very slow thing to diagnose from a game.
    // -----------------------------------------------------------------------

    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug)]
    struct XinputGamepad {
        buttons: u16,
        left_trigger: u8,
        right_trigger: u8,
        lx: i16,
        ly: i16,
        rx: i16,
        ry: i16,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug)]
    struct XinputState {
        packet_number: u32,
        gamepad: XinputGamepad,
    }

    type XInputGetState =
        unsafe extern "system" fn(u32, *mut XinputState) -> u32;

    fn xinput_get_state() -> Option<XInputGetState> {
        // xinput1_4 ships with Windows 8+; 1_3/9_1_0 are the fallbacks a game
        // would use on older systems.
        for name in ["xinput1_4.dll", "xinput1_3.dll", "xinput9_1_0.dll"] {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let module = unsafe { LoadLibraryW(wide.as_ptr()) };
            if module == 0 {
                continue;
            }
            let proc = unsafe { GetProcAddress(module, b"XInputGetState\0".as_ptr()) };
            if let Some(proc) = proc {
                return Some(unsafe { std::mem::transmute_copy(&proc) });
            }
        }
        None
    }

    #[test]
    #[ignore = "needs ViGEmBus installed and ViGEmClient.dll next to the test binary"]
    fn hardware_roundtrip() {
        let api = Api::load().expect("ViGEmClient.dll");
        let get_state = xinput_get_state().expect("XInputGetState");

        let client = unsafe { (api.alloc)() };
        assert!(!client.is_null(), "vigem_alloc");
        let connected = unsafe { (api.connect)(client) };
        assert!(vigem_ok(connected), "vigem_connect: {}", error_text(connected));

        let target = unsafe { (api.x360_alloc)() };
        assert!(!target.is_null(), "vigem_target_x360_alloc");
        let added = unsafe { (api.target_add)(client, target) };
        assert!(vigem_ok(added), "vigem_target_add: {}", error_text(added));

        // A-value no game would produce by accident: A down, left stick hard
        // right, right trigger half, left trigger a quarter.
        let mut report = XusbReport::default();
        apply_button(&mut report, 0, 1.0);
        apply_axis(&mut report, 0, 1.0);
        apply_axis(&mut report, 1, -1.0);
        apply_button(&mut report, 6, 0.25);
        apply_button(&mut report, 7, 0.5);
        let pushed = unsafe { (api.x360_update)(client, target, report) };
        assert!(vigem_ok(pushed), "vigem_target_x360_update: {}", error_text(pushed));

        // Enumerating a freshly plugged pad takes a moment, and it may not land
        // on index 0 if the host already has controllers.
        let mut seen = None;
        for _ in 0..40 {
            for index in 0..4u32 {
                let mut state = XinputState::default();
                if unsafe { get_state(index, &mut state) } == 0
                    && state.gamepad.buttons == XUSB_A
                {
                    seen = Some((index, state.gamepad));
                    break;
                }
            }
            if seen.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let (index, readback) = seen.expect("the virtual pad never appeared on XInput");
        println!("XInput index {index}: {readback:?}");
        assert_eq!(readback.buttons, XUSB_A, "buttons");
        assert_eq!(readback.lx, 32767, "left stick X");
        assert_eq!(readback.ly, 32767, "left stick Y (inverted on the wire)");
        assert_eq!(readback.left_trigger, 64, "left trigger (0.25)");
        assert_eq!(readback.right_trigger, 128, "right trigger (0.5)");

        let removed = unsafe { (api.target_remove)(client, target) };
        assert!(vigem_ok(removed), "vigem_target_remove: {}", error_text(removed));
        unsafe { (api.target_free)(target) };
        unsafe { (api.disconnect)(client) };
        unsafe { (api.free)(client) };
    }
}
