//! libvpx encoder wrapper over `scrap::codec::Encoder` (design 8.1 rows 2-3).
//!
//! Stage1 fixed settings (design 4.4 / 8.1):
//! * codec: VP8 by default; #94 makes it a pick between the two encoders libvpx
//!   already builds (`VP8` / `VP9`), carried in [`crate::pipeline::codec`]. The
//!   codec is fixed at open — it decides which `vpx_codec_*_cx()` is
//!   instantiated — and the SDP names the same one, so switching is a session
//!   restart, not a hot swap.
//! * quality: 1.0; `keyframe_interval` (#86): sized from `--keyframe-secs` as a
//!   frame-count **backstop** only. A pure P-frame chain (`VPX_KF_DISABLED`)
//!   drifts — quantizer error accumulates against the reference, the picture
//!   progressively darkens / textures up and encode time grows (fps drops) — so
//!   a periodic keyframe still has to exist, but `kf_max_dist` counts encoded
//!   frames, not seconds, and a static desktop encodes far fewer frames than a
//!   moving one. The wall-clock period is therefore enforced by the capture
//!   loop (`pipeline::mod`), which calls `force_keyframe` when
//!   `--keyframe-secs` (default 10 s) has elapsed. Combined with frame-dropping
//!   disabled (`rc_dropframe_thresh = 0`) the receiver's reference chain is
//!   re-anchored quickly after any loss; PLI-driven keyframes land in the same
//!   `force_keyframe` path.
//! * i444: false (fullcolor = false in `stream_info`).
//! * No recorder, no hardware fallback chain, no H264/H265/AV1 negotiation.
//!
//! Reused RustDesk/scrap symbols (0e bench chain already compiled through):
//! * `scrap::codec::{Encoder, EncoderCfg}`
//! * `scrap::{VpxEncoderConfig, VpxVideoCodecId, EncodeInput, EncodeYuvFormat, VpxNoOutput}`
//! * `encoder.yuvfmt()`, `encoder.encode_to_message(input, ms)`, `encoder.bitrate()`.
//!
//! Encoded frames arrive as `base::message_proto::VideoFrame` carrying
//! `union=Vp8s(EncodedVideoFrames{frames})` — or `Vp9s` for the VP9 encoder; the
//! two unions have the same shape and `pipeline::sample_map` accepts both.
//! Slicing into RTP-ready chunks lives in `pipeline::sample_map`. `VpxNoOutput`
//! (this round produced no packet) is normalized to `Ok(None)` so the loop
//! counts it as `encode_no_output` without treating it as a fatal encoder error.

use std::ffi::c_uint;

use base::message_proto::VideoFrame;
use scrap::codec::{Encoder, EncoderCfg};
use scrap::{EncodeInput, EncodeYuvFormat, VpxEncoderConfig, VpxNoOutput, VpxRateControl};

use crate::pipeline::codec::VideoCodec;
use crate::pipeline::settings::RateControl;

/// #86: frame-count backstop for `kf_max_dist`, sized at **twice** the
/// configured wall-clock period so the loop's own timer always fires first.
/// libvpx takes this as a `c_uint`; `usize` is what `VpxEncoderConfig` carries,
/// so the conversion happens here and the clamp keeps the cast in range.
fn keyframe_backstop_frames(fps: u32, keyframe_secs: u32) -> usize {
    // An uncapped session (`fps == 0`) has no frame rate to count from; the
    // ceiling stands in so its backstop is as long as a 240 fps one, not as
    // short as a 1 fps one (which would force an IDR every other frame).
    let fps = if fps == 0 { 240 } else { fps.clamp(1, 240) };
    let fps = u64::from(fps);
    let secs = u64::from(keyframe_secs.clamp(1, 120));
    (fps * secs * 2).clamp(60, 240_000) as usize
}

/// Thin lifecycle wrapper the capture loop holds between opens.
pub struct VideoEncoder {
    encoder: Encoder,
    yuvfmt: EncodeYuvFormat,
    width: u32,
    height: u32,
    /// #92: what this encoder was opened with. libvpx will not switch
    /// `rc_end_usage` on a live encoder, so the loop compares this against
    /// `StreamSettings::rate_control()` and rebuilds when they differ.
    rc: RateControl,
    /// The quantizer this encoder was opened with (only meaningful under CQ).
    crf: u32,
    /// #94: which libvpx encoder this is. Same reason as `rc`: the codec is a
    /// property of the instantiated codec context, so the loop compares it
    /// against the process-wide pick.
    codec: VideoCodec,
}

