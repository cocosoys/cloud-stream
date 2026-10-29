//! #85 image codecs for the binary clipboard: PNG / BMP in, DIB out.
//!
//! The clipboard bridge is symmetric in protocol but *not* in format. Windows
//! applications exchange `CF_DIB` (a bare `BITMAPINFOHEADER` plus bottom-up
//! pixels); browsers exchange `image/png`. Nothing in the Windows API converts
//! between them, so this module is the seam:
//!
//! * uplink: browser PNG (or BMP) -> [`decode`] -> [`to_dib`] -> `CF_DIB`;
//! * downlink: host `CF_DIB` -> [`dib_to_bmp`] -> `image/bmp` -> the browser
//!   decodes it with `createImageBitmap` and re-encodes to PNG itself (its
//!   `reencodeBlobAsPng` path). Sending BMP rather than transcoding to PNG here
//!   is deliberate: a PNG *encoder* is another few hundred lines, and the
//!   browser already has one.
//!
//! Deliberately narrow, because a clipboard codec that guesses is worse than one
//! that refuses: 8-bit, non-interlaced PNG (what every browser screenshot and
//! canvas export produces, colour types 0/2/3/4/6), 24/32-bit BI_RGB BMP. A
//! PNG that is 16-bit, interlaced or CMYK is rejected with a log line and the
//! paste behaves as if the clipboard were empty — which is what it effectively
//! was before this module existed.

use anyhow::{anyhow, bail, Result};
use flate2::read::ZlibDecoder;
use std::io::Read;

/// Decoded raster, top-down BGRA. Windows DIBs are bottom-up and browsers are
/// top-down; keeping one convention inside avoids flipping twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4`, row-major, top row first, BGRA byte order.
    pub bgra: Vec<u8>,
}

impl Image {
    fn new(width: u32, height: u32) -> Result<Self> {
        let len = width as usize * height as usize * 4;
        if width == 0 || height == 0 || len > 512 * 1024 * 1024 {
            bail!("implausible image size {width}x{height}");
        }
        Ok(Self {
            width,
            height,
            bgra: vec![0; len],
        })
    }

