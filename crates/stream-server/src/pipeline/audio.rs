//! #77 game-sound downlink: WASAPI loopback -> Opus -> WebRTC audio track.
//!
//! Why loopback and not a "capture device": the game renders to the default
//! output endpoint, and WASAPI's loopback mode on that *render* endpoint hands
//! back the post-mix signal. That is exactly what a player hears locally —
//! game audio plus system sounds — with no virtual cable driver to install.
//!
//! Why a dedicated thread and not the video beat: the video loop is driven by
//! DXGI `WouldBlock` (a static desktop legitimately produces almost no frames),
//! so hanging audio off it would stall the sound whenever the picture settles.
//! This thread owns its own clock.
//!
//! Pacing rule (the one that matters): the beat is **derived from the sample
//! count actually delivered**, never from a wall-clock timer. A `interval(20ms)`
//! loop drifts against the device clock by ~0.1 %, which is ~600 ms after ten
//! minutes — audible as lip-sync slowly failing. Counting samples cannot drift,
//! because the device is the thing that decides how fast time passes.
//!
//! #79 adds the opposite direction, the browser microphone:
//!
//! ```text
//! TrackRemote::read_rtp --> Opus decode --> ring buffer --> WASAPI render
//! ```
//!
//! It runs on two threads, not one, and that is deliberate. `read_rtp` parks
//! until a packet arrives, so a single thread that also has to keep the render
//! endpoint fed would either block for as long as nobody speaks (the endpoint
//! underruns, the first word is clipped) or need a read timeout, and dropping
//! the read future mid-`recv` can lose the packet it had already dequeued.
//! Reader thread blocks forever; writer thread polls at 5 ms. They meet at a
//! bounded channel of encoded packets.
//!
//! Known limit, by construction: the injected microphone lands on the default
//! render endpoint, which is the same endpoint the downlink loopback captures.
//! A player on speakers therefore hears their own voice once, delayed by one
//! round trip. Headphones break the acoustic path; nothing in process can break
//! the electrical one without an echo canceller (the loopback is post-mix, so
//! the injected signal cannot be separated back out).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use audiopus::coder::{Decoder, Encoder};
use audiopus::{Application, Bitrate, Channels, SampleRate};
use bytes::Bytes;
use std::sync::Mutex;
use tokio::sync::mpsc;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVE_FORMAT_PCM, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::webrtc::AudioTrack;
use webrtc::track::track_remote::TrackRemote;

use super::avsync::{self, Fix};

/// Opus only accepts 8/12/16/24/48 kHz. 48 kHz is native, so the host's audio
/// engine does the (single, well-tested) conversion for us when the endpoint
/// runs at 44.1 kHz.
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// 20 ms is the WebRTC default `ptime` and the granularity a lost packet costs.
const FRAME_MS: u32 = 20;
const SAMPLES_PER_FRAME: usize = (SAMPLE_RATE as usize / 1000) * FRAME_MS as usize;
/// Interleaved i16 count for one 20 ms stereo frame (960 * 2).
const INTS_PER_FRAME: usize = SAMPLES_PER_FRAME * CHANNELS as usize;
const BITRATE_BPS: i32 = 64_000;
/// 200 ms of device buffer: enough to ride out a scheduler hiccup on a 4 vCPU
/// VM without adding perceptible latency to the game's own audio path.
const BUFFER_HNS: i64 = 2_000_000;
/// 100 ms on the microphone side. Everything buffered here sits in front of the
/// speaker's own ears rather than behind a screen, so it gets half the slack the
/// capture side does.
const RENDER_BUFFER_HNS: i64 = 1_000_000;
/// Longest legal Opus packet is 60 ms; the decoder writes into a buffer this
/// size and the real length comes back from `decode` (10 ms packets are legal
/// too, since the frontend negotiates `minptime=10`).
const MAX_DECODE_INTS: usize = (SAMPLE_RATE as usize / 1000) * 60 * CHANNELS as usize;
/// Playback starts only once this many 20 ms frames are queued (60 ms).
const PREROLL_FRAMES: usize = 3;
/// Past 500 ms the queue is a backlog, not jitter — trim back to the floor.
const MAX_BUFFERED_FRAMES: usize = 25;
/// Writer poll: 5 ms against a 100 ms buffer gives 20 chances per buffer.
const WRITE_TICK_MS: u64 = 5;

