//! VP8 encoder wrapper over `scrap::codec::Encoder` (design 8.1 rows 2-3).
//!
//! Stage1 fixed settings (design 4.4 / 8.1):
//! * codec: VP8; stage3 re-evaluates VP9.
//! * quality: 1.0; `keyframe_interval: Some(60)` (#53): a pure P-frame chain
//!   (`VPX_KF_DISABLED`) drifts — quantizer error accumulates against the
//!   reference, the picture progressively darkens / textures up and encode time
//!   grows (fps drops). A periodic keyframe re-anchors the reference: 60 frames
//!   = ~2 s at 30 fps, ~1 s at 60 fps. Combined with frame-dropping disabled
//!   (`rc_dropframe_thresh = 0`) the receiver's reference chain is re-anchored
//!   quickly after any loss; PLI-driven keyframes land in stage 3 QoS.
//! * i444: false (fullcolor = false in `stream_info`).
//! * No recorder, no hardware fallback chain, no H264/H265/AV1 negotiation.
//!
//! Reused RustDesk/scrap symbols (0e bench chain already compiled through):
//! * `scrap::codec::{Encoder, EncoderCfg}`
//! * `scrap::{VpxEncoderConfig, VpxVideoCodecId, EncodeInput, EncodeYuvFormat, VpxNoOutput}`
//! * `encoder.yuvfmt()`, `encoder.encode_to_message(input, ms)`, `encoder.bitrate()`.
//!
//! Encoded frames arrive as `base::message_proto::VideoFrame` carrying
//! `union=Vp8s(EncodedVideoFrames{frames})`; slicing into RTP-ready chunks lives
//! in `pipeline::sample_map`. `VpxNoOutput` (this round produced no packet) is
//! normalized to `Ok(None)` so the loop counts it as `encode_no_output` without
//! treating it as a fatal encoder error.

use std::ffi::c_uint;

use base::message_proto::VideoFrame;
use scrap::codec::{Encoder, EncoderCfg};
use scrap::{EncodeInput, EncodeYuvFormat, VpxEncoderConfig, VpxNoOutput, VpxVideoCodecId};

/// Thin lifecycle wrapper the capture loop holds between opens.
pub struct VideoEncoder {
    encoder: Encoder,
    yuvfmt: EncodeYuvFormat,
    width: u32,
    height: u32,
}

impl VideoEncoder {
    /// Build the VP8 `EncoderCfg`, open `Encoder::new(cfg, false)` and cache the
    /// YUV layout the capturer must convert into. The libvpx internal
    /// base-bitrate curve (per resolution) is the initial target; the pipeline
    /// calls `set_bitrate_kbps` right after open so the user-configured bitrate
    /// and the fps multiplier take effect immediately.
    pub fn new(width: u32, height: u32, _bitrate_kbps: u32) -> anyhow::Result<Self> {
        let cfg = EncoderCfg::VPX(VpxEncoderConfig {
            width: width as c_uint,
            height: height as c_uint,
            quality: 1.0,
            codec: VpxVideoCodecId::VP8,
            // #62: 15 -> 60. `#61 experiment D` lowered this to 15 (~2 IDR/s at
            // 30 fps) to test the "broken reference chain" theory; the finding
            // was the opposite — at 1080p one IDR is 100-200 KB, so two per
            // second eat the entire bitrate budget (the quantizer then pins at
            // `q_max` and the picture blurs). 60 frames = ~2 s at 30 fps, ~1 s
            // at 60 fps, and the PLI path (debounced in `pipeline`) re-anchors
            // on demand instead of by brute force.
            keyframe_interval: Some(60),
        });
        let encoder = Encoder::new(cfg, false)?;
        let yuvfmt = encoder.yuvfmt();
        log::info!(
            "VP8 encoder opened: {width}x{height} target={}kbps hw={}",
            _bitrate_kbps,
            encoder.is_hardware()
        );
        Ok(Self {
            encoder,
            yuvfmt,
            width,
            height,
        })
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
