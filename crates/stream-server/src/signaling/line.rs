//! Signaling WS line parser / serializer (design 5.2, 5.3).
//!
//! Channel split (verified against frontend source):
//! * WS socket (`/api/webrtc/signaling/`): handshake lines plus every SDP and ICE
//!   exchange (signaling.js:520-538). The browser sends its answer and local
//!   candidates here, never on a data channel.
//! * Data channels: downstream `pipeline` / `stream_info` / `stream_stats` /
//!   `connection` JSON and upstream control/input command lines. This module owns
//!   WS lines only; DC message parsing lives in `dispatch`.
//!
//! Handshake lines:
//! * C->S `HELLO client <meta-json>`
//! * S->C `HELLO`
//! * C->S `SESSION server`
//! * S->C `SESSION_OK <server_peer_id>`
//! * S<->C `<peer_id> {"sdp":{"type":"offer|answer","sdp":"..."}}`
//! * S<->C `<peer_id> {"ice":{"candidate":"...","sdpMid":"0","sdpMLineIndex":0,...}}`
//!
//! Peer id: stage1 fixed `"1"` to match the Python service the frontend already
//! accepts (signaling.js:383 caches whatever `SESSION_OK` reports).

use hbb_common::serde_json::{self, Value};

/// Fixed server-side peer id used in `SESSION_OK` and as the outbound line prefix.
pub const SERVER_PEER_ID: &str = "1";

/// A parsed inbound WS line.
#[derive(Debug, Clone)]
pub enum IncomingLine {
    /// `HELLO <peer_type> <meta-json>`; meta may fail to parse and stays None.
    Hello {
        peer_type: String,
        meta: Option<Value>,
    },
    /// `SESSION <target>`; target is the requested peer, e.g. `server`.
    SessionRequest { target: String },
    /// `<id> {"sdp":{type,sdp}}`.
    Sdp {
        peer_id: String,
        sdp_type: String,
        sdp: String,
    },
    /// `<id> {"ice":{candidate,...}}`.
    Ice {
        peer_id: String,
        candidate: Value,
    },
    /// Anything not recognized; logged at debug and ignored without breaking
    /// the socket.
    Unrecognized(String),
}

/// Parse one inbound WS text line. Malformed JSON never aborts the loop.
pub fn parse_ws_line(line: &str) -> IncomingLine {
    let line = line.trim_end();
    let mut parts = line.splitn(3, ' ');
    let head = parts.next().unwrap_or("");

    match head {
        "HELLO" => {
            let peer_type = parts.next().unwrap_or("").to_owned();
            let meta = parts.next().and_then(|m| serde_json::from_str::<Value>(m).ok());
            IncomingLine::Hello { peer_type, meta }
        }
        "SESSION" => IncomingLine::SessionRequest {
            target: parts.next().unwrap_or("").to_owned(),
        },
        _ => {
            // `<peer_id> {json}`: split the first token as the id, parse the rest.
            let Some((peer_id, rest)) = line.split_once(' ') else {
                return IncomingLine::Unrecognized(line.to_owned());
            };
            let Ok(msg) = serde_json::from_str::<Value>(rest) else {
                return IncomingLine::Unrecognized(line.to_owned());
            };
            if let Some(sdp) = msg.get("sdp") {
                let sdp_type = sdp
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let sdp_text = sdp.get("sdp").and_then(|v| v.as_str()).unwrap_or("").to_owned();
                IncomingLine::Sdp {
                    peer_id: peer_id.to_owned(),
                    sdp_type,
                    sdp: sdp_text,
                }
            } else if let Some(ice) = msg.get("ice") {
                IncomingLine::Ice {
                    peer_id: peer_id.to_owned(),
                    candidate: ice.clone(),
                }
            } else {
                IncomingLine::Unrecognized(line.to_owned())
            }
        }
    }
}

/// `HELLO` registration acknowledgement sent right after the client's HELLO.
pub fn hello_ack() -> String {
    "HELLO".to_owned()
}

/// `SESSION_OK <peer_id>` confirming the session with the server peer.
pub fn session_ok() -> String {
    format!("SESSION_OK {SERVER_PEER_ID}")
}

/// `<id> {"sdp":{type,sdp}}` outbound line for an offer or answer.
pub fn sdp_line(peer_id: &str, sdp_type: &str, sdp: &str) -> String {
    let body = serde_json::json!({
        "sdp": { "type": sdp_type, "sdp": sdp }
    });
    format!("{peer_id} {body}")
}

/// `<id> {"ice":{...}}` outbound line for a locally gathered candidate.
pub fn ice_line(peer_id: &str, candidate_json: &Value) -> String {
    let body = serde_json::json!({ "ice": candidate_json });
    format!("{peer_id} {body}")
}
