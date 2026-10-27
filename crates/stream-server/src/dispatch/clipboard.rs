//! #55 Windows clipboard bridge (`ClipboardSink`).
//!
//! Two directions, both over the control data channel:
//! * Downlink — the frontend sends `REQUEST_CLIPBOARD` (Ctrl/Cmd+C in the
//!   browser, `selkies-wr-core.js:536`); we read the controlled host clipboard
//!   and reply with the `clipboardContentUpdate` JSON that
//!   `lib/clipboard-sync.js` (`clipboardPreviewMessage`) consumes.
//! * Uplink — the browser's Ctrl/Cmd+V sends the local clipboard as text
//!   chunks (`lib/clipboard-worker-bridge.js`): single `cw,<b64>`, or
//!   `cws,<tid>,<total>` → `cwd,<tid>,<b64>`… → `cwe,<tid>`; we reassemble and
//!   write it into the controlled host clipboard. Binary flavours
//!   (`cb/cbs/cbd/cbe`) are logged and dropped in stage 1 (text-only sync).

use std::sync::Mutex;

use windows_sys::Win32::Foundation::FALSE;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

/// `CF_UNICODETEXT` — winuser.h `#define CF_UNICODETEXT 0x000D`; windows-sys
/// 0.52 does not re-export the clipboard-format constants.
const CF_UNICODETEXT: u32 = 13;

/// Reassembly state for a multi-chunk uplink transfer.
#[derive(Default)]
struct ChunkState {
    tid: String,
    total: u64,
    buf: Vec<u8>,
}

/// Text-only clipboard sink for the controlled host.
pub struct ClipboardSink {
    chunk: Mutex<ChunkState>,
}

impl Default for ClipboardSink {
    fn default() -> Self {
        Self {
            chunk: Mutex::new(ChunkState::default()),
        }
    }
}

/// Read the host clipboard as UTF-16 text. `None` when the clipboard is
/// closed by another app or holds no CF_UNICODETEXT flavour.
fn read_text() -> Option<String> {
    unsafe {
        // OpenClipboard takes an owner HWND; windows-sys 0.52 models HWND as
        // `isize`, so 0 is the NULL owner.
        if OpenClipboard(0) == FALSE {
            return None;
        }
        let handle = GetClipboardData(CF_UNICODETEXT);
        if handle == 0 {
            CloseClipboard();
            return None;
        }
        let ptr = GlobalLock(handle as *mut std::ffi::c_void) as *const u16;
        if ptr.is_null() {
            CloseClipboard();
            return None;
        }
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
        GlobalUnlock(handle as *mut std::ffi::c_void);
        CloseClipboard();
        Some(text)
    }
}

/// Write UTF-16 text into the host clipboard. The handle handed to
/// `SetClipboardData` becomes the system's; it must not be freed here.
fn write_text(text: &str) -> bool {
    unsafe {
        if OpenClipboard(0) == FALSE {
            return false;
        }
        let _ = EmptyClipboard();
        let units: Vec<u16> = text.encode_utf16().collect();
        let size = (units.len() + 1) * std::mem::size_of::<u16>();
        let handle = GlobalAlloc(GMEM_MOVEABLE, size);
        if handle.is_null() {
            CloseClipboard();
            return false;
        }
        let dst = GlobalLock(handle) as *mut u16;
        if dst.is_null() {
            CloseClipboard();
            return false;
        }
        std::ptr::copy_nonoverlapping(units.as_ptr(), dst, units.len());
        *dst.add(units.len()) = 0; // terminator
        GlobalUnlock(handle);
        let written = SetClipboardData(CF_UNICODETEXT, handle as isize) != 0;
        CloseClipboard();
        written
    }
}