// Diagnostics, same shape as the video counters so one log line tells the story.
static CAPTURED_FRAMES: AtomicU64 = AtomicU64::new(0);
static ENCODED_PACKETS: AtomicU64 = AtomicU64::new(0);
static ENCODED_BYTES: AtomicU64 = AtomicU64::new(0);
static SILENT_FRAMES: AtomicU64 = AtomicU64::new(0);

/// #79 microphone path: packets read, frames decoded, PLC frames fabricated,
/// frames the endpoint had to fill with silence.
static UPLINK_PACKETS: AtomicU64 = AtomicU64::new(0);
static UPLINK_FRAMES: AtomicU64 = AtomicU64::new(0);
static UPLINK_CONCEALED: AtomicU64 = AtomicU64::new(0);
static UPLINK_SILENCE: AtomicU64 = AtomicU64::new(0);
/// Packets the writer never saw because its queue was full.
static UPLINK_DROPPED: AtomicU64 = AtomicU64::new(0);

/// #80: live encoder target. `ab,<N>` lands here; the encoder is re-aimed on the
/// next frame rather than restarted, because an Opus encoder can change bitrate
/// mid-stream without a hiccup.
static TARGET_BPS: AtomicI32 = AtomicI32::new(BITRATE_BPS);
/// #80: whether the downlink carries game sound at all. Muting still *emits* a
/// frame every 20 ms — a silence frame costs a couple of bytes, while stopping
/// the track starves the receiver's jitter buffer and the browser reports the
/// stream as ended.
static ENABLED: AtomicBool = AtomicBool::new(true);
/// Opus accepts 6k–510k for stereo; the ceiling is capped at 128k because this
/// carries game audio, not music, and the panel's own default is 128k.
const MIN_BPS: i32 = 6_000;
const MAX_BPS: i32 = 128_000;

/// #80: `ab,<N>`, clamped and recorded. A value of 0 means "back to default".
pub fn set_bitrate_bps(bps: i32) {
    let clamped = if bps <= 0 {
        BITRATE_BPS
    } else {
        bps.clamp(MIN_BPS, MAX_BPS)
    };
    TARGET_BPS.store(clamped, Ordering::Relaxed);
}

/// The downlink's live Opus target, in bps.
///
/// Read by `server_settings` so the panel's audio slider shows what the encoder
/// is actually aimed at: the value lives here rather than in `StreamSettings`
/// because the audio thread outlives the session, and a copy in two places is a
/// copy that can disagree.
pub fn current_bitrate_bps() -> u32 {
    TARGET_BPS.load(Ordering::Relaxed).max(0) as u32
}

/// The stops the panel's audio-bitrate slider indexes into, in bps.
///
/// Advertised in `server_settings` so the track can only hold values this
/// server will actually take: everything here is inside `MIN_BPS..=MAX_BPS`,
/// and nothing outside it is offered. The upstream list went to 510k, which
/// [`set_bitrate_bps`] silently clamps to 128k — a stop that lies about what it
/// did.
pub const BITRATE_STEPS: [u32; 5] = [32_000, 48_000, 64_000, 96_000, 128_000];

/// #80: `START_AUDIO` / `STOP_AUDIO`. Process-wide because the encoder thread is
/// per-session and looks this up every frame.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether the downlink currently carries sound.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// (captured 20 ms frames, encoded packets, encoded bytes, silence-flagged frames).
pub fn totals() -> (u64, u64, u64, u64) {
    (
        CAPTURED_FRAMES.load(Ordering::Relaxed),
        ENCODED_PACKETS.load(Ordering::Relaxed),
        ENCODED_BYTES.load(Ordering::Relaxed),
        SILENT_FRAMES.load(Ordering::Relaxed),
    )
}

/// #79: (packets received, decoded 20 ms frames, concealed frames, silence fills).
pub fn uplink_totals() -> (u64, u64, u64, u64) {
    (
        UPLINK_PACKETS.load(Ordering::Relaxed),
        UPLINK_FRAMES.load(Ordering::Relaxed),
        UPLINK_CONCEALED.load(Ordering::Relaxed),
        UPLINK_SILENCE.load(Ordering::Relaxed),
    )
}