impl VideoEncoder {
    /// Build the VP8 `EncoderCfg`, open `Encoder::new(cfg, false)` and cache the
    /// YUV layout the capturer must convert into. The libvpx internal
    /// base-bitrate curve (per resolution) is the initial target; the pipeline
    /// calls `set_bitrate_kbps` right after open so the user-configured bitrate
    /// and the fps multiplier take effect immediately.
    ///
    /// `keyframe_secs` (#86) is the wall-clock period the capture loop enforces
    /// with `force_keyframe`; here it only sizes a frame-count **backstop** at
    /// twice that period. `kf_max_dist` cannot express a duration: it counts
    /// encoded frames, and a static desktop encodes `fps / repeat_every`
    /// repeats/s, so the same number is 1 s on one screen and 30 s on another.
    /// Keeping the backstop means a loop that somehow never reaches its own
    /// timer still cannot leave a pure P-frame chain open indefinitely.
    pub fn new(
        width: u32,
        height: u32,
        _bitrate_kbps: u32,
        keyframe_secs: u32,
        fps: u32,
        rc: RateControl,
        crf: u32,
        codec: VideoCodec,
    ) -> anyhow::Result<Self> {
        let backstop = keyframe_backstop_frames(fps, keyframe_secs);
        let cfg = EncoderCfg::VPX(VpxEncoderConfig {
            width: width as c_uint,
            height: height as c_uint,
            quality: 1.0,
            // #94: VP8 by default, VP9 when the panel picked it. Both branches
            // are the same `VpxEncoderConfig`; only the `vpx_codec_*_cx()` the
            // vendored wrapper picks differs.
            codec: codec.vpx_id(),
            // #62: 15 -> 60. `#61 experiment D` lowered this to 15 (~2 IDR/s at
            // 30 fps) to test the "broken reference chain" theory; the finding
            // was the opposite — at 1080p one IDR is 100-200 KB, so two per
            // second eat the entire bitrate budget (the quantizer then pins at
            // `q_max` and the picture blurs). The PLI path (debounced in
            // `pipeline`) re-anchors on demand instead of by brute force.
            // #86: the number is now derived from `--keyframe-secs` and is a
            // backstop only; see the doc comment above.
            keyframe_interval: Some(backstop),
            // #92: the panel's rate-control pick. `Cq` pins the quantizer and
            // treats the bitrate target as a ceiling; the value only takes
            // effect when the user chooses CRF, so the default path (CBR) is
            // byte-for-byte what it was before this key existed.
            rc: match rc {
                RateControl::Cbr => VpxRateControl::Cbr,
                RateControl::Cq => VpxRateControl::Cq,
            },
            cq_level: crf,
        });
        let encoder = Encoder::new(cfg, false)?;
        let yuvfmt = encoder.yuvfmt();
        log::info!(
            "{} encoder opened: {width}x{height} target={}kbps hw={} keyframe={}s (backstop {backstop} frames) rc={} crf={}",
            codec.label(),
            _bitrate_kbps,
            encoder.is_hardware(),
            keyframe_secs.clamp(1, 120),
            rc.as_str(),
            crf
        );
        Ok(Self {
            encoder,
            yuvfmt,
            width,
            height,
            rc,
            crf,
            codec,
        })
    }

    /// #94: the codec this encoder was opened with.
    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// #92: the mode this encoder was opened with.
    pub fn rate_control(&self) -> RateControl {
        self.rc
    }

    /// #92: the quantizer this encoder was opened with.
    pub fn crf(&self) -> u32 {
        self.crf
    }

    /// YUV layout the frame conversion must target this round.
    pub fn yuvfmt(&self) -> EncodeYuvFormat {
        self.yuvfmt.clone()
    }

