//! Per-session handshake state and context (design 4.1, 4.2).
//!
//! Stage1 state machine:
//! * `New` — socket accepted, nothing exchanged yet.
//! * `HelloSent` — inbound `HELLO client <meta>` answered with `HELLO`; the
//!   client then sends `SESSION server`.
//! * `SessionReady` — `SESSION_OK 1` sent; the PC, two DCs and VP8 track are
//!   about to be built (real assembly in task #34/#35).
//! * `OfferSent` — local offer serialized and pushed over WS; awaiting the answer.
//! * `AnswerApplied` — answer accepted; remote queued candidates are flushed.
//! * `Connected` — PC state callback reached Connected; pipeline notice,
//!   one-shot `stream_info` and healthy `connection` are sent.
//! * `Streaming` — `START_VIDEO` arrived; the capture loop writes samples.
//! * `Closed` — terminal: loop stopped, stats timer stopped, DCs and PC closed,
//!   WS closed (design 4.2 cleanup order).
//!
//! `Disconnected`/`Failed` on the PC are treated as terminal for stage1; the
//! transient ICE Disconnected state is not modeled separately (hbb_common
//! webrtc.rs:789-834 pattern). Real MediaSession references are bound in task #35.

use hbb_common::serde_json::Value;

/// Handshake progression of one browser session. Terminal / streaming variants
/// and `is_connected` are reserved for the #37 state audit; stage1 transitions
/// through New -> HelloSent -> SessionReady -> OfferSent -> AnswerApplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum HandshakeState {
    New,
    HelloSent,
    SessionReady,
    OfferSent,
    AnswerApplied,
    Connected,
    Streaming,
    Closed,
}

impl Default for HandshakeState {
    fn default() -> Self {
        Self::New
    }
}

/// Mutable session context shared between the WS loop, the PC callback and the
/// capture gate. Stage1 holds only protocol state and the raw HELLO meta here;
/// the PC / DC / track handles and the live gate flags live in the WS task
/// context (`websession::SessionCtx`) behind their own `Arc` handles.
#[derive(Debug, Clone, Default)]
pub struct SignalingSession {
    pub state: HandshakeState,
    /// Meta object captured from the inbound HELLO; stage1 ignores client token,
    /// display id and resize fields but keeps the raw value for logs.
    pub meta: Option<Value>,
}

impl SignalingSession {
    pub fn new() -> Self {
        Self::default()
    }

    /// Move to the given state; record the transition only when it changes.
    pub fn transition(&mut self, next: HandshakeState) {
        if self.state != next {
            log::info!("signaling session state {:?} -> {:?}", self.state, next);
            self.state = next;
        }
    }

    /// True once the PC callback reports Connected and outbound notices may go
    /// out; gates `stream_info` so it is never sent before the PC is live.
    /// Reserved for the #37 state audit; stage1 notices fire from the PC state
    /// callback instead.
    #[allow(dead_code)]
    pub fn is_connected(&self) -> bool {
        matches!(self.state, HandshakeState::Connected | HandshakeState::Streaming)
    }
}
