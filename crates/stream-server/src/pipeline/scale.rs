//! BGRA bilinear scaler (#50).
//!
//! `scrap`'s libyuv FFI exposes no scale entry point, so the resolution-sync
//! path keeps a dependency-free scalar here. Fixed-point 16.16 bilinear with
//! edge clamping; cost scales with the *destination* pixel count (~0.9 Mpx for
//! 1280x720), a few ms in release and acceptable in debug for explicit client
//! resolution changes. The default path (client never asked, seed_native) does
//! not call this at all.

/// Scale a BGRA buffer from `(sw, sh)` into `dst` resized to `(dw, dh)`.
/// `dst` is resized by the caller contract: it must already have the exact
/// capacity `dw * dh * 4` (the caller reuses a scratch `Vec`).
pub fn bgra_scale(src: &[u8], sw: usize, sh: usize, dst: &mut [u8], dw: usize, dh: usize) {
    debug_assert_eq!(src.len(), sw * sh * 4);
    debug_assert_eq!(dst.len(), dw * dh * 4);
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return;
    }

    // 16.16 fixed point mapping: dst pixel center -> source pixel coordinate.
    const SHIFT: i64 = 16;
    const SCALE: i64 = 1 << SHIFT;
    // (dst + 0.5) / dw * sw  ->  ((2*dst+1)*sw) / (2*dw)
    let sx_step = (2 * sw as i64 * SCALE) / (2 * dw as i64);
    let sy_step = (2 * sh as i64 * SCALE) / (2 * dh as i64);
    let src_w = sw as i64;

    let mut sy = (sy_step / 2) - SCALE / 2; // y of the first destination row's center, in source units
    let mut out = 0usize;
    for _ in 0..dh {
        let (y0, fy) = source_row(sy, sh);
        let mut sx = (sx_step / 2) - SCALE / 2; // x of the first destination column's center
        for _ in 0..dw {
            let (x0, fx) = source_col(sx, sw);
            let y1 = (y0 + 1).min(sh - 1);
            let x1 = (x0 + 1).min(sw - 1);

            let a = 4 * ((y0 as i64 * src_w) + x0 as i64) as usize;
            let b = 4 * ((y0 as i64 * src_w) + x1 as i64) as usize;
            let c = 4 * ((y1 as i64 * src_w) + x0 as i64) as usize;
            let d = 4 * ((y1 as i64 * src_w) + x1 as i64) as usize;

            for ch in 0..4 {
                // i64 throughout: src(<=255) * SCALE(65536) * SCALE overflows i32.
                let top = src[a + ch] as i64 * (SCALE - fx)
                    + src[b + ch] as i64 * fx;
                let bot = src[c + ch] as i64 * (SCALE - fx)
                    + src[d + ch] as i64 * fx;
                let v = (top * (SCALE - fy) + bot * fy) / (SCALE * SCALE);
                dst[out] = v.clamp(0, 255) as u8;
                out += 1;
            }
            sx += sx_step;
        }
        sy += sy_step;
    }
}

/// Decompose a fixed-point source coordinate into (integer pixel, fraction).
#[inline]
fn source_row(v: i64, limit: usize) -> (usize, i64) {
    let clamped = v.clamp(0, ((limit as i64) << 16) - 1);
    (((clamped >> 16) as usize).min(limit - 1), clamped & 0xFFFF)
}

#[inline]
fn source_col(v: i64, limit: usize) -> (usize, i64) {
    source_row(v, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_scale_preserves_pixels() {
        let src: Vec<u8> = (0..(4 * 4 * 4)).map(|i| (i % 251) as u8).collect();
        let mut dst = vec![0u8; 4 * 4 * 4];
        bgra_scale(&src, 4, 4, &mut dst, 4, 4);
        assert_eq!(src, dst);
    }

    #[test]
    fn half_scale_averages_block() {
        // 2x2 -> 1x1: uniform colors, every channel averages to the same value.
        let src: Vec<u8> = vec![
            10, 20, 30, 40, 50, 60, 70, 80, //
            90, 100, 110, 120, 130, 140, 150, 160,
        ];
        let mut dst = vec![0u8; 4];
        bgra_scale(&src, 2, 2, &mut dst, 1, 1);
        // (10+50+90+130)/4=70, (20+60+100+140)/4=80, (30+70+110+150)/4=90, (40+80+120+160)/4=100
        assert_eq!(dst, vec![70, 80, 90, 100]);
    }

    #[test]
    fn upscale_keeps_size_and_corners_bounded() {
        let src: Vec<u8> = vec![0u8; 4 * 4 * 4];
        let mut dst = vec![0u8; 8 * 8 * 4];
        bgra_scale(&src, 4, 4, &mut dst, 8, 8);
        assert!(dst.iter().all(|&b| b == 0));
    }
}