    fn set(&mut self, x: usize, y: usize, r: u8, g: u8, b: u8, a: u8) {
        let i = (y * self.width as usize + x) * 4;
        self.bgra[i] = b;
        self.bgra[i + 1] = g;
        self.bgra[i + 2] = r;
        self.bgra[i + 3] = a;
    }
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Decode `image/png` or a BMP/DIB blob into BGRA, dispatching on the signature.
pub fn decode(bytes: &[u8]) -> Result<Image> {
    if bytes.starts_with(&PNG_SIGNATURE) {
        decode_png(bytes)
    } else if bytes.starts_with(b"BM") {
        decode_dib(&bytes[14..])
    } else if looks_like_dib(bytes) {
        decode_dib(bytes)
    } else {
        bail!("unrecognised image format ({} bytes)", bytes.len())
    }
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn le_i32(b: &[u8]) -> i32 {
    i32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

/// A `BITMAPINFOHEADER`-family blob starts with its own size (40, 52, 108, 124).
fn looks_like_dib(b: &[u8]) -> bool {
    b.len() >= 40 && matches!(le32(b), 40 | 52 | 56 | 108 | 124)
}

/// Decode one PNG image (colour types 0/2/3/4/6, bit depth 8, non-interlaced).
pub fn decode_png(bytes: &[u8]) -> Result<Image> {
    if !bytes.starts_with(&PNG_SIGNATURE) {
        bail!("not a PNG");
    }
    let mut pos = PNG_SIGNATURE.len();
    let mut header: Option<(u32, u32, u8, u8)> = None; // w, h, depth, colour
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut palette_alpha: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();
    let mut seen_iend = false;

    while pos + 8 <= bytes.len() {
        let len = be32(&bytes[pos..]) as usize;
        let kind = &bytes[pos + 4..pos + 8];
        let body_start = pos + 8;
        let body_end = body_start
            .checked_add(len)
            .filter(|e| *e <= bytes.len())
            .ok_or_else(|| anyhow!("PNG chunk {kind:?} runs past the end"))?;
        let body = &bytes[body_start..body_end];
        match kind {
            b"IHDR" => {
                if body.len() < 13 {
                    bail!("short IHDR");
                }
                let w = be32(body);
                let h = be32(&body[4..]);
                let depth = body[8];
                let colour = body[9];
                let interlace = body[12];
                if interlace != 0 {
                    bail!("interlaced PNG is not supported");
                }
                if depth != 8 {
                    bail!("{depth}-bit PNG is not supported (8-bit only)");
                }
                if !matches!(colour, 0 | 2 | 3 | 4 | 6) {
                    bail!("PNG colour type {colour} is not supported");
                }
                header = Some((w, h, depth, colour));
            }
            b"PLTE" => {
                palette = body
                    .chunks_exact(3)
                    .map(|c| [c[0], c[1], c[2]])
                    .collect();
            }
            b"tRNS" => palette_alpha = body.to_vec(),
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => {
                seen_iend = true;
                break;
            }
            _ => {}
        }
        pos = body_end + 4; // + CRC
    }
    if !seen_iend {
        bail!("PNG ended without IEND");
    }
    let (width, height, _depth, colour) =
        header.ok_or_else(|| anyhow!("PNG has no IHDR"))?;

    let mut raw = Vec::new();
    ZlibDecoder::new(&idat[..])
        .read_to_end(&mut raw)
        .map_err(|e| anyhow!("PNG zlib: {e}"))?;

    let channels: usize = match colour {
        0 => 1,
        2 => 3,
        3 => 1,
        4 => 2,
        _ => 4,
    };
    let stride = width as usize * channels;
    let expected = (stride + 1) * height as usize;
    if raw.len() < expected {
        bail!("PNG pixel data short: {} < {expected}", raw.len());
    }

    let mut img = Image::new(width, height)?;
    let mut prev = vec![0u8; stride];
    let mut line = vec![0u8; stride];
    for y in 0..height as usize {
        let base = y * (stride + 1);
        let filter = raw[base];
        line.copy_from_slice(&raw[base + 1..base + 1 + stride]);
        unfilter(filter, channels, &mut line, &prev)?;
        for x in 0..width as usize {
            let o = x * channels;
            let (r, g, b, a) = match colour {
                0 => (line[o], line[o], line[o], 255),
                4 => (line[o], line[o], line[o], line[o + 1]),
                2 => (line[o], line[o + 1], line[o + 2], 255),
                3 => {
                    let idx = line[o] as usize;
                    let [r, g, b] = *palette
                        .get(idx)
                        .ok_or_else(|| anyhow!("palette index {idx} out of range"))?;
                    (r, g, b, palette_alpha.get(idx).copied().unwrap_or(255))
                }
                _ => (line[o], line[o + 1], line[o + 2], line[o + 3]),
            };
            img.set(x, y, r, g, b, a);
        }
        std::mem::swap(&mut prev, &mut line);
    }
    Ok(img)
}

/// Undo one PNG scanline filter in place (`prev` is the reconstructed line
/// above, already unfiltered — the filters are defined against that, not
/// against the raw stream).
fn unfilter(filter: u8, bpp: usize, line: &mut [u8], prev: &[u8]) -> Result<()> {
    match filter {
        0 => {}
        1 => {
            for i in bpp..line.len() {
                line[i] = line[i].wrapping_add(line[i - bpp]);
            }
        }
        2 => {
            for i in 0..line.len() {
                line[i] = line[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            for i in 0..line.len() {
                let left = if i >= bpp { line[i - bpp] as u16 } else { 0 };
                line[i] = line[i].wrapping_add(((left + prev[i] as u16) / 2) as u8);
            }
        }
        4 => {
            for i in 0..line.len() {
                let a = if i >= bpp { line[i - bpp] as i16 } else { 0 };
                let b = prev[i] as i16;
                let c = if i >= bpp { prev[i - bpp] as i16 } else { 0 };
                let p = a + b - c;
                let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                let pred = if pa <= pb && pa <= pc {
                    a
                } else if pb <= pc {
                    b
                } else {
                    c
                };
                line[i] = line[i].wrapping_add(pred as u8);
            }
        }
        other => bail!("unknown PNG filter {other}"),
    }
    Ok(())
}

/// How many bytes of a `CF_DIB` blob are actually the image.
///
/// `GlobalSize` reports the *allocation*, which the heap rounds up — a 40-byte
/// header plus an 8x8 icon can arrive inside a 64 KiB block, and wrapping all of
/// it in a BMP file sends megabytes of unrelated heap for a screenshot. Returns
/// `None` for a header this module cannot measure, in which case the caller
/// keeps the whole blob.
pub fn dib_expected_len(dib: &[u8]) -> Option<usize> {
    if dib.len() < 40 {
        return None;
    }
    let header = le32(dib) as usize;
    let width = le_i32(&dib[4..]);
    let height = le_i32(&dib[8..]).unsigned_abs() as usize;
    let bit_count = le16(&dib[14..]) as usize;
    if width <= 0 || height == 0 || !matches!(bit_count, 24 | 32) || header < 40 {
        return None;
    }
    let stride = (width as usize * (bit_count / 8) + 3) & !3;
    Some(header + stride * height)
}

/// Decode a `CF_DIB` blob (or a BMP file body). 24- and 32-bit BI_RGB only —
/// RLE and bitfields are what old applications use, not what a screenshot is.
pub fn decode_dib(dib: &[u8]) -> Result<Image> {
    if dib.len() < 40 {
        bail!("DIB shorter than a BITMAPINFOHEADER");
    }
    let header_size = le32(dib) as usize;
    let width = le_i32(&dib[4..]);
    let height_raw = le_i32(&dib[8..]);
    let bit_count = le16(&dib[14..]);
    let compression = le32(&dib[16..]);
    if width <= 0 || height_raw == 0 {
        bail!("DIB has no extent ({width}x{height_raw})");
    }
    if compression != 0 {
        bail!("DIB compression {compression} is not BI_RGB");
    }
    if !matches!(bit_count, 24 | 32) {
        bail!("{bit_count}-bit DIB is not supported");
    }
    let top_down = height_raw < 0;
    let width = width as usize;
    let height = height_raw.unsigned_abs() as usize;
    let bpp = (bit_count / 8) as usize;
    let stride = (width * bpp + 3) & !3;
    let start = header_size;
    let need = start + stride * height;
    if dib.len() < need {
        bail!("DIB pixel data short: {} < {need}", dib.len());
    }

    let mut img = Image::new(width as u32, height as u32)?;
    for row in 0..height {
        // Bottom-up (the normal case) means the first row in memory is the last
        // row of the picture.
        let y = if top_down { row } else { height - 1 - row };
        let src = start + row * stride;
        for x in 0..width {
            let o = src + x * bpp;
            let (b, g, r) = (dib[o], dib[o + 1], dib[o + 2]);
            // 32-bit BI_RGB defines the fourth byte as unused; only use it when
            // it actually carries alpha, otherwise screenshots paste as a
            // transparent rectangle in apps that honour it.
            let a = if bpp == 4 && dib[o + 3] != 0 {
                dib[o + 3]
            } else {
                255
            };
            img.set(x, y, r, g, b, a);
        }
    }
    Ok(img)
}

/// Encode as `CF_DIB`: 40-byte `BITMAPINFOHEADER` + bottom-up 32-bit BGRA.
/// 32-bit (not 24) because apps that understand alpha then get it for free, and
/// the row padding problem disappears.
pub fn to_dib(img: &Image) -> Vec<u8> {
    let w = img.width as i32;
    let h = img.height as i32;
    let mut out = Vec::with_capacity(40 + img.bgra.len());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bit count
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(img.bgra.len() as u32).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes()); // x ppm
    out.extend_from_slice(&0i32.to_le_bytes()); // y ppm
    out.extend_from_slice(&0u32.to_le_bytes()); // colours used
    out.extend_from_slice(&0u32.to_le_bytes()); // important colours
    let stride = img.width as usize * 4;
    for row in (0..img.height as usize).rev() {
        out.extend_from_slice(&img.bgra[row * stride..(row + 1) * stride]);
    }
    out
}

/// Wrap a `CF_DIB` blob in a `BITMAPFILEHEADER` so a browser can read it as
/// `image/bmp`. The DIB keeps its own header and pixel rows untouched — BMP is
/// literally a 14-byte prefix around a DIB.
pub fn dib_to_bmp(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let header_size = le32(dib) as usize;
    let bit_count = le16(&dib[14..]);
    let colours_used = if dib.len() >= 36 { le32(&dib[32..]) } else { 0 };
    let palette_entries = if bit_count <= 8 {
        if colours_used > 0 {
            colours_used as usize
        } else {
            1usize << bit_count
        }
    } else {
        0
    };
    let off_bits = 14usize
        .checked_add(header_size)?
        .checked_add(palette_entries * 4)?;
    if off_bits > dib.len() + 14 {
        return None;
    }
    let total = 14 + dib.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(off_bits as u32).to_le_bytes());
    out.extend_from_slice(dib);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Image {
        let mut img = Image::new(3, 2).unwrap();
        // Distinct channel values per pixel so a channel swap cannot hide.
        for y in 0..2 {
            for x in 0..3 {
                img.set(x, y, (10 + x * 10) as u8, (20 + y * 10) as u8, 200, 255);
            }
        }
        img
    }

    /// DIB is bottom-up and BMP is a DIB with a prefix, so the round trip must
    /// survive both row order and the header.
    #[test]
    fn dib_round_trip_preserves_pixels() {
        let img = sample();
        let dib = to_dib(&img);
        assert_eq!(decode_dib(&dib).unwrap(), img);

        let bmp = dib_to_bmp(&dib).unwrap();
        assert_eq!(&bmp[..2], b"BM");
        assert_eq!(le32(&bmp[10..]) as usize, 14 + 40, "pixel offset");
        assert_eq!(decode(&bmp).unwrap(), img);
    }

    /// A 32-bit DIB whose alpha byte is zero is opaque in practice: treating it
    /// as transparent pastes an invisible rectangle.
    #[test]
    fn zero_alpha_dib_is_treated_as_opaque() {
        let img = sample();
        let mut dib = to_dib(&img);
        for byte in dib.iter_mut().skip(40).step_by(4) {
            *byte = 0;
        }
        let back = decode_dib(&dib).unwrap();
        assert!(back.bgra.chunks(4).all(|px| px[3] == 255));
    }

    /// 24-bit DIBs are row-padded to 4 bytes; a decoder that ignores the pad
    /// shears the picture.
    #[test]
    fn twenty_four_bit_dib_honours_row_padding() {
        // 3 px wide * 3 bytes = 9 -> padded to 12.
        let w = 3usize;
        let stride = 12usize;
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&(w as i32).to_le_bytes());
        dib.extend_from_slice(&1i32.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&24u16.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        dib.extend_from_slice(&(stride as u32).to_le_bytes());
        dib.extend_from_slice(&[0u8; 16]);
        for x in 0..w {
            dib.extend_from_slice(&[x as u8, 0, 0]); // BGR
        }
        dib.extend_from_slice(&[0xAA, 0xAA, 0xAA]); // padding, must be ignored
        let img = decode_dib(&dib).unwrap();
        assert_eq!(img.width, 3);
        assert_eq!(img.bgra[0], 0); // b of pixel 0
        assert_eq!(img.bgra[4], 1); // b of pixel 1
        assert_eq!(img.bgra[8], 2); // b of pixel 2
    }

    /// Build a minimal PNG so the decoder is tested against real chunk framing,
    /// CRC bytes included (the decoder skips CRCs but must still step over them).
    fn encode_png(width: u32, height: u32, colour: u8, filtered: &[u8]) -> Vec<u8> {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut z = ZlibEncoder::new(Vec::new(), Compression::fast());
        z.write_all(filtered).unwrap();
        let idat = z.finish().unwrap();

        let mut out = Vec::new();
        out.extend_from_slice(&PNG_SIGNATURE);
        {
            let mut chunk = |kind: &[u8; 4], body: &[u8]| {
                out.extend_from_slice(&(body.len() as u32).to_be_bytes());
                out.extend_from_slice(kind);
                out.extend_from_slice(body);
                // The decoder ignores the CRC; zero is a legal placeholder for it.
                out.extend_from_slice(&[0, 0, 0, 0]);
            };
            let mut ihdr = Vec::new();
            ihdr.extend_from_slice(&width.to_be_bytes());
            ihdr.extend_from_slice(&height.to_be_bytes());
            ihdr.extend_from_slice(&[8, colour, 0, 0, 0]);
            chunk(b"IHDR", &ihdr);
            chunk(b"IDAT", &idat);
            chunk(b"IEND", &[]);
        }
        out
    }

    #[test]
    fn png_rgba_without_filters_decodes() {
        // 2x2 RGBA, filter byte 0 on each row.
        let raw = [
            0, 255, 0, 0, 255, 0, 255, 0, 255, // red, green
            0, 0, 0, 255, 255, 255, 255, 255, 255, // blue, white
        ];
        let png = encode_png(2, 2, 6, &raw);
        let img = decode(&png).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        // BGRA order.
        assert_eq!(&img.bgra[0..4], &[0, 0, 255, 255]);
        assert_eq!(&img.bgra[4..8], &[0, 255, 0, 255]);
        assert_eq!(&img.bgra[8..12], &[255, 0, 0, 255]);
        assert_eq!(&img.bgra[12..16], &[255, 255, 255, 255]);
    }

    /// Filter 2 ("Up") is the one a screenshot encoder actually picks on flat
    /// backgrounds; decoding it wrong shifts whole rows.
    #[test]
    fn png_up_filter_reconstructs_rows() {
        // 1x3 grey: 10, 20, 30 encoded with filter 2 => 0, 10, 10.
        let raw = [0, 10, 2, 10, 2, 10];
        let png = encode_png(1, 3, 0, &raw);
        let img = decode(&png).unwrap();
        assert_eq!(img.bgra[0], 10);
        assert_eq!(img.bgra[4], 20);
        assert_eq!(img.bgra[8], 30);
    }

    /// Filter 4 (Paeth) is what libpng emits by default for photos.
    #[test]
    fn png_paeth_filter_reconstructs_rows() {
        // 2x2 grey, first row 5 9, second row 7 12, filter 4 on both.
        // Row 2 bytes are deltas against the Paeth predictor of (left, up,
        // upper-left) = (0,5,0) -> 5 and (7,9,5) -> 9, so the encoded bytes are
        // 2 and 3.
        let raw = [4, 5, 4, 4, 2, 3];
        let png = encode_png(2, 2, 0, &raw);
        let img = decode(&png).unwrap();
        assert_eq!(img.bgra[0], 5);
        assert_eq!(img.bgra[4], 9);
        assert_eq!(img.bgra[8], 7);
        assert_eq!(img.bgra[12], 12);
    }

    /// A 16-bit or interlaced PNG is refused rather than half-decoded: a
    /// half-decoded image on the clipboard is worse than none.
    #[test]
    fn exotic_png_is_refused() {
        let raw = [0u8, 0, 0];
        let mut png = encode_png(1, 1, 6, &raw);
        // IHDR starts at 8 (signature) + 8 (len+type); depth is body byte 8.
        png[8 + 8 + 8] = 16;
        assert!(decode(&png).is_err());

        let raw = [0u8, 0, 0, 0, 0];
        let mut png = encode_png(1, 1, 6, &raw);
        png[8 + 8 + 12] = 1; // interlace byte
        assert!(decode(&png).is_err());
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(decode(b"not an image at all").is_err());
        assert!(decode(&[]).is_err());
        assert!(dib_to_bmp(&[0u8; 8]).is_none());
    }

    /// `GlobalSize` hands back a rounded allocation; the BMP we build must be
    /// cut at the header's geometry, not at the end of the heap block.
    #[test]
    fn dib_length_follows_the_header_not_the_allocation() {
        let img = sample();
        let mut dib = to_dib(&img);
        let real = dib.len();
        dib.resize(real + 4096, 0xCD);
        assert_eq!(dib_expected_len(&dib), Some(real));
        // An unmeasurable header keeps the whole blob (caller falls back).
        assert_eq!(dib_expected_len(&[0u8; 40]), None);
    }
}