    /// Encode one input; `Ok(None)` when libvpx emitted no packet this round
    /// (`VpxNoOutput`), `Ok(Some(frame))` otherwise. Other errors propagate.
    pub fn encode(
        &mut self,
        input: EncodeInput,
        pts_ms: i64,
    ) -> anyhow::Result<Option<VideoFrame>> {
        match self.encoder.encode_to_message(input, pts_ms) {
            Ok(frame) => Ok(Some(frame)),
            Err(e) if e.is::<VpxNoOutput>() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Internal encoder bitrate in kbps, for the stream_info / stats panel.
    pub fn bitrate(&self) -> u32 {
        self.encoder.bitrate()
    }

    /// Re-target the libvpx bitrate at `kbps`. The underlying `EncoderApi`
    /// exposes `set_quality(ratio)` (re-applied via `vpx_codec_enc_config_set`),
    /// so we translate the desired kbps into the equivalent ratio against the
    /// per-resolution base bitrate. Higher kbps -> higher ratio -> lower
    /// quantizers, which is exactly what a 60 fps session needs to avoid VP8
    /// block artifacts.
    pub fn set_bitrate_kbps(&mut self, kbps: u32) {
        // #92: under constant quality the quantizer is the user's decision and
        // the bitrate is only a ceiling — `set_quality` would recompute both
        // from a ratio and quietly undo the CRF. `set_target_bitrate` moves the
        // ceiling alone.
        if self.rc == RateControl::Cq {
            match self.encoder.set_target_bitrate(kbps) {
                Ok(()) => log::info!("encoder bitrate ceiling -> {kbps}kbps (crf mode)"),
                Err(e) => log::warn!("set_target_bitrate {kbps}kbps failed: {e:#}"),
            }
            return;
        }
        let base = scrap::codec::base_bitrate(self.width, self.height) as f32;
        let ratio = if base > 0.0 {
            (kbps as f32 / base).clamp(0.25, 8.0)
        } else {
            1.0
        };
        match self.encoder.set_quality(ratio) {
            Ok(()) => log::info!(
                "encoder bitrate -> {kbps}kbps (ratio {ratio:.2}, base {base:.0})"
            ),
            Err(e) => log::warn!("set_bitrate_kbps {kbps}kbps failed: {e:#}"),
        }
    }

    /// #53 PLI response: force the next encoded frame to be a keyframe so the
    /// browser can re-anchor its VP8 reference chain after a lost packet.
    pub fn force_keyframe(&mut self) {
        match self.encoder.force_keyframe() {
            Ok(()) => log::info!("encoder: forced keyframe requested"),
            Err(e) => log::warn!("force_keyframe failed: {e:#}"),
        }
    }

    /// Stage1 is always CPU software; retained for the stream_info field set.
    pub fn is_hardware(&self) -> bool {
        self.encoder.is_hardware()
    }
}

#[cfg(test)]
mod tests {
    use super::{keyframe_backstop_frames, VideoCodec};

    /// #92: the CQ configuration has to be accepted by libvpx *at open time* —
    /// this is the one thing that can fail silently when a client picks CRF,
    /// because a rejected config surfaces as "the encoder never opened".
    #[test]
    fn cq_encoder_opens_at_the_panel_bounds() {
        for crf in [5u32, 25, 50] {
            let enc = super::VideoEncoder::new(
                320,
                240,
                800,
                10,
                30,
                crate::pipeline::settings::RateControl::Cq,
                crf,
                VideoCodec::Vp8,
            )
            .unwrap_or_else(|e| panic!("CQ encoder failed to open at crf {}: {:#}", crf, e));
            assert_eq!(enc.rate_control(), crate::pipeline::settings::RateControl::Cq);
            assert_eq!(enc.crf(), crf);
        }
        let cbr = super::VideoEncoder::new(
            320,
            240,
            800,
            10,
            30,
            crate::pipeline::settings::RateControl::Cbr,
            25,
            VideoCodec::Vp8,
        )
        .expect("CBR encoder must keep opening exactly as before #92");
        assert_eq!(cbr.rate_control(), crate::pipeline::settings::RateControl::Cbr);
    }

    /// #94: the second codec has to be *really* openable, not just advertised.
    /// This is the whole risk of the feature — a codec listed in the menu whose
    /// `vpx_codec_vp9_cx()` fails on this build would turn a pick into a dead
    /// session, and the failure would surface much later as "no video".
    #[test]
    fn vp9_opens_and_reports_itself() {
        let enc = super::VideoEncoder::new(
            320,
            240,
            800,
            10,
            30,
            crate::pipeline::settings::RateControl::Cbr,
            25,
            VideoCodec::Vp9,
        )
        .expect("libvpx must build a VP9 encoder in this binary");
        assert_eq!(enc.codec(), VideoCodec::Vp9);
        assert!(!enc.is_hardware(), "VP9 here is the software libvpx one");
    }

    #[test]
    fn backstop_is_twice_the_wall_clock_period() {
        // 30 fps * 10 s * 2 = 600 encoded frames. The loop's timer fires at
        // 300 frames' worth of *wall clock*, so the libvpx net can only ever be
        // a safety net — never the thing that decides when an IDR goes out.
        assert_eq!(keyframe_backstop_frames(30, 10), 600);
        assert_eq!(keyframe_backstop_frames(60, 10), 1200);
    }

    #[test]
    fn backstop_tracks_the_configured_seconds() {
        assert_eq!(keyframe_backstop_frames(60, 2), 240);
        assert_eq!(keyframe_backstop_frames(60, 120), 14_400);
    }

    #[test]
    fn backstop_has_a_floor_and_a_ceiling() {
        // 1 fps * 1 s * 2 would be 2 frames: an IDR every other frame. The floor
        // keeps the net from becoming the authority on a tiny-fps session.
        assert_eq!(keyframe_backstop_frames(1, 1), 60);
        // 240 fps * 120 s * 2 = 57 600, under the ceiling; the ceiling only
        // guards against a caller passing absurd values through the clamp.
        assert_eq!(keyframe_backstop_frames(240, 120), 57_600);
    }

    #[test]
    fn backstop_survives_garbage_input() {
        // `Config` can be built as a struct literal, so 0 must not panic or
        // produce a zero budget (libvpx treats 0 as "use the default 128").
        // A rate of 0 is the panel's "no limit" (`settings::FPS_UNCAPPED`), and
        // it is budgeted at the ceiling, not at one frame per second — the
        // latter would drop an IDR every other frame on an uncapped session.
        // The seconds keep their own floor of 1.
        assert_eq!(keyframe_backstop_frames(0, 0), 480);
        assert_eq!(keyframe_backstop_frames(1, 1), 60);
    }
}
