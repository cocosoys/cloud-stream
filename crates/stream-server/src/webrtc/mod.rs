//! PeerConnection assembly and lifecycle (design 6.1, 6.5).
//!
//! Stage1 media surface:
//! * One video transceiver, no audio track / transceiver at all.
//! * SettingEngine: mDNS Disabled, fe80::/10 link-local filtering;
//!   RTCConfiguration ice_servers = [], policy All (hbb_common
//!   webrtc.rs:633-667 pattern). `detach_data_channels` is deliberately NOT
//!   enabled: once enabled, the library stops its internal `read_loop` and the
//!   caller must `detach()` the underlying channel and read/write it directly
//!   (data_channel/mod.rs:282-299). That path exists for hbb_common's custom
//!   large-message reassembly; stage1 uses the standard `on_message` +
//!   `send_text` surface, so the default attached channels keep the automatic
//!   message dispatch. MediaEngine registers the default codec set and the
//!   default NACK / RTCP report / TWCC interceptors
//!   (examples/play-from-disk-h264.rs:106-123): an empty MediaEngine would
//!   produce an offer with no negotiable codec.
//! * Server is always the offerer: after SESSION_OK the session builds the PC,
//!   opens the two data channels, adds the VP8 track, `make_offer` pushes the
//!   offer over the WS signaling line; the client answers and its remote
//!   candidates arrive on the same line.
//! * State callback: Connected -> signaling layer starts the pipeline and
//!   sends stream_info / connection notices; Failed / Closed -> session
//!   cleanup (design 6.5). `Disconnected` is transient and recoverable, so it
//!   does not tear the chain down (hbb_common webrtc.rs:787-803). The 0.13
//!   method name is `on_peer_connection_state_change`; the design draft's
//!   `on_connection_state_change` was never an API name.

pub mod channels;
pub mod ice;
pub mod track;

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use log;
use tokio::sync::{mpsc, watch, Mutex};

use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::interceptor::registry::Registry;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::peer_connection::RTCPeerConnection;

use channels::ControlChannels;
use ice::RemoteCandidate;
pub use track::{AudioTrack, VideoTrack};

/// Session-visible media state mapped from the PC connection state. The
/// signaling layer subscribes to `state_rx` to gate the capture loop and the
/// one-shot downstream notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaState {
    /// Initial state before the PC reaches Connected.
    New,
    /// All transports are connected; safe to start writing samples and send
    /// `stream_info` / `connection`.
    Connected,
    /// Terminal: PC reports Failed; the session must be cleaned up.
    Failed,
    /// Terminal: PC reports Closed; the session must be cleaned up.
    Closed,
}

/// Per-connection WebRTC state owned by one signaling session.
pub struct MediaSession {
    pc: Arc<RTCPeerConnection>,
    video: VideoTrack,
    /// #77: game-sound downlink track (Opus). Fed by `pipeline::audio`.
    audio: AudioTrack,
    channels: ControlChannels,
    /// #53: the video `RTCRtpSender` (wrapped in Arc: the type is not Clone),
    /// retained so an RTCP PLI/FIR listener task can read the inbound feedback
    /// and force a keyframe on the pipeline.
    sender: Arc<RTCRtpSender>,

    state_tx: watch::Sender<MediaState>,
    state_rx: watch::Receiver<MediaState>,

    /// Raw gathered-candidate JSON (`RTCIceCandidateInit` serialized). The
    /// signaling layer wraps it into the `<peer_id> {"ice": {...}}` line.
    ice_rx: mpsc::UnboundedReceiver<String>,
    ice_tx: Option<mpsc::UnboundedSender<String>>,

