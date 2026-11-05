//! #94: which video codec the server offers.
//!
//! Until this module existed the answer was a constant: `pipeline::encoder`
//! built `VpxVideoCodecId::VP8` and `webrtc::track` advertised `video/VP8`, so
//! the panel's encoder row could only report a fact. libvpx ships **both**
//! encoders in this binary already (the vendored `scrap` picks
//! `vpx_codec_vp8_cx()` / `vpx_codec_vp9_cx()` from the same config struct), and
//! webrtc-rs has a VP9 payloader, so the second choice costs an SDP line and a
//! rebuild — not a new dependency.
//!
//! Why the choice is per program, not per session or per process
//! -------------------------------------------------------------
//! The codec is decided **before negotiation**: it is the m-line's `a=rtpmap`
//! and the payloader the track binds to. There is no renegotiation path in this
//! pipeline (the PC is built once per session), so switching codecs is done the
//! only way that does not need one: the server records the pick, and the session
//! is closed so the client — which reconnects on its own — negotiates again with
//! the new codec. That means the pick has to outlive the session that carried
//! it.
//!
//! It lives on [`crate::pipeline::settings::StreamSettings`], which is owned by
//! the **program** (`SharedPipeline.settings`): created once per display,
//! outliving every session attached to it. That satisfies "outlives the session"
//! without the process-wide atomic this module used to hold.
//!
//! #110 — why it moved off the process
//! -----------------------------------
//! A process-wide atomic meant one viewer's pick changed the codec for every
//! screen on the machine, then kicked every session (`codec_registry.kick`) to
//! renegotiate. That is not a settings panel, that is a broadcast. With two
//! displays, or two organisations on one host, "my encoder" has to be a real
//! thing before any isolation can be built on top of it.
//!
//! What this is not
//! ----------------
//! Not a per-connection negotiation: one program, one codec, every session on
//! that screen gets it. Not persisted: a restart returns to `--video-codec`
//! (default VP8), which only seeds the first program.
//!
//! Cost of the second choice: VP9 costs more CPU at the same resolution (libvpx
//! VP9 real-time is roughly 2x VP8 here) and buys sharper text at the same
//! bitrate. That trade is the user's to make, so both are offered and neither is
//! hidden.

use scrap::VpxVideoCodecId;

/// The video codecs this pipeline can actually produce end to end.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum VideoCodec {
    /// The default: cheapest to encode, what every browser that ships WebRTC
    /// can decode.
    #[default]
    Vp8,
    /// Sharper per bit, ~2x the encode cost.
    Vp9,
}

impl VideoCodec {
    /// In panel order. VP8 first because it is the default and the fallback.
    pub const ALL: [VideoCodec; 2] = [VideoCodec::Vp8, VideoCodec::Vp9];

