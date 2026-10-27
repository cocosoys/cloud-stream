//! Data channels created by the server (design 6.2, 5.6).
//!
//! Two channels, opened on the server side before `create_offer`:
//! * `control`: ordered, fully reliable (default init). All whitelisted outbound
//!   JSON (`pipeline`, `stream_info`, `stream_stats`, `connection`) rides this
//!   channel; the frontend stores every non-`pointer` label in `_send_channel`
//!   (webrtc.js:610-615).
//! * `pointer`: ordered = false, max_retransmits = 0 for motion messages;
//!   stage1 sends nothing on it and leaves it attached to match the frontend
//!   branch.
//!
//! No reassembly lives here: reliable `control` keeps fragment ordering; stage1
//! emits whole JSON lines only. The session wires `control.on_message` in task
//! #35 once the upstream command classifier is connected.

use std::sync::Arc;

use anyhow::Result;
use log;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::peer_connection::RTCPeerConnection;

/// Holds the two channel handles the session sends on.
pub struct ControlChannels {
    control: Arc<RTCDataChannel>,
    pointer: Arc<RTCDataChannel>,
}

impl ControlChannels {
    /// Stage6.2: `create_data_channel("control", ordered=true)` then
    /// `create_data_channel("pointer", ordered=false, max_retransmits=0)`.
    /// Open / close handlers only log at this stage; `control.on_message` is
    /// attached later by the signaling task (#35).
    pub async fn open(pc: &RTCPeerConnection) -> Result<Self> {
        let control = pc
            .create_data_channel(
                "control",
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    ..Default::default()
                }),
            )
            .await?;

        let pointer = pc
            .create_data_channel(
                "pointer",
                Some(RTCDataChannelInit {
                    ordered: Some(false),
                    max_retransmits: Some(0),
                    ..Default::default()
                }),
            )
            .await?;

        {
            let control_id = control.id();
            control.on_open(Box::new(move || {
                log::debug!("control data channel open (id {control_id})");
                Box::pin(async {})
            }));
            let control_id = control.id();
            control.on_close(Box::new(move || {
                log::debug!("control data channel closed (id {control_id})");
                Box::pin(async {})
            }));
        }
        {
            let pointer_id = pointer.id();
            pointer.on_open(Box::new(move || {
                log::debug!("pointer data channel open (id {pointer_id})");
                Box::pin(async {})
            }));
            let pointer_id = pointer.id();
            pointer.on_close(Box::new(move || {
                log::debug!("pointer data channel closed (id {pointer_id})");
                Box::pin(async {})
            }));
        }

        Ok(Self { control, pointer })
    }

    /// Stage6.5: send a text line on the reliable `control` channel. The library
    /// returns `ErrClosedPipe` when the channel is not Open; we pre-check the
    /// state so the failure message names the channel stage.
    pub async fn send_control(&self, line: &str) -> Result<()> {
        if self.control.ready_state() != RTCDataChannelState::Open {
            anyhow::bail!(
                "control channel not open (state {:?})",
                self.control.ready_state()
            );
        }
        self.control.send_text(line).await?;
        Ok(())
    }

    /// Shared reference to `control` for the signaling task's `on_message`
    /// registration in task #35.
    pub fn control(&self) -> &Arc<RTCDataChannel> {
        &self.control
    }

    /// Close both channels during session cleanup (plain `RTCDataChannel::close`,
    /// not the detach API).
    pub async fn close_all(&self) -> Result<()> {
        let _ = self.control.close().await;
        let _ = self.pointer.close().await;
        Ok(())
    }
}
