//! Remote cursor: capture (C1) and cursor-image publishing (C2) (#51).
//!
//! C1 — the capture/encode worker paints the live OS cursor into the frame
//! before the YUV conversion, following the RustDesk pattern
//! (`src/platform/windows.rs:202-329`): `GetCursorInfo` for the handle and
//! show/hide state, `GetIconInfo` + `GetBitmapBits` for the 32bpp BGRA bitmap
//! and hotspot, alpha-composited by `draw_cursor`. Modern Windows cursors carry
//! an alpha channel in `hbmColor`, so the classic AND/XOR mask path is skipped
//! (single-color hardware cursors degrade to a no-op; a debug log notes them).
//!
//! C2 — the same worker emits `CursorData` through a `PipelineEvent` when the
//! cursor *identity* changes; the session turns it into the selkies
//! `cursor` message. The frontend expects `curdata` as a **base64 PNG**
//! (`lib/input.js:1902 updateServerCursor`), so this module ships a small
//! dependency-lean PNG encoder (flate2 zlib + hand-rolled CRC32, no new codec
//! crates).
//!
//! Thread boundary: all of this is called from the blocking capture worker
//! (`run_inner_beat`); no `await` crosses it.

use std::io::Write;

use windows_sys::Win32::Foundation::{FALSE, POINT};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, SelectObject, BITMAP,
    BITMAPV5HEADER, BI_BITFIELDS, DIB_RGB_COLORS,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetCursorPos, GetIconInfo, GetSystemMetrics, CURSORINFO, CURSOR_SHOWING,
    ICONINFO, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// One captured cursor: top-down BGRA pixels plus the hotspot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorData {
    /// `hCursor` handle value; 0 means "hidden / none".
    pub id: u64,
    /// BGRA, top-down, `width * height * 4` bytes. Alpha carries opacity.
    pub colors: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub hotx: i32,
    pub hoty: i32,
}

/// Query the primary-screen cursor: `Ok(Some(hCursor))` when visible,
/// `Ok(None)` when hidden, `Err` on API failure.
pub fn cursor_info() -> anyhow::Result<Option<u64>> {
    unsafe {
        let mut ci: CURSORINFO = std::mem::zeroed();
        ci.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
        if GetCursorInfo(&mut ci) == FALSE {
            return Err(anyhow::anyhow!("GetCursorInfo failed: {}", last_os_error()));
        }
        if ci.flags & CURSOR_SHOWING == 0 {
            Ok(None)
        } else {
            Ok(Some(ci.hCursor as u64))
        }
    }
}

/// Virtual-screen origin (primary display is assumed at that origin; the
/// primary is what scrap captures). Used to translate the OS cursor
/// coordinates into frame coordinates.
pub fn virtual_origin() -> (i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    }
}

/// Current cursor screen position (`POINT`).
pub fn cursor_pos() -> (i32, i32) {
    unsafe {
        let mut pt: POINT = std::mem::zeroed();
        if GetCursorPos(&mut pt) == FALSE {
            (0, 0)
        } else {
            (pt.x, pt.y)
        }
    }
}

