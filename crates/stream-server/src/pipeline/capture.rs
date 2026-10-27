//! Display enumeration and frame acquisition (design 8.1 row 1; stage 4.3).
//!
//! Stage1 boundary: fix on `is_primary()` for the only screen, no multi-monitor
//! polling. #42-1 adds the RustDesk-aligned backend switch (`set_gdi`) used by
//! the DXGI finite-recovery chain; a fatal open failure still aborts after the
//! outer recovery backoff budget is exhausted (design stage1-capture-recovery.md
//! section 4.3).
//!
//! Assembly mirrors the 0e bench chain (`crates/bench-capture/src/main.rs:55-74`):
//! `Display::all()` -> prefer `is_primary()` -> `Capturer::new(display)`.
//!
//! #65: the capturer now runs on its own thread (`CaptureWorker`) and publishes
//! the newest frame into a one-slot mailbox; the encode beat takes it with a
//! mutex + swap (microseconds) instead of calling `frame()` itself. Measured
//! reason: a single `frame()` call cost 8.5-11 ms on this VM (mapped-surface
//! compare inside scrap) which alone ate more than half of a 60 fps beat, so the
//! beat could never reach 16.7 ms. The 6.4 MB copy that used to sit on the beat
//! now also happens here, on the capture thread's own budget.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use scrap::{Capturer, Display, Frame, TraitCapturer, TraitPixelBuffer};

/// How long the worker blocks inside `frame()`. Long enough to stay off the CPU
/// when the desktop is static, short enough that new content is picked up well
/// inside one beat.
pub const CAPTURE_POLL: Duration = Duration::from_millis(20);

/// Holds the primary-display capturer for the loop's lifetime.
pub struct CaptureSource {
    capturer: Capturer,
    width: u32,
    height: u32,
    /// Recorded for the `stream_info` node line; not read by the loop.
    #[allow(dead_code)]
    name: String,
}

impl CaptureSource {
    /// Enumerate displays, select the primary (fall back to the first listed
    /// display) and build the capturer. Records native pixels, backend kind and
    /// the display name once at open time.
    pub fn open_primary() -> anyhow::Result<Self> {
        let displays = Display::all()?;
        if displays.is_empty() {
            anyhow::bail!("no display found on this session");
        }
        let mut primary: Option<Display> = None;
        let mut first: Option<Display> = None;
        for d in displays {
            if d.is_primary() {
                primary = Some(d);
                break;
            }
            if first.is_none() {
                first = Some(d);
            }
        }
        let display = primary.or(first).expect("display list non-empty");

        let width = display.width() as u32;
        let height = display.height() as u32;
        let name = display.name();

        let capturer = Capturer::new(display)?;
        let gdi = capturer.is_gdi();
        log::info!(
            "capture source opened: {width}x{height} gdi={gdi} display={name}",
        );
        Ok(Self {
            capturer,
            width,
            height,
            name,
        })
    }

    /// Acquire one frame within `timeout`; `WouldBlock` maps straight to the
    /// repeat-encode path upstream (video_service.rs:818-955 pattern).
    pub fn frame(&mut self, timeout: Duration) -> io::Result<Frame<'_>> {
        self.capturer.frame(timeout)
    }

    /// True when the active backend is the GDI fallback; reads the live capturer
    /// so a `set_gdi` switch is reflected immediately in `stream_info`.
    pub fn is_gdi(&self) -> bool {
        self.capturer.is_gdi()
    }

    /// Mount the bundled GDI capturer over the DXGI duplication path
    /// (video_service.rs `set_gdi`). Returns the backend flag reported by scrap.
    /// The DXGI object is kept alive; later frames route through GDI first
    /// (`scrap/src/dxgi/mod.rs:527-545`).
    pub fn set_gdi(&mut self) -> bool {
        self.capturer.set_gdi()
    }

    /// Native pixels of the selected display, read once at open time.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Display name captured at open time for the log line and stream_info reason.
    #[allow(dead_code)]
    pub fn display_name(&self) -> &str {
        &self.name
    }
}