/// The `clipboardContentUpdate` JSON payload the frontend's
/// `createIncomingClipboard` digests (clipboard-sync.js:1086).
pub fn clipboard_content_update(text: &str) -> String {
    let total = text.chars().count() as u64;
    let limit: u64 = 256 * 1024; // CLIPBOARD_PREVIEW_LIMIT
    let truncated = total > limit;
    let shown: String = if truncated {
        text.chars().take(limit as usize).collect()
    } else {
        text.to_owned()
    };
    let payload = hbb_common::serde_json::json!({
        "type": "clipboardContentUpdate",
        "text": shown,
        "truncated": truncated,
        "totalLength": total,
        "secret": false,
    });
    payload.to_string()
}

impl ClipboardSink {
    /// Parse one uplink line. Text chunks accumulate until `cwe` lands, then
    /// the assembled payload is written to the host clipboard.
    pub fn handle_line(&self, line: &str) {
        let (head, rest) = match line.split_once(',') {
            Some((h, r)) => (h, r),
            None => (line, ""),
        };
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;

        let decode = |b64: &str| -> Option<Vec<u8>> {
            STANDARD.decode(b64).ok().filter(|v| !v.is_empty())
        };

        match head {
            // Single-chunk text transfer (`cw,<base64>`).
            "cw" => {
                if let Some(bytes) = decode(rest) {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    log::info!("clipboard uplink (single chunk, {} bytes)", bytes.len());
                    write_text(&text);
                } else {
                    log::warn!("clipboard cw chunk failed to decode");
                }
            }
            // Multipart text transfer start.
            "cws" => {
                let mut f = rest.splitn(2, ',');
                let tid = f.next().unwrap_or("").to_owned();
                let total = f.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                let mut st = self.chunk.lock().unwrap_or_else(|p| p.into_inner());
                st.tid = tid;
                st.total = total;
                st.buf.clear();
                log::info!("clipboard uplink start: tid={} total={}", st.tid, total);
            }
            // Multipart text chunk.
            "cwd" => {
                let mut f = rest.splitn(2, ',');
                let tid = f.next().unwrap_or("");
                let b64 = f.next().unwrap_or("");
                let mut st = self.chunk.lock().unwrap_or_else(|p| p.into_inner());
                if st.tid != tid {
                    log::warn!("clipboard cwd tid mismatch (got {tid}, want {}); reset", st.tid);
                    st.tid = tid.to_owned();
                    st.buf.clear();
                }
                if let Some(bytes) = decode(b64) {
                    st.buf.extend_from_slice(&bytes);
                }
            }
            // Multipart text end: assemble and write.
            "cwe" => {
                let tid = rest;
                let mut st = self.chunk.lock().unwrap_or_else(|p| p.into_inner());
                if st.tid != tid {
                    log::warn!("clipboard cwe tid mismatch (got {tid}, want {}); reset", st.tid);
                    st.buf.clear();
                    st.tid.clear();
                    return;
                }
                let got = st.buf.len() as u64;
                log::info!(
                    "clipboard uplink complete: tid={} got={} expected={}",
                    tid,
                    got,
                    st.total
                );
                if got == 0 || (st.total > 0 && got != st.total) {
                    log::warn!("clipboard uplink size mismatch; drop");
                    st.buf.clear();
                    st.tid.clear();
                    return;
                }
                let text = String::from_utf8_lossy(&st.buf).into_owned();
                st.buf.clear();
                st.tid.clear();
                write_text(&text);
            }
            // Binary flavours: logged and dropped in stage 1.
            "cb" | "cbs" | "cbd" | "cbe" => {
                log::debug!("clipboard binary flavour ignored in stage1: {head}");
            }
            _ => {
                log::debug!("unhandled clipboard line: {line}");
            }
        }
    }
}

/// Read the host clipboard now and return the downlink JSON (empty text when
/// the clipboard is not readable).
pub fn read_clipboard_update() -> String {
    match read_text() {
        Some(text) => clipboard_content_update(&text),
        None => {
            log::debug!("REQUEST_CLIPBOARD: clipboard not readable");
            clipboard_content_update("")
        }
    }
}