    /// The name on the wire (`video_codec` in the health payload, `--video-codec`).
    pub fn wire(self) -> &'static str {
        match self {
            VideoCodec::Vp8 => "vp8",
            VideoCodec::Vp9 => "vp9",
        }
    }

    pub fn from_wire(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "vp8" => Some(VideoCodec::Vp8),
            "vp9" => Some(VideoCodec::Vp9),
            _ => None,
        }
    }

    /// The id the panel's encoder row uses (`server_settings.encoder`).
    ///
    /// The frontend's own vocabulary: `encoderOptionsWR` is
    /// `["h264enc", "vp8enc", "vp9enc", "av1enc"]` and `displayLabel` turns
    /// `vp9enc` into "VP9". Anything else would need a frontend change to get a
    /// label, and a codec nobody can name in the menu is not a choice.
    pub fn encoder_id(self) -> &'static str {
        match self {
            VideoCodec::Vp8 => "vp8enc",
            VideoCodec::Vp9 => "vp9enc",
        }
    }

    pub fn from_encoder_id(id: &str) -> Option<Self> {
        match id.trim().to_ascii_lowercase().as_str() {
            "vp8enc" | "vp8" => Some(VideoCodec::Vp8),
            "vp9enc" | "vp9" => Some(VideoCodec::Vp9),
            _ => None,
        }
    }

    /// Shown in `stream_info.codec` and the statistics tile.
    pub fn label(self) -> &'static str {
        match self {
            VideoCodec::Vp8 => "VP8",
            VideoCodec::Vp9 => "VP9",
        }
    }

    /// The libvpx encoder inside the vendored `scrap`.
    pub fn vpx_id(self) -> VpxVideoCodecId {
        match self {
            VideoCodec::Vp8 => VpxVideoCodecId::VP8,
            VideoCodec::Vp9 => VpxVideoCodecId::VP9,
        }
    }

    /// The `a=fmtp` line this server offers for the codec.
    ///
    /// VP9 needs `profile-id=0` spelled out: it is the 8-bit 4:2:0 profile
    /// libvpx encodes by default, and it is one of the two entries
    /// `MediaEngine::register_default_codecs` publishes (payload 98 is profile
    /// 0, payload 100 is profile 1). Omitting it still negotiates — the fuzzy
    /// matcher falls back to mime + clock rate — but then a receiver could
    /// reasonably expect profile 1 and decode garbage.
    pub fn sdp_fmtp(self) -> &'static str {
        match self {
            VideoCodec::Vp8 => "",
            VideoCodec::Vp9 => "profile-id=0",
        }
    }

    /// The `u8` the program's atomic stores: 0 = VP8, 1 = VP9.
    ///
    /// A `u8` rather than a `Mutex<VideoCodec>` because the read happens on the
    /// capture loop's hot path (every beat, to notice a pick and rebuild) and
    /// the write is a panel click.
    pub fn as_byte(self) -> u8 {
        match self {
            VideoCodec::Vp8 => 0,
            VideoCodec::Vp9 => 1,
        }
    }

    /// Inverse of [`Self::as_byte`]. Anything that is not 1 is VP8, so a
    /// corrupted or future byte degrades to the codec every browser decodes.
    pub fn from_byte(byte: u8) -> Self {
        if byte == 1 {
            VideoCodec::Vp9
        } else {
            VideoCodec::Vp8
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panel's ids and the wire names have to agree in both directions, or
    /// a pick that arrives as `vp9enc` would silently decode as VP8 and the
    /// panel would report a codec the stream is not using.
    #[test]
    fn panel_ids_and_wire_names_round_trip() {
        for codec in VideoCodec::ALL {
            assert_eq!(VideoCodec::from_encoder_id(codec.encoder_id()), Some(codec));
            assert_eq!(VideoCodec::from_wire(codec.wire()), Some(codec));
        }
        assert_eq!(VideoCodec::from_encoder_id("VP9ENC"), Some(VideoCodec::Vp9));
        assert_eq!(VideoCodec::from_wire(" VP8 "), Some(VideoCodec::Vp8));
    }

    /// Codecs this build cannot encode must not be accepted as a pick: H.264
    /// would need a parameter-set-capable payloader and AV1 an encoder libvpx
    /// does not have. A silent accept is the failure mode that hurts — the
    /// panel would show a codec nothing produces.
    #[test]
    fn unsupported_codecs_are_refused() {
        for name in ["h264enc", "av1enc", "h265enc", "jpeg", ""] {
            assert_eq!(VideoCodec::from_encoder_id(name), None, "{name}");
            assert_eq!(VideoCodec::from_wire(name), None, "{name}");
        }
    }

    /// The atomic byte has to round-trip, and an unknown byte must degrade to
    /// the codec every browser decodes rather than to a stream nobody can play.
    #[test]
    fn the_stored_byte_round_trips_and_fails_safe() {
        for codec in VideoCodec::ALL {
            assert_eq!(VideoCodec::from_byte(codec.as_byte()), codec);
        }
        for junk in [2u8, 7, 255] {
            assert_eq!(VideoCodec::from_byte(junk), VideoCodec::Vp8, "{junk}");
        }
    }

    /// VP8 stays the default: an upgrade must not silently move every session
    /// onto the more expensive encoder.
    #[test]
    fn the_default_is_the_cheap_codec() {
        assert_eq!(VideoCodec::default(), VideoCodec::Vp8);
        assert_eq!(VideoCodec::Vp8.sdp_fmtp(), "");
        assert_eq!(VideoCodec::Vp9.sdp_fmtp(), "profile-id=0");
    }
}
