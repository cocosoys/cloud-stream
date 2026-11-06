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
    BITMAPV5HEADER, BI_BITFIELDS, BI_RGB, DIB_RGB_COLORS,
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

/// `GetDIBits` 一块位图成 32bpp、top-down 的 `w * h * 4` 字节。
///
/// `color_plane` 为真时用 `BI_BITFIELDS` + RGBA 掩码（光标颜色平面是 DIB
/// section，只有这样才能拿到它自己的 alpha）；为假时用 `BI_RGB` —— 1bpp 的
/// 掩码位图会被 GDI 按调色板展开成 `0x00000000` / `0x00FFFFFF`，每个像素只
/// 需要一个比特的信息，读第一个字节就够。
///
/// **高度必须传负数**：光标位图的行序是"本来就是 top-down 的 DIB"，再翻一次
/// 会得到倒过来的指针（`GetBitmapBits` 时代的老坑）。
unsafe fn read_dib_topdown(
    hbm: isize,
    w: usize,
    h: usize,
    color_plane: bool,
) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0u8; w * h * 4];
    let dc = CreateCompatibleDC(0);
    if dc == 0 {
        return Err(anyhow::anyhow!("CreateCompatibleDC failed: {}", last_os_error()));
    }
    let old = SelectObject(dc, hbm);
    let mut bi: BITMAPV5HEADER = std::mem::zeroed();
    bi.bV5Size = std::mem::size_of::<BITMAPV5HEADER>() as u32;
    bi.bV5Width = w as i32;
    bi.bV5Height = -(h as i32);
    bi.bV5Planes = 1;
    bi.bV5BitCount = 32;
    if color_plane {
        bi.bV5Compression = BI_BITFIELDS;
        bi.bV5RedMask = 0x00FF_0000;
        bi.bV5GreenMask = 0x0000_FF00;
        bi.bV5BlueMask = 0x0000_00FF;
        bi.bV5AlphaMask = 0xFF00_0000;
    } else {
        bi.bV5Compression = BI_RGB;
    }
    let got = GetDIBits(
        dc,
        hbm,
        0,
        h as u32,
        buf.as_mut_ptr() as _,
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
    Ok(buf)
}

/// 单色光标：`hbmMask` 里上下叠着 AND 与 XOR 两张 1bpp 掩码。
///
/// 这不是"罕见的边角情况"。`CreateCursor()` 造出来的光标都是单色的，而被控机
/// 上最常见的那几个恰恰出自它：文本 I 形、十字、各种缩放/拖拽箭头，以及
/// **VMware / VirtualBox 这类虚拟机的控制台指针**（.148 上就是它）。跳过
/// `hbmColor == 0` 等于"页面上永远拿不到指针形状"，而这条路径在日志里只留一句
/// `debug`，看起来什么都不像坏了。
///
/// 逐像素规则（Windows 的 AND/XOR 掩码语义）：
///
/// | AND | XOR | 效果 |
/// |---|---|---|
/// | 1 | 0 | 透明 |
/// | 0 | 1 | 白 |
/// | 0 | 0 | 黑 |
/// | 1 | 1 | 反色（这里近似成白；极少见） |
fn monochrome_cursor(
    hcursor: u64,
    ii: &ICONINFO,
) -> anyhow::Result<Option<CursorData>> {
    if ii.hbmMask == 0 {
        return Ok(None);
    }
    let mut bm: BITMAP = unsafe { std::mem::zeroed() };
    let got = unsafe {
        GetObjectW(
            ii.hbmMask as _,
            std::mem::size_of::<BITMAP>() as i32,
            &mut bm as *mut _ as *mut _,
        )
    };
    if got == 0 {
        return Err(anyhow::anyhow!(
            "GetObjectW(hbmMask) failed: {}",
            last_os_error()
        ));
    }
    let w = bm.bmWidth as usize;
    let h2 = bm.bmHeight as usize;
    if w == 0 || h2 < 2 || h2 % 2 != 0 || w > 256 || h2 > 512 {
        log::debug!("cursor {hcursor:#x}: unusable mask {w}x{h2}");
        return Ok(None);
    }
    let h = h2 / 2;
    let mask = unsafe { read_dib_topdown(ii.hbmMask, w, h2, false)? };
    let colors = compose_mono_mask(&mask, w, h);
    Ok(Some(CursorData {
        id: hcursor,
        colors,
        width: w,
        height: h,
        hotx: ii.xHotspot as i32,
        hoty: ii.yHotspot as i32,
    }))
}

