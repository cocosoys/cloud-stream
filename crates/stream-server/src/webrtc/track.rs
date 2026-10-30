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
//! #94: the mime type is now the pipeline's pick (`pipeline::codec`), not a
//! constant. That single string is what the whole downlink hinges on — it
//! selects the SDP `a=rtpmap`, and `TrackLocalStaticSample::bind` asks the
//! capability for its payloader (`payloader_for_codec`, webrtc-rs
//! `rtp_codec.rs:70`), so `video/VP9` here is what makes the sender packetize
//! VP9 frames with a VP9 descriptor instead of a VP8 one. Get it wrong and the
//! browser decodes noise; the two have no common framing.
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
use webrtc::api::media_engine::{MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9};
use webrtc::media::Sample;
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::pipeline::codec::VideoCodec;

/// The four RTCP feedback entries the default VP8/VP9 payloaders understand
/// (goog-remb, ccm/fir, nack, nack/pli). Both codecs take the same set.
fn video_feedback() -> Vec<RTCPFeedback> {
    vec![
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
    ]
}

/// Build the capability for `codec` with the entries the default payloader
/// understands.
///
/// The VP8 arm is byte-for-byte what this function produced before #94: same
/// mime, same clock rate, same empty fmtp. Anything else would change the offer
/// for every existing session, which is exactly the kind of "while I was in
/// there" edit this project keeps paying for.
fn video_capability(codec: VideoCodec) -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: match codec {
            VideoCodec::Vp8 => MIME_TYPE_VP8.to_owned(),
            VideoCodec::Vp9 => MIME_TYPE_VP9.to_owned(),
        },
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: codec.sdp_fmtp().to_owned(),
        rtcp_feedback: video_feedback(),
    }
}

/// Wraps the Arc video track handed to the capture loop for `write_sample`.
#[derive(Clone)]
pub struct VideoTrack {
    inner: Arc<TrackLocalStaticSample>,
}

/// #77: the game-sound downlink track.
///
/// A separate `TrackLocalStaticSample` rather than a second SSRC on the video
/// one: audio and video have different clocks, different codecs and different
/// loss budgets, and WebRTC's own bundling already shares the transport.
///
/// Capability note — `stereo=1` and `minptime=10` are not decoration: the
/// frontend reads them (`webrtc.js:31-34`) to decide how it may reshape the
/// Opus line in its answer, and an omitted `stereo=1` makes Chrome downmix to
/// mono, which silently costs the player positional cues.
#[derive(Clone)]
pub struct AudioTrack {
    inner: Arc<TrackLocalStaticSample>,
}

impl AudioTrack {
    pub fn new() -> Result<Self> {
        let track = TrackLocalStaticSample::new(
            opus_capability(),
            "audio-0".to_owned(),
            "stream-0".to_owned(),
        );
        Ok(Self {
            inner: Arc::new(track),
        })
    }

    pub fn as_track_local(&self) -> Arc<dyn TrackLocal + Send + Sync> {
        Arc::clone(&self.inner) as Arc<dyn TrackLocal + Send + Sync>
    }

    /// Write one Opus packet. Duration is always one frame (20 ms): unlike
    /// video, an audio sample must carry the real frame length or the
    /// packetizer's RTP timestamps drift against the device clock.
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

/// Opus capability: 48 kHz stereo, in-band FEC on, no RTCP feedback.
///
/// Audio deliberately carries no NACK/PLI: a retransmitted audio packet is
/// worthless by the time it arrives (the FEC copy already covered the gap), and
/// asking for one only adds RTT to a stream that must stay continuous.
fn opus_capability() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48_000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

impl VideoTrack {
    /// Stage6.2: build the capability and construct the track. The owning
    /// session registers it on the PC with `as_track_local`.
    ///
    /// #94: `codec` has to be the *same* pick the capture loop opens its
    /// encoder with (`pipeline::codec::current()`). A track that advertises VP8
    /// while the encoder produces VP9 packets is not a degraded stream, it is a
    /// broken one: the payload descriptor and the decoder disagree.
    pub fn new(codec: VideoCodec) -> Result<Self> {
        let track = TrackLocalStaticSample::new(
            video_capability(codec),
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
