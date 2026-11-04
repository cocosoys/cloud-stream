//! One encoder, N downlinks.
//!
//! Why this exists
//! ---------------
//! Until now a pipeline and a browser were the same object: `spawn_loop` took
//! the *one* `VideoTrack` the session had just created, so a second connection
//! had no way to receive the frames the first one was already paying for — it
//! could only be evicted (close code 4001).
//!
//! A second capture is not an option: `IDXGIOutput1::DuplicateOutput` is
//! exclusive per output, so a second `Capturer` on the same screen fails with
//! `DXGI_ERROR_NOT_CURRENTLY_AVAILABLE` and the recovery chain parks it on GDI —
//! two captures, two encoders, and the slower backend for whoever arrived
//! second. Encoding is the frame budget on this host (≈11.5 ms of a 16.7 ms
//! beat), which is why the frames are produced once and copied per viewer:
//! `Bytes::clone` is an `Arc` bump, so a repeat costs a packetizer pass per
//! sink, not an encode.
//!
//! Threading: `write` never holds the lock across an `await`. The sink list is
//! copied out (`VideoTrack` is an `Arc` inside), the lock is dropped, and the
//! writes run **concurrently**, each under its own short deadline. A detached
//! sink is therefore missed by at most one frame in flight, which is exactly the
//! atomicity a disconnect needs; and a sink that stalls costs one dropped frame
//! for one viewer instead of a late frame for all of them.
//!
//! Why the per-sink counters live here and nowhere else
//! ----------------------------------------------------
//! With one shared encoder, "how much bandwidth did this person use" has no
//! answer at the pipeline level: `PipelineStats::bytes_sent` is the program's
//! total, and dividing it by the number of viewers is wrong the moment two
//! sinks accept different numbers of frames (a track that is not bound yet
//! fails every write). The fan-out is the last point where a frame can still be
//! attributed to the connection it was written to, so the accounting is done
//! there — one add per accepted write, which is the cost of a counter bump
//! against a packetizer pass that already happened.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::webrtc::VideoTrack;

/// One attached downlink, so an owner can detach exactly its own entry.
///
/// Ids are handed out monotonically and never reused: a stale `detach` from a
/// session that already left can never remove the sink of a session that
/// attached after it (the same reason `SessionRegistry` ids are monotonic).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SinkId(u64);

/// What one downlink has taken since it attached.
///
/// Deliberately not a window: the health endpoint needs "how much has this
/// person cost me so far", which is a session fact. Rates are derived by the
/// caller dividing by the session's age, so the same counter answers both
/// questions and neither has to be reset (a reset would race the reader).
#[derive(Default, Debug)]
pub struct SinkCounters {
    bytes: AtomicU64,
    frames: AtomicU64,
}

impl SinkCounters {
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Test-only: set the tallies directly.
    ///
    /// The health endpoint's wiring needs a session that has already sent
    /// traffic; producing that in a unit test would mean a real packetizer and
    /// a real peer connection. The increment itself is covered by the fan-out
    /// tests, so what is left to prove here is only that the number is read
    /// from the right place.
    #[cfg(test)]
    pub fn seed(&self, bytes: u64, frames: u64) {
        self.bytes.store(bytes, Ordering::Relaxed);
        self.frames.store(frames, Ordering::Relaxed);
    }
}

/// The handle a session keeps for its own downlink.
///
/// It outlives the fan-out entry on purpose: teardown detaches the sink before
/// the session reports what it used, and the final number must still be
/// readable after the track is gone. Hence an `Arc` back to the counters rather
/// than a lookup by id.
#[derive(Clone)]
pub struct SinkHandle {
    id: SinkId,
    counters: Arc<SinkCounters>,
}

impl SinkHandle {
    pub fn id(&self) -> SinkId {
        self.id
    }

    pub fn bytes(&self) -> u64 {
        self.counters.bytes()
    }

    pub fn frames(&self) -> u64 {
        self.counters.frames()
    }

    /// The shared counter block, for a session that wants to keep reporting
    /// after it hands the handle over (the registry stores this, the task keeps
    /// the handle).
    pub fn counters(&self) -> Arc<SinkCounters> {
        self.counters.clone()
    }
}