/// 把展开成 32bpp 的 AND/XOR 掩码（上下两半）合成 BGRA。
///
/// 单独拆出来是为了能测：`monochrome_cursor` 要先拿到真的 `hbmMask`，而这张
/// 表的四个格子（透明/黑/白/反色）才是唯一容易写反的地方。
fn compose_mono_mask(mask: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut colors = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            // 展开成 32bpp 之后，一个"比特"就是一个像素的第一个字节：0 或 0xFF。
            let and_bit = mask[(y * w + x) * 4] != 0;
            let xor_bit = mask[((y + h) * w + x) * 4] != 0;
            let o = (y * w + x) * 4;
            let (r, g, b, a): (u8, u8, u8, u8) = match (and_bit, xor_bit) {
                (true, false) => (0, 0, 0, 0),
                (false, true) => (255, 255, 255, 255),
                (false, false) => (0, 0, 0, 255),
                (true, true) => (255, 255, 255, 255),
            };
            colors[o] = b;
            colors[o + 1] = g;
            colors[o + 2] = r;
            colors[o + 3] = a;
        }
    }
    colors
}

/// Capture the cursor bitmap for `hCursor` (RustDesk `get_cursor_data` subset).
///
/// 两条路径：有 `hbmColor` 的读颜色平面（自带 alpha）；没有的是**单色光标**，
/// 走 [`monochrome_cursor`]。只有"读不出来"才返回 `None`。
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
                return monochrome_cursor(hcursor, &ii);
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
            // 颜色平面用 BGRA 掩码读出来，byte-for-byte 就是我们的 `CursorData`
            // 布局，不需要再换序（换序是给 PNG 那一步的，见 `cursor_rgba_rows`）。
            let colors = read_dib_topdown(ii.hbmColor, w, h, true)?;
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