/// Capture the cursor bitmap for `hCursor` (RustDesk `get_cursor_data` subset).
/// Returns `None` for single-color cursors (no `hbmColor`): those are rare on
/// modern Windows and skipped in stage 1.
pub fn cursor_data(hcursor: u64) -> anyhow::Result<Option<CursorData>> {
    unsafe {
        let mut ii: ICONINFO = std::mem::zeroed();
        if GetIconInfo(hcursor as _, &mut ii) == FALSE {
            return Err(anyhow::anyhow!("GetIconInfo failed: {}", last_os_error()));
        }
        // ICONINFO owns the two handles; release them before returning. The
        // bitmap bytes are copied out first.
        let result = (|| -> anyhow::Result<Option<CursorData>> {
            if ii.hbmColor == 0 {
                log::debug!("cursor {hcursor:#x}: single-color, skipped (no hbmColor)");
                return Ok(None);
            }
            let mut bm: BITMAP = std::mem::zeroed();
            let got = GetObjectW(
                ii.hbmColor as _,
                std::mem::size_of::<BITMAP>() as i32,
                &mut bm as *mut _ as *mut _,
            );
            if got == 0 {
                return Err(anyhow::anyhow!("GetObjectW(hbmColor) failed: {}", last_os_error()));
            }
            let w = bm.bmWidth as usize;
            let h = bm.bmHeight as usize;
            if w == 0 || h == 0 || w > 256 || h > 256 {
                log::debug!("cursor {hcursor:#x}: unusable size {w}x{h}");
                return Ok(None);
            }
            // Read the colour plane with GetDIBits in **top-down** order
            // (biHeight negative), exactly like RustDesk
            // (`src/platform/windows.cc:488-506`). `GetBitmapBits` on a modern
            // cursor's 32bpp DIB section returns rows in the DIB's own order
            // (already top-down), so naively flipping again produced an
            // upside-down pointer. With the BGRA masks below the output lands
            // byte-for-byte in our BGRA format — no post-shuffle needed.
            let mut colors = vec![0u8; w * h * 4];
            let dc = CreateCompatibleDC(0);
            if dc == 0 {
                return Err(anyhow::anyhow!("CreateCompatibleDC failed: {}", last_os_error()));
            }
            let old = SelectObject(dc, ii.hbmColor as _);
            let mut bi: BITMAPV5HEADER = std::mem::zeroed();
            bi.bV5Size = std::mem::size_of::<BITMAPV5HEADER>() as u32;
            bi.bV5Width = w as i32;
            bi.bV5Height = -(h as i32); // negative => top-down
            bi.bV5Planes = 1;
            bi.bV5BitCount = 32;
            bi.bV5Compression = BI_BITFIELDS;
            bi.bV5RedMask = 0x00FF_0000;
            bi.bV5GreenMask = 0x0000_FF00;
            bi.bV5BlueMask = 0x0000_00FF;
            bi.bV5AlphaMask = 0xFF00_0000;
            let got = GetDIBits(
                dc,
                ii.hbmColor as _,
                0,
                h as u32,
                colors.as_mut_ptr() as _,
                &mut bi as *mut _ as *mut _,
                DIB_RGB_COLORS,
            );
            if old != 0 {
                SelectObject(dc, old);
            }
            DeleteDC(dc);
            if got == 0 || got != h as i32 {
                return Err(anyhow::anyhow!(
                    "GetDIBits failed/short: {got} != {h} ({})",
                    last_os_error()
                ));
            }
            Ok(Some(CursorData {
                id: hcursor,
                colors,
                width: w,
                height: h,
                hotx: ii.xHotspot as i32,
                hoty: ii.yHotspot as i32,
            }))
        })();

        if ii.hbmColor != 0 {
            DeleteObject(ii.hbmColor as _);
        }
        if ii.hbmMask != 0 {
            DeleteObject(ii.hbmMask as _);
        }
        result
    }
}

/// Alpha-composite `cur` into a BGRA `dst` at `(x, y)` (top-left with hotspot
/// already applied by the caller). Out-of-bounds pixels are clipped.
pub fn draw_cursor(
    dst: &mut [u8],
    dst_stride_bytes: usize,
    dst_w: i32,
    dst_h: i32,
    x: i32,
    y: i32,
    cur: &CursorData,
) {
    let cw = cur.width as i32;
    let ch = cur.height as i32;
    for cy in 0..ch {
        let dy = y + cy;
        if dy < 0 || dy >= dst_h {
            continue;
        }
        for cx in 0..cw {
            let dx = x + cx;
            if dx < 0 || dx >= dst_w {
                continue;
            }
            let si = ((cy as usize * cur.width) + cx as usize) * 4;
            let a = cur.colors[si + 3] as u32;
            if a == 0 {
                continue;
            }
            let di = (dy as usize * dst_stride_bytes) + dx as usize * 4;
            let inv = 255 - a;
            dst[di] = ((cur.colors[si] as u32 * a + dst[di] as u32 * inv) / 255) as u8;
            dst[di + 1] = ((cur.colors[si + 1] as u32 * a + dst[di + 1] as u32 * inv) / 255) as u8;
            dst[di + 2] = ((cur.colors[si + 2] as u32 * a + dst[di + 2] as u32 * inv) / 255) as u8;
            // Alpha of the composed pixel is not needed downstream (YUV skips
            // it), keep dst alpha untouched.
        }
    }
}

// ---- C2: minimal PNG encoder (RGBA, 8-bit, filter 0) ----

/// Encode `cur.colors` (top-down BGRA) as a base64 PNG for the frontend.
/// Dependency-lean: `flate2` zlib + hand-rolled CRC32 + base64.
pub fn cursor_png_b64(cur: &CursorData) -> String {
    let row_bytes = cur.width * 4;
    let mut raw = Vec::with_capacity((row_bytes + 1) * cur.height);
    for y in 0..cur.height {
        raw.push(0); // filter: None
        raw.extend_from_slice(&cur.colors[y * row_bytes..(y + 1) * row_bytes]);
    }

    let mut idat = Vec::new();
    {
        let mut enc =
            flate2::write::ZlibEncoder::new(&mut idat, flate2::Compression::default());
        let _ = enc.write_all(&raw);
        let _ = enc.finish();
    }

    let mut png = Vec::with_capacity(64 + idat.len());
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    // IHDR
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(cur.width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(cur.height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, adaptive, no interlace
    push_chunk(&mut png, b"IHDR", &ihdr);
    // IDAT
    push_chunk(&mut png, b"IDAT", &idat);
    // IEND
    push_chunk(&mut png, b"IEND", &[]);

    base64_encode(&png)
}

fn push_chunk(png: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    png.extend_from_slice(tag);
    png.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(tag);
    crc_input.extend_from_slice(data);
    png.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// CRC-32 (ISO 3309 / PNG), reflected table-less bitwise.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Standard base64 (no padding variants), tiny and good enough for <1 KB cursors.
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(T[n as usize & 63] as char);
        }
    }
    out
}