impl std::fmt::Debug for SinkHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SinkHandle")
            .field("id", &self.id.0)
            .field("bytes", &self.bytes())
            .field("frames", &self.frames())
            .finish()
    }
}

/// A downlink that is not a WebRTC track: the encoded frame goes to a channel
/// and the transport decides what to do with it.
///
/// #106: the WebSocket transport needs the raw bitstream under its own 12-byte
/// header, not an RTP packetized track — `VideoTrack` is the wrong shape for it
/// and always will be. The alternatives were both worse: a second capture is
/// impossible (`DuplicateOutput` is exclusive per output, the same reason this
/// file exists) and a second pipeline would mean two encoders on one host whose
/// encode is already 11.5 ms of a 16.7 ms beat. So the socket is one more
/// downlink: same `Bytes`, same frame, no extra encode. `Bytes::clone` is an
/// `Arc` bump, so a second transport costs a channel send.
struct StreamSink {
    id: SinkId,
    /// Bounded on purpose. An unbounded channel would turn "this viewer's
    /// link is slower than the encoder" into unbounded memory growth on the
    /// host, which on a gaming machine is not a slowdown but a crash; here the
    /// frame is dropped, the viewer's writer notices the gap and asks for a
    /// keyframe, and the program never notices.
    tx: mpsc::Sender<Bytes>,
    counters: Arc<SinkCounters>,
}

/// The set of tracks one encoded frame is written to.
#[derive(Default)]
pub struct VideoFanout {
    /// A std mutex, not the tokio one: the list is swapped in a few
    /// microseconds and never held across an await (see the module note).
    inner: Mutex<Vec<(SinkId, VideoTrack, Arc<SinkCounters>)>>,
    /// Non-WebRTC downlinks (#106); see [`StreamSink`]. A separate list rather
    /// than an enum in `inner` so the RTP path keeps its exact shape — a
    /// transport that needs the bitstream must not change what a track sees.
    streams: Mutex<Vec<StreamSink>>,
    next: AtomicU64,
}

impl VideoFanout {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a downlink. The caller keeps the handle and must `detach` it on
    /// teardown; a sink that is never detached keeps receiving frames into a
    /// track whose peer connection is gone (harmless, but it hides the leak).
    pub fn attach(&self, track: VideoTrack) -> SinkHandle {
        let id = SinkId(self.next.fetch_add(1, Ordering::SeqCst) + 1);
        let counters = Arc::new(SinkCounters::default());
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        guard.push((id, track, counters.clone()));
        log::info!("video fanout: sink {} attached ({} live)", id.0, guard.len());
        SinkHandle { id, counters }
    }

    /// Add a non-WebRTC downlink (#106): every subsequent frame is handed to
    /// `tx` as it is, unconverted and without a packetizer.
    ///
    /// The caller owns the receiving end and must `detach` on teardown — a
    /// socket that closes without detaching leaves a sender whose `try_send`
    /// fails forever, which is silent but pointless work on every frame.
    ///
    /// No keyframe request happens here: this type has no pipeline handle, and
    /// the transport is the party that knows whether its viewer has a reference
    /// yet. `ws_media` requests one on attach, exactly like `attach(track, true)`.
    pub fn attach_stream(&self, tx: mpsc::Sender<Bytes>) -> SinkHandle {
        let id = SinkId(self.next.fetch_add(1, Ordering::SeqCst) + 1);
        let counters = Arc::new(SinkCounters::default());
        let mut guard = self.streams.lock().unwrap_or_else(|p| p.into_inner());
        guard.push(StreamSink {
            id,
            tx,
            counters: counters.clone(),
        });
        log::info!(
            "video fanout: stream sink {} attached ({} live)",
            id.0,
            guard.len()
        );
        SinkHandle { id, counters }
    }

    /// Remove one downlink. `false` when the id was already gone, which a
    /// teardown path treats as "nothing to do" rather than as an error.
    pub fn detach(&self, id: SinkId) -> bool {
        let mut removed = {
            let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let before = guard.len();
            guard.retain(|(sink, _, _)| *sink != id);
            guard.len() != before
        };
        if !removed {
            let mut guard = self.streams.lock().unwrap_or_else(|p| p.into_inner());
            let before = guard.len();
            guard.retain(|sink| sink.id != id);
            removed = guard.len() != before;
        }
        if removed {
            log::info!("video fanout: sink {} detached ({} live)", id.0, self.len());
        }
        removed
    }