/// `cur.colors`（BGRA，Windows DIB 的顺序）→ PNG 的 RGBA 行数据，每行前加一个
/// filter 字节（0 = None）。
///
/// 这一次换序不能省：IHDR 里写的是 color type 6 = **RGBA**，而 `cursor_data`
/// 交出来的是 **BGRA**。白箭头三个通道相等，看不出问题；彩色指针（链接的手形、
/// 十字、游戏里的自绘指针）会把红蓝对调 —— 而这不是"哪条传输"的问题，WebRTC
/// 和 WebSocket 吃的是同一份 PNG。
fn cursor_rgba_rows(cur: &CursorData) -> Vec<u8> {
    let row_bytes = cur.width * 4;
    let mut raw = Vec::with_capacity((row_bytes + 1) * cur.height);
    for y in 0..cur.height {
        raw.push(0); // filter: None
        for px in cur.colors[y * row_bytes..(y + 1) * row_bytes].chunks_exact(4) {
            raw.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    raw
}

/// Encode `cur.colors` (top-down BGRA) as a base64 PNG for the frontend.
/// Dependency-lean: `flate2` zlib + hand-rolled CRC32 + base64.
pub fn cursor_png_b64(cur: &CursorData) -> String {
    let raw = cursor_rgba_rows(cur);

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
        // 尾巴必须补 `=`。浏览器侧的 `atob` 按 WHATWG 的 forgiving-base64 是能
        // 接受缺省的，但这段 base64 并不是喂给 `atob` 的：页面把它拼成
        // `url("data:image/png;base64,…")` 塞进 CSS 的 `cursor`，走的是浏览器的
        // data-URI 解码路径 —— 那条路没有"原谅"这一说。少两个 `=`，光标就静默
        // 退回默认箭头，页面上看起来像"服务端什么都没发"。
        if chunk.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(T[n as usize & 63] as char);
        } else {
            out.push('=');
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

    /// 单色光标的四种 AND/XOR 组合。
    ///
    /// `mask` 是 `GetDIBits` 展开后的样子：每像素 4 字节，只有第一个字节有意义
    /// （0 = 掩码位 0，0xFF = 掩码位 1）。上半是 AND，下半是 XOR。
    ///
    /// 写反的后果不是"不显示"，而是**整块黑方块**（透明被当成黑色）或**什么都
    /// 看不见**（黑被当成透明）—— 两种都很难从截图上看出根因。
    #[test]
    fn mono_mask_maps_the_four_and_xor_combinations() {
        // 4 个像素：透明(AND1,XOR0)、白(0,1)、黑(0,0)、反色(1,1)
        let w = 4;
        let h = 1;
        // 上半：AND = [1,0,0,1]，下半：XOR = [0,1,0,1]
        let and_bits = [0xFFu8, 0, 0, 0xFF];
        let xor_bits = [0u8, 0xFF, 0, 0xFF];
        let mut mask = vec![0u8; w * h * 4 * 2];
        for x in 0..w {
            mask[x * 4] = and_bits[x];
            mask[(w + x) * 4] = xor_bits[x];
        }
        let px = compose_mono_mask(&mask, w, h);
        let alpha: Vec<u8> = (0..w).map(|x| px[x * 4 + 3]).collect();
        assert_eq!(alpha, vec![0, 255, 255, 255], "透明必须是 alpha 0");
        // 白色像素三个通道全 255；黑色像素全 0。
        assert_eq!(&px[4..8], &[255, 255, 255, 255], "0/1 -> 白");
        assert_eq!(&px[8..12], &[0, 0, 0, 255], "0/0 -> 黑");
    }

    /// 真机验一次单色路径：造一个 `hbmColor == 0` 的光标，走完整的
    /// `GetIconInfo` → `GetObjectW(hbmMask)` → `GetDIBits` → 合成。
    ///
    /// 这是 .148 上真正坏掉的那条：虚拟机控制台的指针是 `CreateCursor()` 造的
    /// 单色光标，于是页面上永远没有指针。`compose_mono_mask` 只测了查表，这里
    /// 测的是"两张 1bpp 掩码能不能被读出来、上下半是不是对的那两半"。
    #[test]
    fn a_real_monochrome_cursor_decodes_through_geticoninfo() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{CreateCursor, DestroyCursor};

        const W: usize = 16;
        const H: usize = 16;
        let stride = 2; // 16 位 → 两个字节，CreateCursor 要求按 WORD 对齐
        // 左半：AND=0、XOR=1 → 白；右半：AND=1、XOR=0 → 透明。
        //
        // 左右不对称是有意的：如果 AND/XOR 上下两半读反了，结果会正好镜像过来
        // （左边变透明、右边变白），这条断言就会响。上下对称的图案测不出这个。
        let and_plane: Vec<u8> = (0..H).flat_map(|_| [0x00u8, 0xFF]).collect();
        let xor_plane: Vec<u8> = (0..H).flat_map(|_| [0xFFu8, 0x00]).collect();
        assert_eq!(and_plane.len(), stride * H);
        let hcur = unsafe {
            CreateCursor(
                0,
                0,
                0,
                W as i32,
                H as i32,
                and_plane.as_ptr() as _,
                xor_plane.as_ptr() as _,
            )
        };
        assert!(hcur != 0, "CreateCursor failed: {}", unsafe { last_os_error() });

        let cd = cursor_data(hcur as u64)
            .expect("GetIconInfo/GetDIBits must not fail on a hand-made cursor")
            .expect("a monochrome cursor must decode, not be skipped");
        unsafe { DestroyCursor(hcur) };

        assert_eq!((cd.width, cd.height), (W, H), "mask 高度要除以 2");
        // 左上角是白：AND=0、XOR=1。
        assert_eq!(
            &cd.colors[0..4],
            &[255, 255, 255, 255],
            "left half (AND=0, XOR=1) must be opaque white"
        );
        // 右上角透明：AND=1、XOR=0。
        let right_top = (W - 1) * 4;
        assert_eq!(
            cd.colors[right_top + 3],
            0,
            "right half (AND=1, XOR=0) must be transparent"
        );
    }

    #[test]
    fn crc32_known_vector() {
        // PNG spec example: CRC of "123456789" is 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// `colors` 是 BGRA，PNG 声明的是 RGBA —— 这里必须换序。
    ///
    /// 用一个纯红像素做判据：写错的话前三个字节是 `00 00 FF`（蓝），红指针在
    /// 页面上会变成蓝的。白/黑箭头察觉不到，所以这条测试是唯一会响的地方。
    #[test]
    fn png_rows_swap_bgra_to_rgba() {
        let cur = CursorData {
            id: 1,
            // 1x1 纯红：B=0 G=0 R=255 A=255
            colors: vec![0, 0, 255, 255],
            width: 1,
            height: 1,
            hotx: 0,
            hoty: 0,
        };
        let rows = cursor_rgba_rows(&cur);
        assert_eq!(rows, vec![0, 255, 0, 0, 255], "filter byte + RGBA");
    }

    /// base64 必须补齐 `=`：这段字符串最终进的是 CSS 的
    /// `url("data:image/png;base64,…")`，蹭不上 `atob` 那套"缺省也认"的宽容
    /// 规则。少了它，光标会静默退回默认箭头 —— 看起来像"服务端什么都没发"。
    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b"Man"), "TWFu", "3 的倍数：无 padding");
        assert_eq!(base64_encode(b"Ma"), "TWE=", "余 2：一个 =");
        assert_eq!(base64_encode(b"M"), "TQ==", "余 1：两个 =");
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
        // Padding 先摘掉：下面的 `take = chunk.len() - 1` 假设最后一个字符是
        // 数据而不是 `=`，带着它算会多吐一个字节。
        let chars: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
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