/// One frame handed from the capture thread to the encode beat. Owned BGRA
/// bytes, tightly packed (`width * height * 4`).
pub struct CapturedFrame {
    /// Monotonic publish counter; the beat uses it for its own accounting.
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// The mailbox between the two threads. `frame` holds at most one unconsumed
/// frame (newest wins), `spare` is the buffer the beat hands back so the worker
/// reuses it instead of allocating 6.4 MB per frame.
struct Mailbox {
    frame: Option<CapturedFrame>,
    spare: Option<Vec<u8>>,
    /// First unconsumed capture error; the beat loop owns the recovery chain and
    /// clears it, which also releases the worker to poll again.
    error: Option<(io::ErrorKind, String)>,
    seq: u64,
    /// Set once the capture thread has an open capturer (and `width`/`height`).
    ready: bool,
    /// Set when `open_primary()` failed inside the thread; the outer scheduler
    /// owns the retry/backoff policy.
    open_error: Option<String>,
}

struct Shared {
    mailbox: Mutex<Mailbox>,
    /// Signalled when the beat consumes (or discards) the published frame, so
    /// the worker fetches at most one frame per beat instead of spinning.
    consumed: Condvar,
    stop: AtomicBool,
    gdi_request: AtomicBool,
    is_gdi: AtomicBool,
    width: AtomicU32,
    height: AtomicU32,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Mailbox> {
        self.mailbox.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Publish one frame, newest wins; the previous unconsumed buffer is kept as
    /// the next spare.
    fn publish(&self, width: u32, height: u32, data: Vec<u8>) {
        let mut mb = self.lock();
        if let Some(old) = mb.frame.take() {
            mb.spare = Some(old.data);
        }
        mb.seq += 1;
        let seq = mb.seq;
        mb.frame = Some(CapturedFrame {
            seq,
            width,
            height,
            data,
        });
        drop(mb);
        self.consumed.notify_all();
    }

    fn has_pending_error(&self) -> bool {
        self.lock().error.is_some()
    }

    /// Publish the opened dimensions; wakes `wait_ready`.
    fn mark_ready(&self, width: u32, height: u32, gdi: bool) {
        self.width.store(width, Ordering::Relaxed);
        self.height.store(height, Ordering::Relaxed);
        self.is_gdi.store(gdi, Ordering::Relaxed);
        let mut mb = self.lock();
        mb.ready = true;
        drop(mb);
        self.consumed.notify_all();
    }

    /// Capture-thread open failure; wakes `wait_ready` with an error.
    fn fail_open(&self, reason: String) {
        let mut mb = self.lock();
        mb.open_error = Some(reason);
        drop(mb);
        self.consumed.notify_all();
    }
}

/// Handle to the capture thread.
///
/// #65: `CaptureSource` owns raw COM pointers (not `Send`), so the capturer is
/// **opened inside the thread** and never crosses a thread boundary. The outer
/// scheduler spawns a worker, waits for `ready`, opens the encoder for the
/// published dimensions, and drops the worker to rebuild.
pub struct CaptureWorker {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

impl CaptureWorker {
    /// Spawn the capture thread; it opens the primary display itself.
    pub fn spawn() -> Self {
        let shared = Arc::new(Shared {
            mailbox: Mutex::new(Mailbox {
                frame: None,
                spare: None,
                error: None,
                seq: 0,
                ready: false,
                open_error: None,
            }),
            consumed: Condvar::new(),
            stop: AtomicBool::new(false),
            gdi_request: AtomicBool::new(false),
            is_gdi: AtomicBool::new(false),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
        });
        let worker_shared = shared.clone();
        let join = std::thread::Builder::new()
            .name("capture".to_owned())
            .spawn(move || run(worker_shared))
            .ok();
        if join.is_none() {
            log::error!("failed to spawn the capture thread");
        }
        Self { shared, join }
    }