    /// How many downlinks are attached, of either kind. One number because the
    /// question every caller asks is "is anybody still watching" — splitting it
    /// would let a transport-less program report zero viewers and stop a
    /// capture a socket was still reading.
    pub fn len(&self) -> usize {
        let tracks = self
            .inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len();
        let streams = self
            .streams
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len();
        tracks + streams
    }

    /// Whether no downlink is attached — nobody is watching.
    ///
    /// Used by the tests and by the "is anybody still here" question
    /// `PipelineRegistry` asks before stopping a capture.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Write one encoded frame to every attached track, returning how many
    /// accepted it.
    ///
    /// Zero means nobody is listening — the caller keeps encoding (the capture
    /// loop's gate is `START_VIDEO`, not the sink count) but must not count the
    /// frame as sent, or `stream_stats.video_mbps` reports traffic that never
    /// left. A track that is not bound yet (offer/answer still in flight)
    /// returns `Err`; that is normal for the first frames after an attach, and
    /// it is also why the per-sink counters are bumped **after** the write:
    /// counting on attempt would report bandwidth that was never queued.
    pub async fn write(&self, data: Bytes, spf: Duration) -> usize {
        let len = data.len() as u64;
        let sinks: Vec<(SinkId, VideoTrack, Arc<SinkCounters>)> = {
            let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard
                .iter()
                .map(|(id, track, counters)| (*id, track.clone(), counters.clone()))
                .collect()
        };
        // Copied out for the same reason as the tracks: the lock is never held
        // across a send, and a `Sender` is cheap to clone.
        let streams: Vec<(SinkId, mpsc::Sender<Bytes>, Arc<SinkCounters>)> = {
            let guard = self.streams.lock().unwrap_or_else(|p| p.into_inner());
            guard
                .iter()
                .map(|s| (s.id, s.tx.clone(), s.counters.clone()))
                .collect()
        };
        if sinks.is_empty() && streams.is_empty() {
            return 0;
        }
        // Concurrent, not one after another (#104). Two sinks written in
        // sequence cost the *sum* of their latencies on every frame, so one
        // viewer on a congested link (measured on the target host: 400 ms
        // `send_ms` with two tabs, against 7 ms with one) held the encoder's
        // next frame behind it. The loop's beat then stretched to seconds and
        // the picture froze for *everybody* — the second tab did not just take
        // longer, it made the first one unusable.
        let budget = write_budget(spf);
        let writes = sinks.into_iter().map(|(id, track, counters)| {
            let data = data.clone();
            async move {
                // A stalled sink must not own the frame budget. Timing out drops
                // *this* frame for *this* viewer and nothing else; the next
                // frame tries again, and a track that stays stuck shows up as a
                // frozen viewer rather than a frozen program.
                match tokio::time::timeout(budget, track.write_sample(data, spf)).await {
                    Ok(Ok(())) => {
                        counters.frames.fetch_add(1, Ordering::Relaxed);
                        counters.bytes.fetch_add(len, Ordering::Relaxed);
                        true
                    }
                    Ok(Err(err)) => {
                        log::debug!("video fanout: sink {} write failed: {err:#}", id.0);
                        false
                    }
                    Err(_) => {
                        log::debug!(
                            "video fanout: sink {} write exceeded {budget:?}, frame dropped",
                            id.0
                        );
                        false
                    }
                }
            }
        });
        let mut accepted = futures_util::future::join_all(writes)
            .await
            .into_iter()
            .filter(|ok| *ok)
            .count();
        // #106: the streaming downlinks, after the tracks. `try_send` and no
        // await: a socket that is slow must cost its own frame, never the
        // program's beat — the whole point of the budget above is that one
        // stalled viewer cannot stretch the encoder's next frame.
        for (id, tx, counters) in streams {
            match tx.try_send(data.clone()) {
                Ok(()) => {
                    counters.frames.fetch_add(1, Ordering::Relaxed);
                    counters.bytes.fetch_add(len, Ordering::Relaxed);
                    accepted += 1;
                }
                // Full: this transport is behind. Dropping the *newest* frame
                // rather than the oldest keeps the queue from ever holding a
                // stale picture, and the resulting gap in frame ids is what the
                // viewer's decode gate reports (it asks for a keyframe, so the
                // stream re-anchors instead of smearing).
                Err(TrySendError::Full(_)) => {
                    log::debug!("video fanout: stream sink {} is behind, frame dropped", id.0);
                }
                // Closed: the transport's reader is gone but the detach has not
                // landed yet. Nothing to do — the next frame finds it detached.
                Err(TrySendError::Closed(_)) => {}
            }
        }
        accepted
    }
}