/// A running loopback capture plus its event handle.
struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
}

impl Loopback {
    /// Open loopback on the default render endpoint at 48 kHz / 16-bit / stereo.
    ///
    /// # Safety
    /// Must run on the thread that owns the COM apartment (this module
    /// initialises MTA at the top of `spawn`, and never shares the interfaces
    /// across threads).
    unsafe fn open() -> Result<Self> {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .context("CoInitializeEx(MTA) for the audio thread")?;

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("CoCreateInstance(MMDeviceEnumerator)")?;
        // eRender + loopback is the whole trick: we ask for the *speaker*
        // endpoint, then capture from it.
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("GetDefaultAudioEndpoint(eRender, eConsole) — no active output device?")?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .context("IMMDevice::Activate(IAudioClient)")?;

        // Plain PCM rather than WAVEFORMATEXTENSIBLE: 2ch/16-bit fits the
        // legacy header, and shared-mode initialisation accepts the format the
        // engine can convert to. If the endpoint rejects it, IsFormatSupported
        // / the Initialize error message says so in the log.
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: CHANNELS,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * CHANNELS as u32 * 2,
            nBlockAlign: CHANNELS * 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        let event = CreateEventW(None, false, false, None).context("CreateEventW")?;
        if event.is_invalid() {
            return Err(anyhow!("CreateEventW returned an invalid handle"));
        }

        let init = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            BUFFER_HNS,
            0,
            &format,
            None,
        );
        if let Err(err) = init {
            let _ = CloseHandle(event);
            return Err(anyhow!("IAudioClient::Initialize(loopback 48k/16/2): {err}"));
        }
        client.SetEventHandle(event).context("SetEventHandle")?;
        let capture: IAudioCaptureClient = client.GetService().context("GetService(IAudioCaptureClient)")?;
        client.Start().context("IAudioClient::Start")?;

        log::info!(
            "audio: loopback open {SAMPLE_RATE} Hz / {CHANNELS} ch / 16-bit, \
             frame={FRAME_MS}ms bitrate={BITRATE_BPS}bps"
        );
        Ok(Self {
            client,
            capture,
            event,
        })
    }

    /// Block until the device signals, then hand back what it produced.
    ///
    /// Returns the drained interleaved samples. A silent period still returns
    /// samples (zeros) when the device flags `AUDCLNT_BUFFERFLAGS_SILENT`, which
    /// keeps the timeline continuous: an encoder that stops emitting during
    /// silence makes the receiver think the stream died.
    unsafe fn drain(&self, out: &mut Vec<i16>) -> Result<()> {
        let wait = WaitForSingleObject(self.event, 500);
        if wait != WAIT_OBJECT_0 {
            // Nothing rendered for 500 ms. Not an error: a machine with no
            // playback at all legitimately never signals.
            return Ok(());
        }
        loop {
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            if let Err(err) = self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)
            {
                return Err(anyhow!("IAudioCaptureClient::GetBuffer: {err}"));
            }
            if frames == 0 {
                let _ = self.capture.ReleaseBuffer(0);
                return Ok(());
            }
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            if silent {
                SILENT_FRAMES.fetch_add(1, Ordering::Relaxed);
                out.extend(std::iter::repeat(0i16).take(frames as usize * CHANNELS as usize));
            } else if !data.is_null() {
                let count = frames as usize * CHANNELS as usize;
                let slice = std::slice::from_raw_parts(data as *const i16, count);
                out.extend_from_slice(slice);
            }
            self.capture.ReleaseBuffer(frames)?;
            // More packets may already be queued; loop until the device is dry.
            if self.capture.GetNextPacketSize().unwrap_or(0) == 0 {
                return Ok(());
            }
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
        log::info!("audio: loopback closed");
    }
}