    /// Block until the capturer is open (or the open failed). The outer
    /// scheduler needs the dimensions before it can open the encoder.
    pub fn wait_ready(&self, timeout: Duration) -> Result<(u32, u32), String> {
        let deadline = std::time::Instant::now() + timeout;
        let mut mb = self.shared.lock();
        loop {
            if let Some(err) = mb.open_error.take() {
                return Err(err);
            }
            if mb.ready {
                return Ok((
                    self.shared.width.load(Ordering::Relaxed),
                    self.shared.height.load(Ordering::Relaxed),
                ));
            }
            if self.shared.stop.load(Ordering::Relaxed) {
                return Err("capture worker stopped before open".to_owned());
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err("capture open timed out".to_owned());
            }
            let (guard, _) = self
                .shared
                .consumed
                .wait_timeout(mb, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            mb = guard;
        }
    }

    /// Take the newest frame, or `None` when the desktop produced nothing since
    /// the last beat. Never blocks on the capturer.
    pub fn take(&self) -> Option<CapturedFrame> {
        let mut mb = self.shared.lock();
        let frame = mb.frame.take();
        if frame.is_some() {
            drop(mb);
            self.shared.consumed.notify_all();
        }
        frame
    }

    /// Hand the consumed buffer back so the worker refills it in place.
    pub fn give_back(&self, data: Vec<u8>) {
        let mut mb = self.shared.lock();
        if mb.spare.is_none() {
            mb.spare = Some(data);
        }
    }

    /// First unconsumed capture error (the beat loop runs the recovery chain).
    pub fn take_error(&self) -> Option<(io::ErrorKind, String)> {
        let mut mb = self.shared.lock();
        let err = mb.error.take();
        if err.is_some() {
            drop(mb);
            self.shared.consumed.notify_all();
        }
        err
    }

    /// Ask the worker to mount the bundled GDI capturer (recovery chain).
    pub fn request_gdi(&self) {
        self.shared.gdi_request.store(true, Ordering::Relaxed);
    }

    pub fn is_gdi(&self) -> bool {
        self.shared.is_gdi.load(Ordering::Relaxed)
    }

    /// Native pixels of the opened display (set by the capture thread).
    pub fn dimensions(&self) -> (u32, u32) {
        (
            self.shared.width.load(Ordering::Relaxed),
            self.shared.height.load(Ordering::Relaxed),
        )
    }

    /// Signal and join the capture thread.
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.consumed.notify_all();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for CaptureWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Capture thread body: open the display, then block in `frame()`, publish the
/// newest pixels, wait for the beat to consume, repeat.
fn run(shared: Arc<Shared>) {
    let mut source = match CaptureSource::open_primary() {
        Ok(source) => source,
        Err(err) => {
            log::warn!("capture thread: open failed: {err:#}");
            shared.fail_open(format!("{err:#}"));
            return;
        }
    };
    shared.mark_ready(source.width, source.height, source.is_gdi());
    log::info!("capture worker started (poll {CAPTURE_POLL:?})");

    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        if shared.gdi_request.swap(false, Ordering::Relaxed) {
            let gdi = source.set_gdi();
            shared.is_gdi.store(gdi, Ordering::Relaxed);
            log::info!("capture worker switched backend: gdi={gdi}");
        }
        // Hold off while the beat loop still owes us a recovery decision; polling
        // a broken duplication object would only queue more errors.
        if shared.has_pending_error() {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }

        match source.frame(CAPTURE_POLL) {
            Ok(frame) => {
                let (w, h, data) = match &frame {
                    Frame::PixelBuffer(pb) => (pb.width() as u32, pb.height() as u32, pb.data()),
                    _ => continue,
                };
                let need = w as usize * h as usize * 4;
                if need == 0 || data.len() < need {
                    continue;
                }
                let mut buf = shared.lock().spare.take().unwrap_or_default();
                buf.resize(need, 0);
                buf.copy_from_slice(&data[..need]);
                // End the capturer borrow before touching the mailbox again.
                drop(frame);
                shared.publish(w, h, buf);
                // One outstanding frame at a time: wait for the beat to consume.
                let mut mb = shared.lock();
                while mb.frame.is_some()
                    && !shared.stop.load(Ordering::Relaxed)
                    && mb.error.is_none()
                {
                    let (guard, _) = shared
                        .consumed
                        .wait_timeout(mb, CAPTURE_POLL)
                        .unwrap_or_else(|p| p.into_inner());
                    mb = guard;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => {
                // Publish once; the beat loop decides rebuild vs GDI fallback.
                let mut mb = shared.lock();
                if mb.error.is_none() {
                    mb.error = Some((e.kind(), format!("{e}")));
                }
            }
        }
    }
    log::info!("capture worker stopped");
}