/// How long one sink may hold a frame before it is dropped.
///
/// Four frame intervals is the natural answer — it tolerates the jitter a
/// congested link produces without ever crossing into "the next frame is late"
/// — clamped to a floor so a 120 fps program does not time out on a healthy
/// write, and to a ceiling so a 15 fps one cannot stretch the beat to a second.
fn write_budget(spf: Duration) -> Duration {
    let four = spf.saturating_mul(4);
    four.max(Duration::from_millis(50))
        .min(Duration::from_millis(150))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::codec::VideoCodec;

    fn track() -> VideoTrack {
        VideoTrack::new(VideoCodec::Vp8).expect("VP8 track")
    }

    #[test]
    fn attach_hands_out_distinct_ids() {
        let fanout = VideoFanout::new();
        let a = fanout.attach(track());
        let b = fanout.attach(track());
        let c = fanout.attach(track());
        assert_ne!(a.id(), b.id());
        assert_ne!(b.id(), c.id());
        assert_ne!(a.id(), c.id());
        assert_eq!(fanout.len(), 3);
    }

    #[test]
    fn detach_removes_exactly_its_own_sink() {
        let fanout = VideoFanout::new();
        let a = fanout.attach(track());
        let b = fanout.attach(track());
        assert!(fanout.detach(a.id()));
        assert_eq!(fanout.len(), 1);
        // The survivor is b: detaching it must still work, and a second detach
        // of the same id is a no-op rather than an error path the caller has to
        // distinguish.
        assert!(fanout.detach(b.id()));
        assert!(fanout.is_empty());
        assert!(!fanout.detach(b.id()));
        assert!(!fanout.detach(a.id()));
    }

    #[test]
    fn ids_are_not_reused_after_the_list_drains() {
        // A late `detach` from a session that already left must not remove the
        // sink a newer session got for the same slot.
        let fanout = VideoFanout::new();
        let first = fanout.attach(track());
        assert!(fanout.detach(first.id()));
        let second = fanout.attach(track());
        assert_ne!(first.id(), second.id());
        assert!(!fanout.detach(first.id()));
        assert_eq!(fanout.len(), 1);
    }

    #[tokio::test]
    async fn write_with_no_sink_reaches_nobody() {
        let fanout = VideoFanout::new();
        let accepted = fanout.write(Bytes::from_static(&[0u8; 16]), Duration::from_millis(16)).await;
        assert_eq!(accepted, 0, "an empty fanout must not report a delivery");
    }

    #[tokio::test]
    async fn write_reports_only_the_sinks_that_took_the_frame() {
        // An unbound track is the state every fresh session is in for the first
        // frames (the packetizer exists only after offer/answer), and the
        // forwarder's `send_ms` window must not count it as sent.
        let fanout = VideoFanout::new();
        let handle = fanout.attach(track());
        let accepted = fanout.write(Bytes::from_static(&[0u8; 16]), Duration::from_millis(16)).await;
        assert!(accepted <= 1);
        assert!(fanout.detach(handle.id()));
        assert_eq!(
            fanout.write(Bytes::from_static(&[0u8; 16]), Duration::from_millis(16)).await,
            0
        );
    }

    /// The whole point of the counter: a session's bytes must be attributable
    /// even when the fan-out holds several sinks. With one shared encoder the
    /// program total cannot answer "who used the bandwidth".
    #[tokio::test]
    async fn each_sink_counts_only_what_it_accepted() {
        let fanout = VideoFanout::new();
        let a = fanout.attach(track());
        let b = fanout.attach(track());
        let frame = Bytes::from_static(&[7u8; 100]);

        let accepted = fanout.write(frame.clone(), Duration::from_millis(16)).await;
        if accepted == 0 {
            // Both tracks are unbound in a unit test, so no packetizer exists.
            // Nothing was delivered, and the counters must say exactly that.
            assert_eq!(a.bytes(), 0);
            assert_eq!(b.bytes(), 0);
            return;
        }
        // `accepted` is how many sinks took *this one* frame, not a frame count:
        // each of them must have added exactly one frame's worth of bytes.
        assert_eq!(a.bytes(), 100, "one 100-byte frame per sink");
        assert_eq!(a.frames(), 1);
        assert_eq!(b.bytes(), a.bytes(), "both sinks saw the same frames");
        assert_eq!(b.frames(), 1);
    }

    /// Teardown detaches first and reports afterwards, so the handle must stay
    /// readable once the fan-out entry is gone — the final number is the one an
    /// operator quotes.
    #[tokio::test]
    async fn a_detached_handle_still_reports_what_it_took() {
        let fanout = VideoFanout::new();
        let handle = fanout.attach(track());
        let accepted = fanout.write(Bytes::from_static(&[1u8; 64]), Duration::from_millis(16)).await;
        let before = handle.bytes();
        assert!(fanout.detach(handle.id()));
        assert_eq!(handle.bytes(), before, "detach must not zero the tally");
        // One frame was written, so a sink that took it counted 64 bytes — and a
        // sink that did not take it counted nothing.
        assert_eq!(before, 64 * accepted as u64);
    }

    /// #106: a stream sink is a downlink like any other. The point of the test
    /// is the assert that would fail on a track: `accepted` is 1 although no
    /// track is bound, because a channel always takes the frame its capacity
    /// allows — which is why the forwarder's `send_ms` window must count this
    /// transport as real traffic.
    #[tokio::test]
    async fn a_stream_sink_takes_a_frame_with_no_track_attached() {
        let fanout = VideoFanout::new();
        let (tx, mut rx) = mpsc::channel(4);
        let handle = fanout.attach_stream(tx);

        let accepted = fanout
            .write(Bytes::from_static(&[9u8; 32]), Duration::from_millis(16))
            .await;
        assert_eq!(accepted, 1);

        let got = rx.recv().await.expect("the frame the fan-out handed over");
        assert_eq!(&got[..], &[9u8; 32][..], "bytes arrive unmodified");
        assert_eq!(handle.frames(), 1);
        assert_eq!(handle.bytes(), 32);
    }

    /// A viewer whose link is slower than the encoder must lose its own frames
    /// and nothing else. `0` here is the honest answer — the frame never left —
    /// and the missing frame id is what makes the viewer ask for a keyframe.
    #[tokio::test]
    async fn a_full_stream_channel_drops_the_frame_rather_than_buffering() {
        let fanout = VideoFanout::new();
        let (tx, _rx) = mpsc::channel(1);
        let handle = fanout.attach_stream(tx);
        let frame = Bytes::from_static(&[0u8; 8]);
        let spf = Duration::from_millis(16);

        assert_eq!(fanout.write(frame.clone(), spf).await, 1);
        assert_eq!(fanout.write(frame, spf).await, 0, "no room, no delivery");
        assert_eq!(handle.frames(), 1, "the drop is not counted as a send");
    }

    /// Both kinds in one fan-out: the program has one viewer on WebRTC and one
    /// on the socket, and neither may be invisible to `len` — the registry stops
    /// the capture when it reads zero, and a socket still reading would get
    /// nothing more.
    #[test]
    fn tracks_and_streams_share_one_live_count() {
        let fanout = VideoFanout::new();
        let (tx, _rx) = mpsc::channel(4);
        let track_sink = fanout.attach(track());
        let stream_sink = fanout.attach_stream(tx);
        assert_ne!(track_sink.id(), stream_sink.id(), "one id space for both kinds");
        assert_eq!(fanout.len(), 2);

        assert!(fanout.detach(stream_sink.id()));
        assert_eq!(fanout.len(), 1);
        assert!(fanout.detach(track_sink.id()));
        assert!(fanout.is_empty());
    }
}