/// Start the downlink on its own OS thread.
///
/// COM apartment + blocking waits mean this cannot be a tokio task; the async
/// track write is bridged with `Handle::block_on` from the runtime that spawned
/// us.
///
/// #108: the thread encodes **once** for every attached sink. Before this each
/// session opened its own WASAPI loopback client and its own Opus encoder, so
/// the second tab doubled the capture threads and the encoding cost for a
/// bitstream that is identical by construction (same endpoint, same mix).
pub fn spawn(fanout: Arc<AudioFanout>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    let handle = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("audio-loopback".to_owned())
        .spawn(move || {
            if let Err(err) = run(fanout, stop, handle) {
                log::warn!("audio downlink stopped: {err:#}");
            }
        })
        .expect("spawn audio-loopback thread")
}

/// One encoded audio frame's destinations, the audio twin of `VideoFanout`.
///
/// Two kinds of sink because the two transports want different things: a
/// WebRTC viewer takes a `Sample` on its own track (RTP packetises it), while
/// the WebSocket viewer takes the raw Opus packet and frames it itself
/// (`[0x01, 0x00] + opus` on the wire). Encoding is shared, framing is not.
pub struct AudioFanout {
    tracks: Mutex<Vec<(AudioSinkId, AudioTrack)>>,
    streams: Mutex<Vec<(AudioSinkId, mpsc::Sender<Bytes>)>>,
    next: AtomicU64,
}

/// Identity of one attached downlink, so a teardown removes exactly its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AudioSinkId(pub u64);

impl Default for AudioFanout {
    fn default() -> Self {
        Self {
            tracks: Mutex::new(Vec::new()),
            streams: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
        }
    }
}

impl AudioFanout {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a WebRTC audio track. The caller keeps the id and detaches on
    /// teardown; a track left behind receives frames for a peer connection that
    /// is already gone.
    pub fn attach(&self, track: AudioTrack) -> AudioSinkId {
        let id = AudioSinkId(self.next.fetch_add(1, Ordering::Relaxed) + 1);
        self.tracks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((id, track));
        id
    }

    /// Add a raw-Opus downlink (#106's WebSocket transport). Bounded on
    /// purpose: a socket slower than the 20 ms cadence must lose its own
    /// frames, not grow a queue that turns into latency.
    pub fn attach_stream(&self, tx: mpsc::Sender<Bytes>) -> AudioSinkId {
        let id = AudioSinkId(self.next.fetch_add(1, Ordering::Relaxed) + 1);
        self.streams
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((id, tx));
        id
    }

    pub fn detach(&self, id: AudioSinkId) -> bool {
        let removed = {
            let mut guard = self.tracks.lock().unwrap_or_else(|p| p.into_inner());
            let before = guard.len();
            guard.retain(|(sink, _)| *sink != id);
            guard.len() != before
        };
        if removed {
            return true;
        }
        let mut guard = self.streams.lock().unwrap_or_else(|p| p.into_inner());
        let before = guard.len();
        guard.retain(|(sink, _)| *sink != id);
        guard.len() != before
    }

