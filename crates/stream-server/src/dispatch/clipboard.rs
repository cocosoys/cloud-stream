//! #55 / #85 Windows clipboard bridge (`ClipboardSink`).
//!
//! Two directions, both over the control data channel:
//!
//! * **Downlink** — the frontend sends `REQUEST_CLIPBOARD` (Ctrl/Cmd+C in the
//!   browser, `selkies-wr-core.js:536`); we read the controlled host clipboard
//!   and answer with the `clipboard-msg*` family that the page's
//!   `incomingClipboard` consumes (`selkies-wr-core.js:3470`):
//!   `clipboard-msg` for one payload, `clipboard-msg-start/-data/-end` for
//!   anything that does not fit one data-channel message.
//! * **Uplink** — the browser's Ctrl/Cmd+V sends its clipboard as chunks
//!   (`lib/clipboard-worker-bridge.js`): text as `cw,<b64>` or
//!   `cws/cwd/cwe` multiparts, images as `cb,<mime>,<b64>` or
//!   `cbs/cbd/cbe`. Text lands as `CF_UNICODETEXT`, an image is decoded
//!   (see `clipboard_image`) and lands as `CF_DIB`.
//!
//! Two things about this file that are easy to get wrong:
//!
//! 1. The old downlink used `clipboardContentUpdate`, which only the
//!    WebSocket-flavoured core consumes. This page handles the `clipboard-msg*`
//!    family and logs "Unknown clipboard cmd received" for anything else — so
//!    the text path was silently dead here as well, not just the binary one.
//! 2. `CF_DIB` is not a BMP *file*: it has no `BITMAPFILEHEADER`. The browser
//!    needs a file, so the downlink wraps it (`clipboard_image::dib_to_bmp`);
//!    the uplink goes the other way and drops the 14 bytes.

use std::sync::Mutex;

use windows_sys::Win32::Foundation::FALSE;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

use super::clipboard_image;

/// `CF_UNICODETEXT` — winuser.h `#define CF_UNICODETEXT 0x000D`; windows-sys
/// 0.52 does not re-export the clipboard-format constants.
const CF_UNICODETEXT: u32 = 13;
/// `CF_DIB` — `#define CF_DIB 8`. Format 8 is a `BITMAPINFOHEADER` followed by
/// the pixel rows, bottom-up.
const CF_DIB: u32 = 8;

/// Base64 per outbound chunk. A data-channel message is capped at 64 KiB once
/// SCTP framing is accounted for, and base64 costs a third, so 36 KiB of raw
/// bytes (48 KiB of text) plus the JSON envelope stays inside it.
const DOWNLINK_CHUNK_RAW: usize = 36 * 1024;

/// Reassembly state for one multipart uplink transfer.
#[derive(Default)]
struct ChunkState {
    tid: String,
    mime: String,
    total: u64,
    buf: Vec<u8>,
}

/// Clipboard sink for the controlled host.
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

/// Read the host clipboard as a raw `CF_DIB` blob (header + pixels, no file
/// header). `None` when there is no bitmap on the clipboard.
fn read_dib() -> Option<Vec<u8>> {
    unsafe {
        if OpenClipboard(0) == FALSE {
            return None;
        }
        let handle = GetClipboardData(CF_DIB);
        if handle == 0 {
            CloseClipboard();
            return None;
        }
        let size = GlobalSize(handle as *mut std::ffi::c_void);
        let ptr = GlobalLock(handle as *mut std::ffi::c_void) as *const u8;
        if ptr.is_null() || size < 40 {
            GlobalUnlock(handle as *mut std::ffi::c_void);
            CloseClipboard();
            return None;
        }
        // `GlobalSize` is the allocation size and can exceed the DIB (the heap
        // rounds up); the header's own geometry says where the pixels end.
        let all = std::slice::from_raw_parts(ptr, size);
        let len = clipboard_image::dib_expected_len(all)
            .unwrap_or(size)
            .min(size);
        let dib = all[..len].to_vec();
        GlobalUnlock(handle as *mut std::ffi::c_void);
        CloseClipboard();
        Some(dib)
    }
}

/// Copy `bytes` into a movable global block. The handle becomes the system's on
/// a successful `SetClipboardData`, so it must not be freed here.
unsafe fn set_clipboard_bytes(format: u32, bytes: &[u8]) -> bool {
    if OpenClipboard(0) == FALSE {
        return false;
    }
    let _ = EmptyClipboard();
    let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
    if handle.is_null() {
        CloseClipboard();
        return false;
    }
    let dst = GlobalLock(handle) as *mut u8;
    if dst.is_null() {
        CloseClipboard();
        return false;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
    GlobalUnlock(handle);
    let written = SetClipboardData(format, handle as isize) != 0;
    CloseClipboard();
    written
}

/// Write UTF-16 text into the host clipboard.
fn write_text(text: &str) -> bool {
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0); // terminator
    let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
    unsafe { set_clipboard_bytes(CF_UNICODETEXT, &bytes) }
}

