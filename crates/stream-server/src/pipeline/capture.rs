//! Display enumeration and frame acquisition (design 8.1 row 1; stage 4.3).
//!
//! Stage1 boundary: fix on `is_primary()` for the only screen, no multi-monitor
//! polling. #42-1 adds the RustDesk-aligned backend switch (`set_gdi`) used by
//! the DXGI finite-recovery chain; a fatal open failure still aborts after the
//! outer recovery backoff budget is exhausted (design stage1-capture-recovery.md
//! section 4.3).
//!
//! Assembly mirrors the 0e bench chain (`crates/bench-capture/src/main.rs:55-74`):
//! `Display::all()` -> prefer `is_primary()` -> `Capturer::new(display)`. The
//! returned `Frame<'a>` borrows the capturer for the duration of the beat; the
//! loop converts it to YUV immediately and never stores the frame between beats.

use std::io;
use std::time::Duration;

use scrap::{Capturer, Display, Frame, TraitCapturer};

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