    /// Whether anybody is listening. Used by the program lifecycle: audio
    /// capture runs exactly as long as the program does, and no sink may be
    /// left pointing at a closed track.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.tracks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
            + self.streams
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .len()
    }

    /// Hand one encoded packet to every sink. Blocking by design: this runs on
    /// the audio thread, whose whole job is to keep a 20 ms cadence, and
    /// `write_sample` only enqueues on the track's internal channel.
    fn write(&self, data: Bytes, handle: &tokio::runtime::Handle) {
        let tracks: Vec<(AudioSinkId, AudioTrack)> = {
            let guard = self.tracks.lock().unwrap_or_else(|p| p.into_inner());
            guard.iter().map(|(id, t)| (*id, t.clone())).collect()
        };
        for (id, track) in tracks {
            if let Err(err) = handle
                .block_on(track.write_sample(data.clone(), Duration::from_millis(FRAME_MS as u64)))
            {
                log::debug!("audio sink {}: write_sample failed: {err}", id.0);
            }
        }
        // Copied out so the lock is not held across `try_send`.
        let streams: Vec<(AudioSinkId, mpsc::Sender<Bytes>)> = {
            let guard = self.streams.lock().unwrap_or_else(|p| p.into_inner());
            guard.iter().map(|(id, tx)| (*id, tx.clone())).collect()
        };
        for (id, tx) in streams {
            match tx.try_send(data.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    log::debug!("audio sink {} is behind, one frame dropped", id.0);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }
}

fn run(
    fanout: Arc<AudioFanout>,
    stop: Arc<AtomicBool>,
    handle: tokio::runtime::Handle,
) -> Result<()> {
    let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
        .map_err(|e| anyhow!("opus encoder: {e}"))?;
    encoder
        .set_bitrate(Bitrate::BitsPerSecond(BITRATE_BPS))
        .map_err(|e| anyhow!("opus set_bitrate: {e}"))?;
    // In-band FEC costs a little bitrate and buys packet-loss recovery without
    // a round trip; NACK would need RTCP latency we do not want on audio.
    let _ = encoder.set_inband_fec(true);
    let _ = encoder.set_complexity(5);

    let loopback = unsafe { Loopback::open()? };
    let mut pcm: Vec<i16> = Vec::with_capacity(INTS_PER_FRAME * 4);
    let mut opus_buf = vec![0u8; 4000];
    // #80: encoded once and reused while muted — re-encoding a buffer of zeros
    // 50 times a second would cost more CPU than the silence is worth.
    let silence = vec![0i16; INTS_PER_FRAME];
    let mut applied_bps = BITRATE_BPS;
    let mut last_enabled = true;

    while !stop.load(Ordering::Relaxed) {
        // #80: re-aim on change only; `set_bitrate` reconfigures the encoder and
        // should not run 50 times a second.
        let want_bps = TARGET_BPS.load(Ordering::Relaxed);
        if want_bps != applied_bps {
            match encoder.set_bitrate(Bitrate::BitsPerSecond(want_bps)) {
                Ok(()) => {
                    applied_bps = want_bps;
                    log::info!("audio: encoder bitrate -> {want_bps} bps");
                }
                Err(err) => log::warn!("audio: set_bitrate({want_bps}) failed: {err}"),
            }
        }
        let now_enabled = ENABLED.load(Ordering::Relaxed);
        if now_enabled != last_enabled {
            last_enabled = now_enabled;
            // Say it once, at the transition, so the log answers "did the mute
            // button reach the server" without a trace of every frame.
            log::info!(
                "audio: downlink {}",
                if now_enabled { "resumed" } else { "muted (silence frames continue)" }
            );
        }

        unsafe { loopback.drain(&mut pcm)? };

        // #83: re-align with the video clock before emitting. The audio thread
        // owns the only correction that does not show up as a glitch: one 20 ms
        // frame dropped (media clock slows) or one extra silence frame emitted
        // (media clock speeds up). `avsync` rate-limits both to one step every
        // 2 s and only past 80 ms of skew, so a healthy session never sees one.
        match avsync::fix() {
            // Both bounds are guarded by `avsync` (80 ms of skew, one step per
            // 2 s), so this never fires on a healthy session.
            Fix::Retard if pcm.len() >= INTS_PER_FRAME => {
                pcm.drain(..INTS_PER_FRAME);
                log::debug!("avsync: dropped one audio frame (audio was ahead of video)");
            }
            Fix::Advance => {
                emit_frame(&mut encoder, &mut opus_buf, &silence, &fanout, &handle)?;
                log::debug!("avsync: emitted one extra silence frame (audio was behind video)");
            }
            _ => {}
        }
        // Emit whole frames only. Whatever is left over stays in `pcm` for the
        // next signal — that carry is what keeps the sample clock honest.
        while pcm.len() >= INTS_PER_FRAME {
            let frame: Vec<i16> = pcm.drain(..INTS_PER_FRAME).collect();
            // Muted: the clock keeps running (same frames, same 20 ms cadence),
            // only the payload becomes silence.
            let payload: &[i16] = if now_enabled { &frame } else { &silence };
            emit_frame(&mut encoder, &mut opus_buf, payload, &fanout, &handle)?;
        }
    }
    Ok(())
}

/// Encode one 20 ms frame, write it to the track and advance the media clock.
///
/// Split out of the loop so the #83 correction path (an extra silence frame)
/// and the normal path share exactly one definition of "one frame emitted" —
/// including the `note_audio` call that tells the sync clock how far audio has
/// advanced. Two copies would drift apart the first time one of them changed.
fn emit_frame(
    encoder: &mut Encoder,
    opus_buf: &mut [u8],
    payload: &[i16],
    fanout: &AudioFanout,
    handle: &tokio::runtime::Handle,
) -> Result<()> {
    let n = encoder
        .encode(payload, opus_buf)
        .map_err(|e| anyhow!("opus encode: {e}"))?;
    if n == 0 {
        return Ok(());
    }
    ENCODED_PACKETS.fetch_add(1, Ordering::Relaxed);
    ENCODED_BYTES.fetch_add(n as u64, Ordering::Relaxed);
    CAPTURED_FRAMES.fetch_add(1, Ordering::Relaxed);
    let data = Bytes::copy_from_slice(&opus_buf[..n]);
    fanout.write(data, handle);
    avsync::note_audio(Duration::from_millis(FRAME_MS as u64));
    Ok(())
}

// ---------------------------------------------------------------------------
// #79 microphone uplink: RTP -> Opus decode -> WASAPI render
// ---------------------------------------------------------------------------

/// A running render client on the default output endpoint.
struct Output {
    client: IAudioClient,
    render: IAudioRenderClient,
    /// Endpoint buffer length in frames; the writer fills whatever the engine
    /// says is free, so this is the ceiling for one `GetBuffer` call.
    buffer_frames: u32,
}

impl Output {
    /// Open the default render endpoint at 48 kHz / 16-bit / stereo.
    ///
    /// Same endpoint as the loopback capture side (`eConsole`/`eRender`) on
    /// purpose: the microphone has to come out of the speaker the player is
    /// already listening to, not a second device the game never mixes into.
    ///
    /// # Safety
    /// Must run on the thread that owns the COM apartment; the interfaces are
    /// never shared across threads.
    unsafe fn open() -> Result<Self> {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .context("CoInitializeEx(MTA) for the microphone thread")?;

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("CoCreateInstance(MMDeviceEnumerator) for the microphone")?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("GetDefaultAudioEndpoint(eRender, eConsole) — no active output device?")?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .context("IMMDevice::Activate(IAudioClient) for the microphone")?;

        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: CHANNELS,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * CHANNELS as u32 * 2,
            nBlockAlign: CHANNELS * 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        // No event handle: this side is polled at 5 ms, which is cheaper than an
        // event wait per period and keeps the silence-fill path on the same
        // branch as the data path.
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, RENDER_BUFFER_HNS, 0, &format, None)
            .context("IAudioClient::Initialize(render 48k/16/2)")?;
        let render: IAudioRenderClient = client
            .GetService()
            .context("GetService(IAudioRenderClient)")?;
        let buffer_frames = client.GetBufferSize().context("IAudioClient::GetBufferSize")?;
        client.Start().context("IAudioClient::Start(render)")?;

        log::info!(
            "audio: render open {SAMPLE_RATE} Hz / {CHANNELS} ch / 16-bit, \
             buffer={} ms ({} frames)",
            RENDER_BUFFER_HNS / 10_000,
            buffer_frames
        );
        Ok(Self {
            client,
            render,
            buffer_frames,
        })
    }

    /// Hand `frames` samples to the engine, padding with silence when short.
    unsafe fn write(&self, pcm: &mut VecDeque<i16>, playing: bool) -> Result<()> {
        let padding = self.client.GetCurrentPadding().unwrap_or(0);
        let free = self.buffer_frames.saturating_sub(padding);
        if free == 0 {
            return Ok(());
        }
        let ptr = self.render.GetBuffer(free)?;
        if ptr.is_null() {
            return Ok(());
        }
        let dst = std::slice::from_raw_parts_mut(ptr as *mut i16, free as usize * CHANNELS as usize);

        let mut filled = 0usize;
        if playing {
            let whole = (pcm.len() / CHANNELS as usize).min(free as usize) * CHANNELS as usize;
            for (slot, sample) in dst.iter_mut().zip(pcm.drain(..whole)) {
                *slot = sample;
            }
            filled = whole;
        }
        if filled < dst.len() {
            for slot in &mut dst[filled..] {
                *slot = 0;
            }
            UPLINK_SILENCE.fetch_add(((dst.len() - filled) / CHANNELS as usize) as u64, Ordering::Relaxed);
        }
        self.render.ReleaseBuffer(free, 0)?;
        Ok(())
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
        log::info!("audio: render closed");
    }
}

