//! Encoded packet -> `webrtc::media::Sample` mapping (design 6.3, 8.1 row 5).
//!
//! The scrap encoder returns a `base::message_proto::VideoFrame`; for VP8 the
//! oneof carries `Vp8s(EncodedVideoFrames { frames: Vec<EncodedVideoFrame> })`,
//! each frame `{ data: bytes::Bytes, key: bool, pts: i64 }`. A single captured
//! frame may split into several RTP payload chunks; each chunk becomes one
//! `Sample` handed to the `TrackLocalStaticSample`.
//!
//! Stage1 fixed rules (design 6.3):
//! * `Sample.data` = the chunk bytes; `Sample.duration` is always exactly `spf`
//!   (design 9.2 #4), even on a repeat frame.
//! * The track packetizer owns RTP sequence numbers and media clock; `pts` here
//!   is kept for diagnostics only.
//! * The `key` flag decides whether a chunk belongs to a keyframe; the `Sample`
//!   struct itself has no keyframe field.
//! * VP8 and (#94) VP9 are accepted; every other union returns an empty list,
//!   because this pipeline has no encoder for it.

use base::message_proto::{video_frame, VideoFrame};
use bytes::Bytes;

/// One RTP-ready chunk prepared for the track.
#[derive(Debug, Clone)]
pub struct EncodedChunk {
    /// VP8 payload bytes (prost bytes maps to `bytes::Bytes`).
    pub data: Bytes,
    /// True when this chunk belongs to a keyframe.
    pub key: bool,
    /// Encoder presentation timestamp in the encoder's own clock (diagnostics).
    pub pts: u32,
}

/// Split an encoder `VideoFrame` into VP8 chunks. Returns an empty list for any
/// non-VP8 union (stage1 never negotiates those codecs).
pub fn video_frame_to_chunks(vf: VideoFrame) -> Vec<EncodedChunk> {
    let Some(union) = vf.union else {
        return Vec::new();
    };
    match union {
        video_frame::Union::Vp8s(frames) => chunks_of(frames),
        // #94: the VP9 encoder emits the same `EncodedVideoFrames` shape in a
        // different oneof arm. Dropping it here would have been a silent black
        // screen: the encoder would produce frames, the loop would count
        // `encoded_ok`, and nothing would ever reach the track.
        video_frame::Union::Vp9s(frames) => chunks_of(frames),
        other => {
            log::debug!("drop encoder union this pipeline cannot send: {:?}", codec_tag(&other));
            Vec::new()
        }
    }
}

/// One frame list -> one chunk list. Shared by the VP8 and VP9 arms: both carry
/// `Vec<EncodedVideoFrame>` with the same fields, and the RTP payloader (not
/// this function) is what knows the codec's descriptor layout.
fn chunks_of(frames: base::message_proto::EncodedVideoFrames) -> Vec<EncodedChunk> {
    frames
        .frames
        .into_iter()
        .map(|f| EncodedChunk {
            data: f.data,
            key: f.key,
            pts: f.pts as u32,
        })
        .collect()
}

/// Tiny discriminator tag for the debug line above; avoids Display on the oneof.
fn codec_tag(union: &video_frame::Union) -> &'static str {
    match union {
        video_frame::Union::Vp8s(_) => "vp8s",
        video_frame::Union::Vp9s(_) => "vp9s",
        video_frame::Union::H264s(_) => "h264s",
        video_frame::Union::H265s(_) => "h265s",
        video_frame::Union::Av1s(_) => "av1s",
        video_frame::Union::Rgb(_) => "rgb",
        video_frame::Union::Yuv(_) => "yuv",
        _ => "other",
    }
}
