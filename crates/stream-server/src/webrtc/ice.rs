//! Local / remote ICE candidate exchange over the signaling line (design 6.4).
//!
//! Local candidate: inside `on_ice_candidate`, `candidate.to_json()` yields an
//! `RTCIceCandidateInit`; the session serializes it and the signaling layer
//! wraps it in `<peer_id> {"ice": {...}}`. The `None` (gathering complete)
//! callback sends no empty line.
//!
//! Remote candidate: parse the same object from `{"ice": {...}}`; if the
//! remote description has not been applied yet, queue it and apply after
//! `set_remote_description(answer)`; otherwise call `add_ice_candidate` straight
//! away (peer_connection/mod.rs:1651).
//!
//! The 0.13 field mapping: `RTCIceCandidateInit { candidate, sdp_mid,
//! sdp_mline_index, username_fragment }`; JSON keys are camelCase with
//! `sdpMLineIndex` renamed separately.

use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

/// Remote-side candidate buffered until the remote description is applied.
#[derive(Debug, Clone, Default)]
pub struct RemoteCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_mline_index: Option<u16>,
    pub username_fragment: Option<String>,
}

/// Stage6.4: deserialize the inbound ice object into a queueable candidate.
/// Returns `None` when the required `candidate` field is missing or empty.
pub fn remote_from_json(value: &hbb_common::serde_json::Value) -> Option<RemoteCandidate> {
    let candidate = value.get("candidate")?.as_str()?.to_owned();
    Some(RemoteCandidate {
        candidate,
        sdp_mid: value
            .get("sdpMid")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        sdp_mline_index: value
            .get("sdpMLineIndex")
            .and_then(|v| v.as_u64())
            .map(|v| v as u16),
        username_fragment: value
            .get("usernameFragment")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
    })
}

/// Convert the parsed candidate into the init struct `add_ice_candidate`
/// consumes.
pub(crate) fn to_init(c: &RemoteCandidate) -> RTCIceCandidateInit {
    RTCIceCandidateInit {
        candidate: c.candidate.clone(),
        sdp_mid: c.sdp_mid.clone(),
        sdp_mline_index: c.sdp_mline_index,
        username_fragment: c.username_fragment.clone(),
    }
}