/// Start the microphone path on two OS threads.
///
/// Returns the writer thread; the reader is fire-and-forget and ends itself when
/// `read_rtp` errors (PC teardown).
pub fn spawn_uplink(track: Arc<TrackRemote>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    let handle = tokio::runtime::Handle::current();
    // 64 packets = 1.28 s of headroom. Beyond that the network is not slow, it
    // is gone, and replaying it late is worse than dropping it.
    let (tx, rx) = sync_channel::<(u16, Bytes)>(64);
    let reader_stop = stop.clone();
    std::thread::Builder::new()
        .name("audio-mic-rx".to_owned())
        .spawn(move || read_loop(track, reader_stop, handle, tx))
        .expect("spawn audio-mic-rx thread");

    std::thread::Builder::new()
        .name("audio-mic-tx".to_owned())
        .spawn(move || {
            if let Err(err) = run_uplink(stop, rx) {
                log::warn!("audio uplink stopped: {err:#}");
            }
        })
        .expect("spawn audio-mic-tx thread")
}

/// Reader: block on `read_rtp` forever and forward `(seq, payload)`.
///
/// Blocking here is the point. A read with a timeout would have to drop the
/// future when it expires, and dropping `RTPReadStream::recv` can discard a
/// packet that had already been popped off the stream — at 50 timeouts per
/// second that is most of the audio.
fn read_loop(
    track: Arc<TrackRemote>,
    stop: Arc<AtomicBool>,
    handle: tokio::runtime::Handle,
    tx: SyncSender<(u16, Bytes)>,
) {
    while !stop.load(Ordering::Relaxed) {
        match handle.block_on(track.read_rtp()) {
            Ok((packet, _attr)) => {
                if packet.payload.is_empty() {
                    continue;
                }
                UPLINK_PACKETS.fetch_add(1, Ordering::Relaxed);
                let seq = packet.header.sequence_number;
                // A full channel means the writer is behind; dropping is correct
                // (see `sync_channel` capacity note) but must not block the read.
                if tx
                    .try_send((seq, Bytes::copy_from_slice(&packet.payload)))
                    .is_err()
                {
                    UPLINK_DROPPED.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(err) => {
                log::info!("audio: mic reader ended: {err}");
                break;
            }
        }
    }
}

/// Decode + render loop. Owns the Opus decoder (it is not `Send`, which is why
/// this lives on this thread rather than being created by the reader).
fn run_uplink(stop: Arc<AtomicBool>, rx: Receiver<(u16, Bytes)>) -> Result<()> {
    let output = unsafe { Output::open() }?;
    let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Stereo)
        .map_err(|e| anyhow!("opus decoder: {e}"))?;
    let mut decoded = vec![0i16; MAX_DECODE_INTS];
    let mut pcm: VecDeque<i16> = VecDeque::with_capacity(INTS_PER_FRAME * MAX_BUFFERED_FRAMES);
    let mut last_seq: Option<u16> = None;
    let mut playing = false;

    while !stop.load(Ordering::Relaxed) {
        // 1. Take everything the network delivered since the last tick.
        loop {
            match rx.try_recv() {
                Ok((seq, payload)) => {
                    // Loss concealment: one fabricated frame per missing packet,
                    // capped at 3. A long gap is a stall or a mute, not loss —
                    // inventing 30 frames of noise for it would be worse than
                    // the silence.
                    if let Some(prev) = last_seq {
                        let missing = seq.wrapping_sub(prev).wrapping_sub(1);
                        for _ in 0..missing.min(3) {
                            if let Ok(n) = decoder.decode(None::<&[u8]>, &mut decoded[..], false) {
                                push_frame(&mut pcm, &decoded, n);
                                UPLINK_CONCEALED.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    last_seq = Some(seq);
                    match decoder.decode(Some(&payload[..]), &mut decoded[..], false) {
                        Ok(n) => {
                            push_frame(&mut pcm, &decoded, n);
                            UPLINK_FRAMES.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(err) => log::debug!("opus decode: {err}"),
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    log::info!("audio: mic writer ended (reader gone)");
                    return Ok(());
                }
            }
        }

        // 2. Overrun: the queue outgrew the cap, so drop the oldest audio and
        //    keep the floor. Keeping the newest is what makes a burst audible
        //    instead of late.
        let cap = INTS_PER_FRAME * MAX_BUFFERED_FRAMES;
        if pcm.len() > cap {
            let drop = pcm.len() - INTS_PER_FRAME * PREROLL_FRAMES;
            pcm.drain(..drop);
            log::debug!("audio uplink: overrun, dropped {drop} samples");
        }

        // 3. Preroll once; after that stay in the playing state even across
        //    short silences, so a talker's first syllable is not re-buffered
        //    every sentence.
        if !playing && pcm.len() >= INTS_PER_FRAME * PREROLL_FRAMES {
            playing = true;
            log::info!(
                "audio: uplink playing ({} ms buffered)",
                pcm.len() * 1000 / (SAMPLE_RATE as usize * CHANNELS as usize)
            );
        }

        unsafe { output.write(&mut pcm, playing)? };
        std::thread::sleep(Duration::from_millis(WRITE_TICK_MS));
    }
    log::info!("audio: uplink thread exiting");
    Ok(())
}

/// Append one decoded frame (interleaved i16) to the playback queue.
fn push_frame(pcm: &mut VecDeque<i16>, decoded: &[i16], samples_per_channel: usize) {
    let end = samples_per_channel * CHANNELS as usize;
    if end <= decoded.len() {
        pcm.extend(decoded[..end].iter().copied());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #108：一条编码结果要能同时到达多个下行，且 detach 只摘掉自己的那一个。
    ///
    /// 这条测试刻意只挂 stream sink：`attach(track)` 需要一条真的
    /// `TrackLocalStaticSample`，而写 track 又要一个运行中的 `PeerConnection`
    /// 才有意义。流的路径才是这次改造真正新增的那一半。
    #[tokio::test]
    async fn one_encoded_frame_reaches_every_attached_sink() {
        let fanout = AudioFanout::new();
        let (tx_a, mut rx_a) = mpsc::channel(4);
        let (tx_b, mut rx_b) = mpsc::channel(4);
        let a = fanout.attach_stream(tx_a);
        let b = fanout.attach_stream(tx_b);
        assert_eq!(fanout.len(), 2);

        fanout.write(Bytes::from_static(&[0xaa, 0xbb]), &tokio::runtime::Handle::current());

        assert_eq!(rx_a.recv().await.unwrap().as_ref(), &[0xaa, 0xbb]);
        assert_eq!(rx_b.recv().await.unwrap().as_ref(), &[0xaa, 0xbb]);

        assert!(fanout.detach(a));
        assert_eq!(fanout.len(), 1);
        assert!(!fanout.detach(a), "第二次摘同一个 id 是无操作，不是错误");

        fanout.write(Bytes::from_static(&[0xcc]), &tokio::runtime::Handle::current());
        assert_eq!(rx_b.recv().await.unwrap().as_ref(), &[0xcc]);
        assert!(rx_a.recv().await.is_none(), "摘掉的 sink 不再收到任何东西");
        assert!(fanout.detach(b), "b 仍然挂着，摘它必须成功");
        assert_eq!(fanout.len(), 0);
    }

    /// 队列满时丢的是**这一帧**，而不是把发送方堵死：音频线程必须保持 20 ms
    /// 的节奏，一个慢 socket 不能拖住整条链路。
    #[tokio::test]
    async fn a_full_sink_drops_its_frame_instead_of_blocking_the_encoder() {
        let fanout = AudioFanout::new();
        let (tx, _rx) = mpsc::channel(1);
        fanout.attach_stream(tx);
        let handle = tokio::runtime::Handle::current();
        // 第一条进队，第二、三条必然落空；`write` 仍然返回。
        for _ in 0..3 {
            fanout.write(Bytes::from_static(&[1, 2, 3]), &handle);
        }
    }
}