fn last_os_error() -> String {
    std::io::Error::last_os_error().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_roundtrip_has_signature_and_ihdr() {
        let cur = CursorData {
            id: 1,
            colors: vec![0u8; 4 * 4 * 4],
            width: 4,
            height: 4,
            hotx: 0,
            hoty: 0,
        };
        let b64 = cursor_png_b64(&cur);
        assert!(!b64.is_empty());
        // Decode back and check the PNG signature + chunk lengths.
        let bytes = decode_b64(&b64);
        assert_eq!(&bytes[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(&bytes[12..16], b"IHDR");
        assert_eq!(&bytes[16..20], &4u32.to_be_bytes());
        assert_eq!(&bytes[20..24], &4u32.to_be_bytes());
        assert_eq!(&bytes[bytes.len() - 8..bytes.len() - 4], b"IEND");
    }

    #[test]
    fn crc32_known_vector() {
        // PNG spec example: CRC of "123456789" is 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE");
        assert_eq!(base64_encode(b"M"), "TQ");
    }

    #[test]
    fn draw_cursor_composites_alpha() {
        let mut dst = [0u8; 4 * 4 * 4];
        let cur = CursorData {
            id: 1,
            colors: vec![0, 0, 0, 255, 0, 0, 0, 0], // 1x2: opaque + transparent
            width: 1,
            height: 2,
            hotx: 0,
            hoty: 0,
        };
        draw_cursor(&mut dst, 16, 4, 4, 1, 1, &cur);
        // Opaque pixel lands at (1,1) B=0; the transparent row at (1,2) no-ops.
        assert_eq!(dst[1 * 16 + 1 * 4], 0); // (1,1) B
        assert_eq!(dst[2 * 16 + 1 * 4], 0); // (1,2) untouched
        // A red cursor onto the black frame:
        let cur2 = CursorData {
            id: 2,
            colors: vec![0, 0, 255, 255],
            width: 1,
            height: 1,
            hotx: 0,
            hoty: 0,
        };
        draw_cursor(&mut dst, 16, 4, 4, 2, 0, &cur2);
        assert_eq!(dst[0 * 16 + 2 * 4 + 2], 255); // R at (2,0)
        // Alpha blending: half-alpha red over green => blend in [0..255].
        let mut dst2 = [0u8; 4 * 4 * 4];
        for px in dst2.chunks_mut(4) {
            px[1] = 255; // green background
        }
        let half = CursorData {
            id: 3,
            colors: vec![0, 0, 255, 128],
            width: 1,
            height: 1,
            hotx: 0,
            hoty: 0,
        };
        draw_cursor(&mut dst2, 16, 4, 4, 0, 0, &half);
        assert_eq!(dst2[2], 128); // R = 255*128/255
        assert_eq!(dst2[1], 127); // G = 255*127/255
    }

    #[test]
    fn draw_cursor_clips_out_of_bounds() {
        let mut dst = [9u8; 4 * 4 * 4];
        let cur = CursorData {
            id: 3,
            colors: vec![1, 2, 3, 255],
            width: 1,
            height: 1,
            hotx: 0,
            hoty: 0,
        };
        draw_cursor(&mut dst, 16, 4, 4, 4, 4, &cur); // fully out at (4,4)
        assert!(dst.iter().all(|&b| b == 9));
        // Partially out at (-1,0): the pixel would land at (0,0)? No — a 1x1
        // cursor at (-1,0) is fully out horizontally.
        draw_cursor(&mut dst, 16, 4, 4, -1, 0, &cur);
        assert!(dst.iter().all(|&b| b == 9));
    }

    fn decode_b64(s: &str) -> Vec<u8> {
        let chars: Vec<u8> = s.bytes().collect();
        let mut out = Vec::new();
        for chunk in chars.chunks(4) {
            let mut n = 0u32;
            for &c in chunk {
                n = (n << 6)
                    | match c {
                        b'A'..=b'Z' => (c - b'A') as u32,
                        b'a'..=b'z' => (c - b'a' + 26) as u32,
                        b'0'..=b'9' => (c - b'0' + 52) as u32,
                        b'+' => 62,
                        b'/' => 63,
                        _ => 0,
                    };
            }
            let bytes = n.to_be_bytes();
            let take = chunk.len() - 1;
            out.extend_from_slice(&bytes[4 - take..]);
        }
        out
    }
}
