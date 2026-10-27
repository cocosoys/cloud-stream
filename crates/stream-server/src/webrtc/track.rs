//! Downlink video track (design 6.2, 6.3).
//!
//! Stage6.2 constructs the VP8 capability and the `TrackLocalStaticSample`:
//! * mime_type `video/VP8`, clock_rate 90000, channels 0, empty fmtp;
//!   rtcp_feedback keeps the four built-in entries goog-remb, ccm/fir, nack,
//!   nack/pli. `RTCPFeedback` has no constructor in this fork: it exposes the
//!   two public fields `typ` / `parameter` directly (parameter is empty when
//!   the feedback type carries no sub-option).
//! * `TrackLocalStaticSample::new(vp8, "video-0", "stream-0")`; the Arc is
//!   handed to the PC via `add_track` as `Arc<dyn TrackLocal + Send + Sync>`
//!   (examples/play-from-disk-h264.rs:157-159 pattern).
//!
//! Stage6.3 maps each encoded chunk to a `webrtc::media::Sample`:
//! * duration always equals spf (repeat frames included, design 9.2 #4);
//! * timestamp `SystemTime::now()`, packet_timestamp 0; the internal packetizer
//!   derives RTP timestamps from duration x clock_rate once bound;
//! * write_sample errors surface to the caller, which counts and logs them in
//!   the capture loop instead of blocking the beat.
//!
//! The Sample struct has no keyframe flag; `frame.key` only warms the
//! first-keyframe bookkeeping.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use bytes::Bytes;
use webrtc::api::media_engine::MIME_TYPE_VP8;
use webrtc::media::Sample;
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

/// Build the VP8 capability with the four RTCP feedback entries the default
/// VP8 payloader understands (goog-remb, ccm/fir, nack, nack/pli).
fn vp8_capability() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_VP8.to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: String::new(),
        rtcp_feedback: vec![
            RTCPFeedback {
                typ: "goog-remb".to_owned(),
                parameter: String::new(),
            },
            RTCPFeedback {
                typ: "ccm".to_owned(),
                parameter: "fir".to_owned(),
            },
            RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: String::new(),
            },
            RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: "pli".to_owned(),
            },
        ],
    }
}

/// Wraps the Arc video track handed to the capture loop for `write_sample`.
#[derive(Clone)]
pub struct VideoTrack {
    inner: Arc<TrackLocalStaticSample>,
}

impl VideoTrack {
    /// Stage6.2: build the capability and construct the track. The owning
    /// session registers it on the PC with `as_track_local`.
    pub fn new() -> Result<Self> {
        let track = TrackLocalStaticSample::new(
            vp8_capability(),
            "video-0".to_owned(),
            "stream-0".to_owned(),
        );
        Ok(Self {
            inner: Arc::new(track),
        })
    }

    /// Erase the concrete Arc type for `add_track`'s
    /// `Arc<dyn TrackLocal + Send + Sync>` parameter.
    pub fn as_track_local(&self) -> Arc<dyn TrackLocal + Send + Sync> {
        Arc::clone(&self.inner) as Arc<dyn TrackLocal + Send + Sync>
    }

    /// Stage6.3: assemble one `Sample` for an encoded VP8 chunk and write it to
    /// the track. Before the track is bound, the library returns `Ok`
    /// silently (track_local_static_sample.rs:87-89); after binding the packetizer
    /// splits the chunk into RTP packets.
    pub async fn write_sample(&self, data: Bytes, duration: Duration) -> Result<()> {
        let sample = Sample {
            data,
            timestamp: SystemTime::now(),
            duration,
            packet_timestamp: 0,
            prev_dropped_packets: 0,
            prev_padding_packets: 0,
        };
        self.inner.write_sample(&sample).await?;
        Ok(())
    }
}