    /// Candidates arriving before the remote answer is applied are held here
    /// and flushed once `set_remote_description` succeeds.
    pending: Arc<Mutex<Vec<RemoteCandidate>>>,
    remote_applied: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

impl MediaSession {
    /// Stage6.1: build API / SettingEngine / MediaEngine / PC, open channels,
    /// add the VP8 track, register the ICE and state callbacks.
    pub async fn new() -> Result<Self> {
        let mut setting_engine = SettingEngine::default();
        // No detach_data_channels(): attached channels keep the internal read
        // loop that dispatches on_message; see the module-level note.
        setting_engine.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        // fe80::/10 can only be bound together with a scope id, which IpAddr
        // cannot carry: gathering one yields a failed bind and a warning per
        // address only (hbb_common webrtc.rs:642-648).
        setting_engine.set_ip_filter(Box::new(|ip: IpAddr| match ip {
            IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 != 0xfe80,
            IpAddr::V4(_) => true,
        }));

        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs()?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

        let api = APIBuilder::new()
            .with_setting_engine(setting_engine)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        // Internal LAN direct connection: host candidates only, no STUN/TURN.
        let config = RTCConfiguration {
            ice_servers: vec![],
            ice_transport_policy: RTCIceTransportPolicy::All,
            ..Default::default()
        };
        let pc = Arc::new(api.new_peer_connection(config).await?);

        let video = VideoTrack::new()?;
        let sender = pc.add_track(video.as_track_local()).await?;

        // #77: the sound track first (produces `m=audio sendonly`), then an
        // explicit recvonly transceiver for the microphone.
        //
        // Order is semantic, not cosmetic: the frontend reserves "the first
        // recvonly audio m-line" as its microphone slot
        // (`webrtc.js:_prepareUplinkTransceivers`). Adding the recvonly one
        // first would make the browser attach the mic to the downlink m-line
        // and leave the game silent.
        let audio = AudioTrack::new()?;
        pc.add_track(audio.as_track_local()).await?;
        pc.add_transceiver_from_kind(
            RTPCodecType::Audio,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("add recvonly audio transceiver for the mic: {e}"))?;

        let channels = ControlChannels::open(&pc).await?;

        let (ice_tx, ice_rx) = mpsc::unbounded_channel::<String>();

        // Trickle ICE: forward each local candidate as its init JSON. Gathering
        // complete arrives as `None` and sends no empty message. The sender is
        // retired inside the callback so a dead PC cannot park the forwarder.
        let mut local_ice_tx = Some(ice_tx.clone());
        pc.on_ice_candidate(Box::new(move |candidate| {
            let Some(candidate) = candidate else {
                local_ice_tx = None;
                return Box::pin(async {});
            };
            let Some(sender) = local_ice_tx.clone() else {
                return Box::pin(async {});
            };
            Box::pin(async move {
                match candidate.to_json() {
                    Ok(init) => match hbb_common::serde_json::to_string(&init) {
                        Ok(json) => {
                            let _ = sender.send(json);
                        }
                        Err(err) => {
                            log::warn!("failed to serialize local ICE candidate: {err}");
                        }
                    },
                    Err(err) => {
                        log::warn!("failed to convert local ICE candidate to JSON: {err}");
                    }
                }
            })
        }));

        let (state_tx, state_rx) = watch::channel(MediaState::New);
        let state_for_callback = state_tx.clone();
        let pending = Arc::new(Mutex::new(Vec::new()));
        let remote_applied = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));

