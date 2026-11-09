//! #28: 最小 PNG 编码器（8-bit、color type 6 = RGBA、filter 0）。
//!
//! 与 `cursor.rs` 里给指针用的那一份是同一套做法 —— `flate2` 的 zlib + 手写
//! CRC32，不引新依赖。单独成模块是因为两者的**输入**完全不同：指针是 32×32 的
//! `CursorData`，截图是整屏 1.5 MB 的 BGRA；共用的是"写 chunk / 算 CRC"这十几行，
//! 之前它们埋在 `cursor` 里，别人要用只能复制。
//!
//! 为什么不用 `image` / `png` crate：本仓库对依赖很克制（vendored scrap、手写
//! BMP dump、手写 base64），而 PNG 的"能显示"子集只有 4 个 chunk、一种过滤器，
//! 一个 crate 换进来要连带 `flate2` 之外的解码器、色彩空间和元数据表。

use std::io::Write;

/// 把紧排的 BGRA（Windows DIB 顺序）编成 PNG。
///
/// `bgra` 必须是 `width * height * 4` 字节，行间**没有** padding：管线里的
/// `pristine_bgra`（`pipeline::mod` 的 #70 快照）就是这个形状。输入短了返回空
/// 向量而不是 panic —— 调用方在 HTTP 线程上，一条 500 比一个 abort 好。
pub fn encode_bgra(width: u32, height: u32, bgra: &[u8]) -> Vec<u8> {
    let row_bytes = width as usize * 4;
    if bgra.len() < row_bytes * height as usize {
        return Vec::new();
    }
    let mut raw = Vec::with_capacity((row_bytes + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0); // filter: None
        // IHDR 写的是 RGBA，输入是 BGRA：这一趟换序不能省。白字黑底看不出，
        // 一旦截图里有蓝色资源管理器标题栏或红色按钮，通道错了就是紫的。
        for px in bgra[y * row_bytes..(y + 1) * row_bytes].chunks_exact(4) {
            raw.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    wrap(width, height, &raw)
}

/// 把**已经带好每行 filter 字节**的原始数据包成 PNG。
///
/// `filtered` 的长度必须是 `(width * 4 + 1) * height`。`cursor.rs` 用它，因为那
/// 边已经按行做过 BGRA→RGBA 换序（并附了注释解释为什么）。
pub(crate) fn wrap(width: u32, height: u32, filtered: &[u8]) -> Vec<u8> {
    let mut idat = Vec::new();
    {
        // `fast`（zlib 级别 1）而不是默认的 6：截图是"点一下等结果"的交互，
        // 一帧 1920×1080 的桌面用默认级别要等几百毫秒，而桌面截图大量是平坦区域，
        // 级别 1 的压缩率本来就不差。具体数字记在 `docs/roadmap-l2-l3-gaps.md`
        // §2.10 的实测表里，由 `screenshot: ... encode=Nms` 那行量出来。
        // 指针图（32×32）差几十字节，无所谓。
        let mut enc = flate2::write::ZlibEncoder::new(&mut idat, flate2::Compression::fast());
        let _ = enc.write_all(filtered);
        let _ = enc.finish();
    }

    let mut png = Vec::with_capacity(64 + idat.len());
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    // 8-bit, RGBA, deflate, adaptive filtering, no interlace.
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    push_chunk(&mut png, b"IHDR", &ihdr);
    push_chunk(&mut png, b"IDAT", &idat);
    push_chunk(&mut png, b"IEND", &[]);
    png
}

pub(crate) fn push_chunk(png: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    png.extend_from_slice(tag);
    png.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(tag);
    crc_input.extend_from_slice(data);
    png.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// CRC-32 (ISO 3309 / PNG), reflected, table-less bitwise.
///
/// 表驱动的版本快两个数量级，但要 1 KB 静态表；这里每次调用最多几 MB，实测
/// 一帧 756×488 的 PNG 里 CRC 只占个位数毫秒，不值得。
pub(crate) fn crc32(data: &[u8]) -> u32 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::TryInto;
    use std::io::Read;

    /// Read one chunk back out of an encoded PNG body.
    fn chunk<'a>(png: &'a [u8], want: &[u8; 4]) -> Option<&'a [u8]> {
        let mut at = 8; // signature
        while at + 8 <= png.len() {
            let len = u32::from_be_bytes(png[at..at + 4].try_into().ok()?) as usize;
            let tag = &png[at + 4..at + 8];
            let body = png.get(at + 8..at + 8 + len)?;
            if tag == want {
                return Some(body);
            }
            at += 12 + len;
        }
        None
    }

    #[test]
    fn a_two_by_two_bgra_square_roundtrips_to_rgba() {
        // Three distinct colours plus one transparent pixel.
        let bgra = [
            0, 0, 255, 255, // red   (B=0,G=0,R=255)
            255, 0, 0, 255, // blue
            0, 255, 0, 128, // green, half alpha
            9, 9, 9, 0, // transparent
        ];
        let png = encode_bgra(2, 2, &bgra);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

        let ihdr = chunk(&png, b"IHDR").expect("IHDR");
        assert_eq!(u32::from_be_bytes(ihdr[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(ihdr[4..8].try_into().unwrap()), 2);
        assert_eq!(ihdr[8], 8, "bit depth");
        assert_eq!(ihdr[9], 6, "colour type RGBA");

        let mut raw = Vec::new();
        flate2::read::ZlibDecoder::new(chunk(&png, b"IDAT").expect("IDAT"))
            .read_to_end(&mut raw)
            .expect("inflate");
        // Two rows, each: filter byte + 4 bytes of RGBA.
        assert_eq!(raw.len(), 2 * (1 + 8));
        assert_eq!(&raw[1..5], &[255, 0, 0, 255], "BGRA red -> RGBA red");
        assert_eq!(&raw[5..9], &[0, 0, 255, 255], "BGRA blue -> RGBA blue");
        assert_eq!(&raw[10..14], &[0, 255, 0, 128], "green keeps alpha");
        assert_eq!(&raw[14..18], &[9, 9, 9, 0], "transparent stays transparent");
    }

    #[test]
    fn a_short_buffer_returns_nothing_instead_of_panicking() {
        assert!(encode_bgra(4, 4, &[0; 16]).is_empty());
    }

    #[test]
    fn every_row_carries_its_own_filter_byte() {
        let png = encode_bgra(1, 3, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        let mut raw = Vec::new();
        flate2::read::ZlibDecoder::new(chunk(&png, b"IDAT").unwrap())
            .read_to_end(&mut raw)
            .unwrap();
        assert_eq!(raw.len(), 3 * 5);
        for row in 0..3 {
            assert_eq!(raw[row * 5], 0, "filter None on row {row}");
        }
    }

    #[test]
    fn crc32_known_vector() {
        // The canonical check value for "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
