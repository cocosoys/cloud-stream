//! Signaling layer: one browser WS session, handshake state machine and SDP/ICE
//! exchange over the socket (design 4.1, 5.2, 6.4).
//!
//! The WS loop, session context and media-session wiring live in `websession`;
//! this module re-exports the parser, state shape and the session registry so
//! `http` and `main` compile against a fixed surface.

pub mod latency;
pub mod line;
pub mod session;
pub mod websession;

pub use latency::{latency_measurement, LatencyProbe, PING_INTERVAL};
pub use line::{parse_ws_line, hello_ack, session_ok, sdp_line, ice_line, IncomingLine, SERVER_PEER_ID};
pub use session::{HandshakeState, SignalingSession};
pub use websession::{ClientMeta, SessionInfo, SessionRegistry};
// #110: the WS media transport asks the same two questions the WebRTC control
// channel does — "which policy surface is this?" and "may a viewer run it?" —
// so they are shared rather than reimplemented next to the other socket. Two
// copies would drift the moment a new `Action` variant lands.
pub(crate) use websession::{surface_of, viewer_may_run};