        // Weak reference breaks the pc -> handler -> pc cycle that close()
        // never clears (hbb_common webrtc.rs:778-783).
        let weak_pc = Arc::downgrade(&pc);
        // The state callback below takes the original `closed` Arc; this clone
        // stays with the session struct for `is_closed`.
        let closed_owned = closed.clone();
        pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
            let state_tx = state_for_callback.clone();
            let weak_pc = weak_pc.clone();
            let closed = closed.clone();
            Box::pin(async move {
                log::debug!("peer connection state: {state}");
                match state {
                    RTCPeerConnectionState::Connected => {
                        let _ = state_tx.send(MediaState::Connected);
                    }
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                        closed.store(true, Ordering::SeqCst);
                        let notice = if state == RTCPeerConnectionState::Failed {
                            MediaState::Failed
                        } else {
                            MediaState::Closed
                        };
                        let _ = state_tx.send(notice);
                        // Replace the ICE handler so the forwarder loop ends
                        // instead of parking on a receiver that never drains.
                        if let Some(pc) = weak_pc.upgrade() {
                            pc.on_ice_candidate(Box::new(|_| Box::pin(async {})));
                        }
                    }
                    // New / Connecting / Disconnected: no chain action.
                    // Disconnected is a transient, recoverable ICE consent state.
                    _ => {}
                }
            })
        }));

        // Stage1 both channels are created by this server; the answerer does
        // not open its own DC. A handler remains registered so an unexpected
        // inbound channel is logged instead of arriving on an unbound default.
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            log::warn!("unexpected inbound data channel: {}", dc.label());
            Box::pin(async {})
        }));

        Ok(Self {
            pc,
            video,
            audio,
            channels,
            sender,
            state_tx,
            state_rx,
            ice_rx,
            ice_tx: Some(ice_tx),
            pending,
            remote_applied,
            closed: closed_owned,
        })
    }

    /// #53: spawn the RTCP feedback listener. The browser's `RTCPeerConnection`
    /// sends PLI (and FIR) over the inbound RTCP stream when it loses decoder
    /// context (RFC 4585 / RFC 8834: senders MUST respond). `read_rtcp`
    /// (rtp_sender/mod.rs:612) yields the raw packets; we downcast them and
    /// poke the pipeline to force the next keyframe, re-anchoring the VP8
    /// reference chain. The loop ends when the sender's RTCP stream closes
    /// (PC teardown).
    pub fn spawn_pli_listener(&self, handle: crate::pipeline::PipelineHandle) {
        let sender = Arc::clone(&self.sender);
        log::info!("spawning PLI/FIR listener");
        tokio::spawn(async move {
            loop {
                match sender.read_rtcp().await {
                    Ok((packets, _attrs)) => {
                        for packet in packets {
                            let pli = packet
                                .as_any()
                                .downcast_ref::<PictureLossIndication>()
                                .is_some();
                            let fir = packet
                                .as_any()
                                .downcast_ref::<FullIntraRequest>()
                                .is_some();
                            if pli || fir {
                                log::info!(
                                    "received {} request; forcing keyframe",
                                    if pli { "PLI" } else { "FIR" }
                                );
                                // #78: the panel's "Keyframe requests" figure.
                                handle.note_keyframe_request();
                                handle.request_keyframe();
                            }
                        }
                    }
                    Err(err) => {
                        log::info!("PLI listener ended: {err}");
                        break;
                    }
                }
            }
        });
    }

    /// Stage6.4: create the offer, install it as the local description and
    /// return it for the signaling layer to serialize onto the WS line. The
    /// server performs no SDP munging; the client answer side applies the
    /// VP8 packetization tweaks itself (webrtc.js:311-318).
    pub async fn make_offer(&self) -> Result<RTCSessionDescription> {
        let offer = self.pc.create_offer(None).await?;
        self.pc.set_local_description(offer.clone()).await?;
        Ok(offer)
    }

    /// Stage6.4: install the client answer, then flush any remote candidates
    /// that arrived before the remote description was applied. The frontend
    /// contract only ever answers an offer (webrtc.js:301), so the description
    /// is built through `answer()`: the constructor unmarshals the SDP and
    /// initializes the private `parsed` field that an external caller cannot
    /// set directly.
    pub async fn apply_answer(&self, _sdp_type: &str, sdp: &str) -> Result<()> {
        let desc = RTCSessionDescription::answer(sdp.to_owned())?;
        self.pc.set_remote_description(desc).await?;
        self.remote_applied.store(true, Ordering::SeqCst);

        let mut pending = self.pending.lock().await;
        for candidate in pending.drain(..) {
            if let Err(err) = self
                .pc
                .add_ice_candidate(ice::to_init(&candidate))
                .await
            {
                log::warn!("failed to apply queued remote ICE candidate: {err}");
            }
        }
        Ok(())
    }

    /// Stage6.4: queue the remote candidate until the answer lands, or add it
    /// straight away once the remote description is installed.
    pub async fn add_remote(&self, candidate: RemoteCandidate) -> Result<()> {
        if self.remote_applied.load(Ordering::SeqCst) {
            self.pc.add_ice_candidate(ice::to_init(&candidate)).await?;
        } else {
            self.pending.lock().await.push(candidate);
        }
        Ok(())
    }

    /// State watcher for the signaling task: latest Connected / Failed /
    /// Closed notice drives the pipeline gate and cleanup.
    pub fn state_rx(&self) -> watch::Receiver<MediaState> {
        self.state_rx.clone()
    }

    /// Own the local-candidate receiver for the WS forwarder task. Tokio mpsc
    /// receivers are not `Clone`, so the receiver is transferred once; later
    /// calls return `None` and the old sender is retired with it.
    pub fn take_ice_rx(&mut self) -> Option<mpsc::UnboundedReceiver<String>> {
        if self.ice_tx.is_some() {
            let (_, empty_rx) = mpsc::unbounded_channel();
            let old = std::mem::replace(&mut self.ice_rx, empty_rx);
            self.ice_tx.take();
            Some(old)
        } else {
            None
        }
    }

    /// Video track handle for the capture loop's `write_sample` calls.
    pub fn video(&self) -> &VideoTrack {
        &self.video
    }

    /// #77: audio track handle for the loopback thread's `write_sample` calls.
    pub fn audio(&self) -> &AudioTrack {
        &self.audio
    }

    /// Data channel handles for downstream JSON and the upstream command hook.
    pub fn channels(&self) -> &ControlChannels {
        &self.channels
    }

    /// Stage6.5 cleanup: close both channels and the PC, retire the ICE sender.
    pub async fn close(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let _ = self.channels.close_all().await;
        let _ = self.pc.close().await;
        let _ = self.state_tx.send(MediaState::Closed);
        Ok(())
    }
}