/// Write a raster as `CF_DIB` (the flavour Paint, Word and Explorer paste).
fn write_dib(dib: &[u8]) -> bool {
    unsafe { set_clipboard_bytes(CF_DIB, dib) }
}

/// Base64 of a byte slice, the one encoding the whole clipboard protocol uses.
fn b64(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    STANDARD.encode(bytes)
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    STANDARD.decode(s).ok().filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Downlink: host clipboard -> `clipboard-msg*`
// ---------------------------------------------------------------------------

fn clipboard_msg_single(mime: &str, content: &str) -> String {
    hbb_common::serde_json::json!({
        "type": "clipboard-msg",
        "data": {
            "mime_type": mime,
            "content": content,
            "secret": false,
        }
    })
    .to_string()
}

fn clipboard_msg_start(mime: &str, total: usize) -> String {
    hbb_common::serde_json::json!({
        "type": "clipboard-msg-start",
        "data": {
            "mime_type": mime,
            "total_size": total,
            "secret": false,
        }
    })
    .to_string()
}

fn clipboard_msg_data(content: &str) -> String {
    hbb_common::serde_json::json!({
        "type": "clipboard-msg-data",
        "data": { "content": content }
    })
    .to_string()
}

fn clipboard_msg_end() -> String {
    hbb_common::serde_json::json!({
        "type": "clipboard-msg-end",
        "data": {}
    })
    .to_string()
}

/// Encode one payload as a single message or a multipart sequence.
fn clipboard_messages(mime: &str, bytes: &[u8]) -> Vec<String> {
    if bytes.len() < DOWNLINK_CHUNK_RAW {
        return vec![clipboard_msg_single(mime, &b64(bytes))];
    }
    let mut out = vec![clipboard_msg_start(mime, bytes.len())];
    for chunk in bytes.chunks(DOWNLINK_CHUNK_RAW) {
        out.push(clipboard_msg_data(&b64(chunk)));
    }
    out.push(clipboard_msg_end());
    out
}

/// Read the host clipboard and return the messages to send.
///
/// Text wins when the clipboard carries both: copying from Word or Excel puts
/// rich content plus `CF_DIB` on the clipboard, and pasting the picture when the
/// user copied a table is the wrong half of that.
pub fn read_clipboard_update() -> Vec<String> {
    if let Some(text) = read_text().filter(|t| !t.is_empty()) {
        return clipboard_messages("text/plain", text.as_bytes());
    }
    if let Some(dib) = read_dib() {
        match clipboard_image::dib_to_bmp(&dib) {
            Some(bmp) => {
                log::info!(
                    "REQUEST_CLIPBOARD: image answered ({} bytes DIB -> {} bytes BMP)",
                    dib.len(),
                    bmp.len()
                );
                return clipboard_messages("image/bmp", &bmp);
            }
            None => log::warn!(
                "REQUEST_CLIPBOARD: CF_DIB present but not convertible ({} bytes)",
                dib.len()
            ),
        }
    }
    log::debug!("REQUEST_CLIPBOARD: clipboard holds no text or bitmap");
    // An empty text payload is the "nothing to paste" answer the page handles.
    clipboard_messages("text/plain", b"")
}

impl ClipboardSink {
    /// Parse one uplink line. Chunks accumulate until the transfer's end message
    /// lands, then the assembled payload is written to the host clipboard.
    pub fn handle_line(&self, line: &str) {
        let (head, rest) = match line.split_once(',') {
            Some((h, r)) => (h, r),
            None => (line, ""),
        };

        match head {
            // ---- text, single chunk (`cw,<base64>`) ----
            "cw" => match b64_decode(rest) {
                Some(bytes) => {
                    log::info!("clipboard uplink (text, single chunk, {} bytes)", bytes.len());
                    write_text(&String::from_utf8_lossy(&bytes));
                }
                None => log::warn!("clipboard cw chunk failed to decode"),
            },
            // ---- text, multipart start (`cws,<tid>,<total>`) ----
            "cws" => {
                let mut f = rest.splitn(2, ',');
                let tid = f.next().unwrap_or("").to_owned();
                let total = f.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                let mut st = self.chunk.lock().unwrap_or_else(|p| p.into_inner());
                st.tid = tid;
                st.mime = "text/plain".to_owned();
                st.total = total;
                st.buf.clear();
                log::info!("clipboard uplink start: text tid={} total={}", st.tid, total);
            }
            // ---- binary, single chunk (`cb,<mime>,<base64>`) ----
            "cb" => {
                let (mime, payload) = match rest.split_once(',') {
                    Some(v) => v,
                    None => {
                        log::warn!("clipboard cb without a mime type");
                        return;
                    }
                };
                match b64_decode(payload) {
                    Some(bytes) => {
                        log::info!(
                            "clipboard uplink ({mime}, single chunk, {} bytes)",
                            bytes.len()
                        );
                        write_binary(mime, &bytes);
                    }
                    None => log::warn!("clipboard cb chunk failed to decode"),
                }
            }
            // ---- binary, multipart start (`cbs,<tid>,<mime>,<total>`) ----
            "cbs" => {
                let mut f = rest.splitn(3, ',');
                let tid = f.next().unwrap_or("").to_owned();
                let mime = f.next().unwrap_or("application/octet-stream").to_owned();
                let total = f.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                let mut st = self.chunk.lock().unwrap_or_else(|p| p.into_inner());
                st.tid = tid;
                st.mime = mime.clone();
                st.total = total;
                st.buf.clear();
                log::info!("clipboard uplink start: {mime} tid={} total={}", st.tid, total);
            }
            // ---- multipart data, both flavours (`cwd` / `cbd`) ----
            "cwd" | "cbd" => {
                let mut f = rest.splitn(2, ',');
                let tid = f.next().unwrap_or("");
                let payload = f.next().unwrap_or("");
                let mut st = self.chunk.lock().unwrap_or_else(|p| p.into_inner());
                if st.tid != tid {
                    log::warn!("clipboard {head} tid mismatch (got {tid}, want {}); reset", st.tid);
                    st.tid = tid.to_owned();
                    st.buf.clear();
                }
                match b64_decode(payload) {
                    Some(bytes) => st.buf.extend_from_slice(&bytes),
                    None => log::warn!("clipboard {head} chunk failed to decode"),
                }
            }
            // ---- multipart end (`cwe` / `cbe`) ----
            "cwe" | "cbe" => {
                let tid = rest;
                let mut st = self.chunk.lock().unwrap_or_else(|p| p.into_inner());
                if st.tid != tid {
                    log::warn!("clipboard {head} tid mismatch (got {tid}, want {}); reset", st.tid);
                    st.buf.clear();
                    st.tid.clear();
                    return;
                }
                let got = st.buf.len() as u64;
                let mime = st.mime.clone();
                log::info!(
                    "clipboard uplink complete: {mime} tid={tid} got={got} expected={}",
                    st.total
                );
                if got == 0 || (st.total > 0 && got != st.total) {
                    log::warn!("clipboard uplink size mismatch; drop");
                    st.buf.clear();
                    st.tid.clear();
                    return;
                }
                let bytes = std::mem::take(&mut st.buf);
                st.tid.clear();
                st.mime.clear();
                drop(st);
                write_binary(&mime, &bytes);
            }
            _ => {
                log::debug!("unhandled clipboard line: {line}");
            }
        }
    }
}

/// Land one uplink payload on the host clipboard according to its MIME type.
///
/// Text goes straight in; an image is decoded to pixels and written as
/// `CF_DIB`. A format the decoder refuses (16-bit PNG, JPEG, WebP) is logged and
/// dropped rather than written as raw bytes — a `CF_DIB` header full of JPEG
/// data would paste as garbage, which is worse than a paste that does nothing.
fn write_binary(mime: &str, bytes: &[u8]) {
    if mime == "text/plain" || mime.starts_with("text/") {
        write_text(&String::from_utf8_lossy(bytes));
        return;
    }
    match clipboard_image::decode(bytes) {
        Ok(img) => {
            let dib = clipboard_image::to_dib(&img);
            log::info!(
                "clipboard uplink: {mime} {}x{} -> CF_DIB {} bytes",
                img.width,
                img.height,
                dib.len()
            );
            if !write_dib(&dib) {
                log::warn!("clipboard uplink: SetClipboardData(CF_DIB) failed");
            }
        }
        Err(err) => log::warn!("clipboard uplink: {mime} not convertible ({err:#}); dropped"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The page only understands `clipboard-msg*`; the old
    /// `clipboardContentUpdate` fell through to "Unknown clipboard cmd".
    #[test]
    fn text_downlink_uses_the_message_family_the_page_reads() {
        let msgs = clipboard_messages("text/plain", b"hello");
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("\"type\":\"clipboard-msg\""));
        assert!(msgs[0].contains("\"mime_type\":\"text/plain\""));
        // "hello" in base64.
        assert!(msgs[0].contains("aGVsbG8="));
        assert!(!msgs[0].contains("clipboardContentUpdate"));
    }

    /// Anything that does not fit one message must be framed, and the declared
    /// `total_size` is compared against the *raw* byte count by the page.
    #[test]
    fn large_payloads_are_framed_with_a_raw_byte_total() {
        let raw = vec![7u8; DOWNLINK_CHUNK_RAW * 2 + 5];
        let msgs = clipboard_messages("image/bmp", &raw);
        assert!(msgs[0].contains("\"type\":\"clipboard-msg-start\""));
        assert!(msgs[0].contains(&format!("\"total_size\":{}", raw.len())));
        assert_eq!(msgs.last().unwrap().contains("clipboard-msg-end"), true);
        // Three chunks: full, full, remainder.
        assert_eq!(msgs.len(), 1 + 3 + 1);
    }

    /// A payload exactly at the boundary must not be sent as a single message
    /// that overflows the data channel.
    #[test]
    fn boundary_sized_payload_is_framed_too() {
        let raw = vec![0u8; DOWNLINK_CHUNK_RAW];
        let msgs = clipboard_messages("image/bmp", &raw);
        assert!(msgs[0].contains("clipboard-msg-start"));
    }
}
