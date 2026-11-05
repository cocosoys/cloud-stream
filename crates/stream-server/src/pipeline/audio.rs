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
//! Known limit, by construction: the downlink loopback captures the **default**
//! render endpoint's post-mix signal, so anything written there comes back one
//! round trip later. Writing the microphone to the default speaker therefore
//! makes a player hear their own voice (#109). The fix is topological, not
//! algorithmic: write to a *virtual cable* endpoint instead
//! (`platform::virtual_mic` resolves one — `Steam Streaming Microphone`,
//! `CABLE Input`, ...), whose capture side the game reads as a microphone. The
//! injected signal then never reaches the default endpoint, and the loop does
//! not exist. `--mic-device default` restores the old behaviour for talk-back,
//! where the echo is inherent and accepted.
//!
//! #110 adds the same uplink to the WebSocket transport: `[0x02] + Opus` arrives
//! on the media socket instead of on an RTP track. Only the data source differs —
//! decoding, loss handling and the render watermark all go through one `pump`,
//! because "what gets written to the endpoint" must not depend on the transport.
//! The page encodes **24 kHz mono** at 20 ms; the decoder is created at 48 kHz
//! stereo and Opus resamples/upmixes it, so there is no resampler here.

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
/// 注入端只维持这么深的排队。设备缓冲是 100 ms，但把它填满会让数据与静音交替
/// 出现（见 [`Output::write`]）。50 ms 足够吸收一次调度抖动（10 个 5 ms tick），
/// 又比填满少一半延迟。
const RENDER_TARGET_MS: u32 = 50;
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
/// WS 上行的队列深度：8 包 = 160 ms。比 RTP 那条（64）短得多，因为 WS 是可靠
/// 有序传输，不存在"迟到但有用"的包 —— 迟到的语音只会比丢掉更糟。
const WS_MIC_QUEUE: usize = 8;
/// WS 上行空闲这么久就关掉 render 客户端。
///
/// 页面的 worklet 在连续静音后会停发（`SILENCE_THRESHOLD_CHUNKS`），所以"没有包"
/// 本身就是正常状态。一直挂着 render 客户端等于一直往端点写静音：白占一个音频
/// 客户端，还会让本已闲置的端点保持唤醒。1.5 s 是"一句话之间的停顿"和"人走开了"
/// 的分界，重开的代价只是一次 `IAudioClient::Start`。
const WS_MIC_IDLE: Duration = Duration::from_millis(1_500);
/// 一条 Opus 包最长 1275 字节（RFC 6716 §3.4）；再大的一定不是麦克风数据。
/// WS 上行不经过 RTP 的 MTU 约束，所以这里要自己挡一下，免得一条畸形帧把
/// 解码器喂到 panic 或者把队列占满。
const MAX_OPUS_BYTES: usize = 1_500;

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
/// Opus 解不开的包（畸形、或者编码器版本不匹配）。分出来单独计数，因为
/// "包到了但解不出"和"包没到"是两条完全不同的排查路径。
static UPLINK_DECODE_ERR: AtomicU64 = AtomicU64::new(0);
/// render 客户端当前是不是开着。WS 上行按需打开、空闲关闭，运维要能看出
/// "现在到底有没有人开麦"，否则 `packets` 一直涨而声音不来时无从判断。
static UPLINK_OPEN: AtomicBool = AtomicBool::new(false);
/// 最后一包的时间。用 `Mutex<Option<Instant>>` 而不是原子毫秒数：这里没有
/// 每秒百万次的读写，一个锁比"自己找个时间原点再换算"便宜也清楚。
static UPLINK_LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Opus accepts 6k–510k for stereo; the ceiling is capped at 128k because this
/// carries game audio, not music, and the panel's own default is 128k.
const MIN_BPS: i32 = 6_000;
const MAX_BPS: i32 = 128_000;

/// #110: the built-in target, before any client pick.
///
/// Exposed because the *program* now owns the live value
/// (`StreamSettings.audio_bps`) and has to seed itself from here — one place
/// knows what "default" means, and it is this one.
pub fn default_bitrate_bps() -> i32 {
    BITRATE_BPS
}

/// #110: `ab,<N>` → a target the encoder will actually take.
///
/// `<= 0` means "back to the built-in default"; anything else is clamped into
/// the Opus range, so the panel's slider can only hold values the encoder
/// accepts. The value itself lives per program now — see
/// [`crate::pipeline::settings::StreamSettings::set_audio_bitrate_bps`].
pub fn clamp_bps(bps: i32) -> i32 {
    if bps <= 0 {
        BITRATE_BPS
    } else {
        bps.clamp(MIN_BPS, MAX_BPS)
    }
}

/// The stops the panel's audio-bitrate slider indexes into, in bps.
///
/// Advertised in `server_settings` so the track can only hold values this
/// server will actually take: everything here is inside `MIN_BPS..=MAX_BPS`,
/// and nothing outside it is offered. The upstream list went to 510k, which
/// [`clamp_bps`] silently clamps to 128k — a stop that lies about what it did.
pub const BITRATE_STEPS: [u32; 5] = [32_000, 48_000, 64_000, 96_000, 128_000];

/// (captured 20 ms frames, encoded packets, encoded bytes, silence-flagged frames).
pub fn totals() -> (u64, u64, u64, u64) {
    (
        CAPTURED_FRAMES.load(Ordering::Relaxed),
        ENCODED_PACKETS.load(Ordering::Relaxed),
        ENCODED_BYTES.load(Ordering::Relaxed),
        SILENT_FRAMES.load(Ordering::Relaxed),
    )
}

/// #79 上行的一次快照。字段名与 `health` 的 JSON 一一对应。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UplinkStats {
    /// 收到的 Opus 包（RTP 与 WS 两条路合计）。
    pub packets: u64,
    /// 解出来的 20 ms 帧。
    pub frames: u64,
    /// 丢包伪造的帧（只有 RTP 会非零：WS 不会丢包，只会排队溢出）。
    pub concealed: u64,
    /// 端点里被补零的帧数。
    pub silence: u64,
    /// 队列满而丢掉的包。
    pub dropped: u64,
    /// 解不开的包。
    pub decode_errors: u64,
    /// render 客户端现在开着没有。
    pub open: bool,
    /// 最后一包到现在多久了；一包都没收到过时是 `None`。
    pub last_packet_age_ms: Option<u64>,
}

pub fn uplink_stats() -> UplinkStats {
    UplinkStats {
        packets: UPLINK_PACKETS.load(Ordering::Relaxed),
        frames: UPLINK_FRAMES.load(Ordering::Relaxed),
        concealed: UPLINK_CONCEALED.load(Ordering::Relaxed),
        silence: UPLINK_SILENCE.load(Ordering::Relaxed),
        dropped: UPLINK_DROPPED.load(Ordering::Relaxed),
        decode_errors: UPLINK_DECODE_ERR.load(Ordering::Relaxed),
        open: UPLINK_OPEN.load(Ordering::Relaxed),
        last_packet_age_ms: UPLINK_LAST
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .map(|at| at.elapsed().as_millis().min(u64::MAX as u128) as u64),
    }
}

/// 这个进程现在能不能收麦克风上行。
///
/// `Err(note)` 里是给页面/运维看的原因。两条拒绝的理由必须分开：
/// 没有虚拟录音设备（装什么），或者虚拟线缆同时是默认播放端点（改哪设置）。
/// 两者都表现为"开麦没声音"，但修法完全不同。
pub fn uplink_available() -> std::result::Result<(), String> {
    match crate::platform::virtual_mic::target() {
        Some(target) if !target.usable => Err(target.note.clone()),
        _ => Ok(()),
    }
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
/// #110: start this program's audio downlink.
///
/// `bps` and `enabled` are the *program's* atomics (`StreamSettings`), not
/// process-wide statics: this thread is already one per program (one loopback
/// capture feeding every sink on it), so a value shared across programs meant a
/// viewer muting their own stream silenced the other screen too.
pub fn spawn(
    fanout: Arc<AudioFanout>,
    stop: Arc<AtomicBool>,
    bps: Arc<AtomicI32>,
    enabled: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let handle = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("audio-loopback".to_owned())
        .spawn(move || {
            if let Err(err) = run(fanout, stop, handle, bps, enabled) {
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
    bps: Arc<AtomicI32>,
    enabled: Arc<AtomicBool>,
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
        let want_bps = bps.load(Ordering::Relaxed);
        if want_bps != applied_bps {
            match encoder.set_bitrate(Bitrate::BitsPerSecond(want_bps)) {
                Ok(()) => {
                    applied_bps = want_bps;
                    log::info!("audio: encoder bitrate -> {want_bps} bps");
                }
                Err(err) => log::warn!("audio: set_bitrate({want_bps}) failed: {err}"),
            }
        }
        let now_enabled = enabled.load(Ordering::Relaxed);
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
    /// Open a render endpoint at 48 kHz / 16-bit / stereo.
    ///
    /// #109: the endpoint is no longer necessarily the default speaker. The
    /// injected microphone is mixed into whatever endpoint we write to, and the
    /// downlink loopback captures the *default* endpoint's post-mix signal — so
    /// writing to the default speaker guarantees the player hears themselves one
    /// round trip later. `platform::virtual_mic` resolves a virtual cable
    /// instead (`Steam Streaming Microphone`, `CABLE Input`, ...), whose capture
    /// side the game reads as a microphone; the default speaker never sees the
    /// injected signal, and the loop does not exist.
    ///
    /// `None` from `sink_name()` keeps the old behaviour and is only reachable
    /// through `--mic-device default` (talk-back use, echo accepted).
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
        let device = match crate::platform::virtual_mic::sink_name() {
            Some(name) => {
                let picked = crate::platform::virtual_mic::find_render(&enumerator, &name)?;
                log::info!("audio: uplink sink = {name:?} (virtual microphone)");
                picked
            }
            None => enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("GetDefaultAudioEndpoint(eRender, eConsole) — no active output device?")?,
        };
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

    /// Hand the endpoint one low-watermark refill.
    ///
    /// # 两条规则，都是上一版踩出来的
    ///
    /// 1. **只补到 `RENDER_TARGET_MS`，不填满 100 ms 的设备缓冲。** 上一版取
    ///    `free = buffer - padding` 把 free 全填上，于是缓冲里永远是
    ///    `数据(20 ms) + 静音(80 ms)` 的条纹：设备按实时速率消费，每 100 ms 里
    ///    只有前 20 ms 有声音。实测 4.8 s 连续语音被补了 58560 个采样/声道
    ///    （≈1.2 s）静音，听起来是断续的"哒哒"声。
    /// 2. **播放中绝不提前补静音。** 只填到目标深度还不够：麦克风每 20 ms 来一包，
    ///    而这里 5 ms 转一圈，三次里有三次手上没数据，若照样"补到位"，静音就会
    ///    始终领在数据前面 —— 实测改成 50 ms 目标后静音反而涨到 123840。正确做法
    ///    是播放中只写手上真有的帧（没数据就什么都不写，让缓冲自然排空），静音
    ///    只用于"还没开始播"时给端点保活。
    ///
    /// 副作用是把注入延迟从 100 ms 降到 50 ms，对"开麦听到自己"也是好事。
    unsafe fn write(&self, pcm: &mut VecDeque<i16>, playing: bool) -> Result<()> {
        let padding = self.client.GetCurrentPadding().unwrap_or(0);
        let target = ((SAMPLE_RATE as u64 * RENDER_TARGET_MS as u64 / 1000) as u32)
            .min(self.buffer_frames);
        if padding >= target {
            return Ok(());
        }
        let want = (target - padding) as usize;
        let ready = pcm.len() / CHANNELS as usize;

        // 播放中只写真实数据；未开播时用静音把端点喂着，避免 stream 一起一停。
        let frames = if playing { ready.min(want) } else { want };
        if frames == 0 {
            // 播放中且手上没数据：这一轮不写。写静音会把"没声音"提前 50 ms
            // 铺进缓冲，下一包到达时只能排在它后面。
            return Ok(());
        }

        let ptr = self.render.GetBuffer(frames as u32)?;
        if ptr.is_null() {
            return Ok(());
        }
        let dst = std::slice::from_raw_parts_mut(ptr as *mut i16, frames * CHANNELS as usize);
        if playing {
            for (slot, sample) in dst.iter_mut().zip(pcm.drain(..frames * CHANNELS as usize)) {
                *slot = sample;
            }
        } else {
            for slot in dst.iter_mut() {
                *slot = 0;
            }
            UPLINK_SILENCE.fetch_add(frames as u64, Ordering::Relaxed);
        }
        // 释放的是写进去的帧数，不是整个缓冲区：多报会让引擎把没写的部分当成
        // 已填充的数据播出去。
        self.render.ReleaseBuffer(frames as u32, 0)?;
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

/// Decode + render loop for the RTP path (WebRTC).
///
/// Owns the Opus decoder (it is not `Send`, which is why this lives on this
/// thread rather than being created by the reader).
fn run_uplink(stop: Arc<AtomicBool>, rx: Receiver<(u16, Bytes)>) -> Result<()> {
    // RTP 那侧没有空闲概念：会话结束就是麦克风结束，所以设备启动即开。
    pump(stop, rx, None)
}

/// `UPLINK_OPEN` 的守卫。线程以任何方式退出（包括 `?` 提前返回）都要把标志复位，
/// 否则 health 会一直报"麦克风开着"，而其实线程早就死了 —— 那比没有这个字段更糟。
struct OpenFlag;

impl Drop for OpenFlag {
    fn drop(&mut self) {
        UPLINK_OPEN.store(false, Ordering::Relaxed);
    }
}

/// 解码 + 写设备的公共泵。
///
/// `idle = None` 是 RTP：设备一开就留着。`idle = Some(d)` 是 WS：第一包到达才开
/// 设备（页面的 worklet 在静音时会停发，连接长时间没有麦克风数据是常态），
/// 连续 `d` 没有数据就关掉，免得一直往端点写静音。
fn pump(
    stop: Arc<AtomicBool>,
    rx: Receiver<(u16, Bytes)>,
    idle: Option<Duration>,
) -> Result<()> {
    // #109：没有可用的虚拟录音设备时**不**静默退回默认扬声器 —— 那正是自听
    // 的成因，而且退回之后一切看起来都正常，只是每次开麦都听见自己。
    uplink_available().map_err(|note| anyhow!("麦克风不可用：{note}"))?;
    let lazy = idle.is_some();
    let _flag = OpenFlag;
    let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Stereo)
        .map_err(|e| anyhow!("opus decoder: {e}"))?;
    let mut decoded = vec![0i16; MAX_DECODE_INTS];
    let mut pcm: VecDeque<i16> = VecDeque::with_capacity(INTS_PER_FRAME * MAX_BUFFERED_FRAMES);
    let mut last_seq: Option<u16> = None;
    let mut playing = false;
    let mut output: Option<Output> = if lazy {
        None
    } else {
        let out = unsafe { Output::open() }?;
        UPLINK_OPEN.store(true, Ordering::Relaxed);
        Some(out)
    };
    // 打开设备的最后一次尝试。设备打不开（端点被拔掉、驱动在重装）时不能
    // 每 5 ms 重试一次 —— 那会把日志刷爆，也会让一次坏调用变成忙等。
    // `None` = 还没试过。
    let mut last_open: Option<std::time::Instant> = None;
    let mut last_packet = std::time::Instant::now();
    let mut heard_any = false;

    while !stop.load(Ordering::Relaxed) {
        // 1. Take everything the network delivered since the last tick.
        loop {
            match rx.try_recv() {
                Ok((seq, payload)) => {
                    heard_any = true;
                    last_packet = std::time::Instant::now();
                    *UPLINK_LAST.lock().unwrap_or_else(|p| p.into_inner()) = Some(last_packet);
                    // Loss concealment: one fabricated frame per missing packet,
                    // capped at 3. A long gap is a stall or a mute, not loss —
                    // inventing 30 frames of noise for it would be worse than
                    // the silence. (WS 是一条可靠有序的流，序号是本地递增的，
                    // `missing` 恒为 0，这一段只对 RTP 生效。)
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
                        Err(err) => {
                            UPLINK_DECODE_ERR.fetch_add(1, Ordering::Relaxed);
                            log::debug!("opus decode: {err}");
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    log::info!("audio: mic writer ended (sender gone)");
                    return Ok(());
                }
            }
        }

        // 2. 按需开设备。打开失败不是致命的：端点可能是后插上的，也可能是
        //    驱动正在重装 —— 每 2 s 再试一次，比"第一包没开成，这个会话就哑了"
        //    更接近运维期待的行为。
        let due = last_open.map_or(true, |at| at.elapsed() >= Duration::from_secs(2));
        if output.is_none() && heard_any && due {
            last_open = Some(std::time::Instant::now());
            match unsafe { Output::open() } {
                Ok(out) => {
                    output = Some(out);
                    UPLINK_OPEN.store(true, Ordering::Relaxed);
                    playing = false;
                }
                Err(err) => log::warn!("audio: uplink render not open yet: {err:#}"),
            }
        }

        // 3. Overrun: the queue outgrew the cap, so drop the oldest audio and
        //    keep the floor. Keeping the newest is what makes a burst audible
        //    instead of late.
        let cap = INTS_PER_FRAME * MAX_BUFFERED_FRAMES;
        if pcm.len() > cap {
            let drop = pcm.len() - INTS_PER_FRAME * PREROLL_FRAMES;
            pcm.drain(..drop);
            log::debug!("audio uplink: overrun, dropped {drop} samples");
        }

        // 4. Preroll once; after that stay in the playing state even across
        //    short silences, so a talker's first syllable is not re-buffered
        //    every sentence.
        if !playing && pcm.len() >= INTS_PER_FRAME * PREROLL_FRAMES {
            playing = true;
            log::info!(
                "audio: uplink playing ({} ms buffered)",
                pcm.len() * 1000 / (SAMPLE_RATE as usize * CHANNELS as usize)
            );
        }

        if let Some(out) = &output {
            unsafe { out.write(&mut pcm, playing)? };
        }

        // 5. 空闲关设备（只有 WS 那条会走到）。判断放在写之后：这一轮刚写进去的
        //    数据必须先进端点，否则"最后一句话"会被这段逻辑吃掉。
        if let Some(idle) = idle {
            if output.is_some() && last_packet.elapsed() > idle {
                output = None;
                UPLINK_OPEN.store(false, Ordering::Relaxed);
                playing = false;
                pcm.clear();
                log::info!(
                    "audio: uplink idle {} ms, render closed",
                    last_packet.elapsed().as_millis()
                );
            }
        }

        std::thread::sleep(Duration::from_millis(WRITE_TICK_MS));
    }
    log::info!("audio: uplink thread exiting");
    Ok(())
}

// ---------------------------------------------------------------------------
// #110 WS 上行：页面 -> `0x02` Opus -> 解码 -> 写虚拟录音设备
// ---------------------------------------------------------------------------

/// 一条 WS 连接上的麦克风注入点。
///
/// 与 RTP 那条路的差别只有数据来源：RTP 由 `TrackRemote` 推，这里由 WS 的二进制
/// 帧推。解码、丢帧策略、写设备全走同一个 [`pump`]，因为"往端点写什么"不该因为
/// 传输不同而不同 —— 那正是两套实现开始漂移的起点。
///
/// 每会话一个实例（`WinInputSink` 也是每会话一个，理由相同：状态不能共享）。
/// 两个页签同时开麦时，两个 render 客户端写同一个端点，引擎把它们混在一起 ——
/// 这与"两个人都对着同一个麦克风说话"的语义一致。
pub struct MicUplink {
    tx: SyncSender<(u16, Bytes)>,
    stop: Arc<AtomicBool>,
    /// 本地递增。WS 是有序的，序号只用于让 `pump` 的丢包补偿逻辑保持沉默。
    seq: u16,
    /// 有没有真的收到过包。用于区分"页面开了麦但一帧没发"和"根本没开麦"。
    packets: u64,
}

impl MicUplink {
    /// 起一条上行。目标不可用时返回原因，调用方应该回 `MICROPHONE_DISABLED`
    /// 并把它写进日志 —— 静默失败会让用户盯着一个不动的电平条。
    pub fn start() -> std::result::Result<Self, String> {
        uplink_available()?;
        let (tx, rx) = sync_channel::<(u16, Bytes)>(WS_MIC_QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        std::thread::Builder::new()
            .name("audio-mic-ws".to_owned())
            .spawn(move || {
                if let Err(err) = pump(thread_stop, rx, Some(WS_MIC_IDLE)) {
                    log::warn!("audio: ws uplink stopped: {err:#}");
                }
            })
            .map_err(|e| format!("启动麦克风线程失败: {e}"))?;
        Ok(Self {
            tx,
            stop,
            seq: 0,
            packets: 0,
        })
    }

    /// 送一帧 Opus。返回 `false` 表示这一帧被丢掉了（队列满或包不合法）。
    ///
    /// 不阻塞：WS 读循环一旦被音频拖住，画面帧也会跟着排队 —— 而画面比语音
    /// 更不能等。丢一帧语音的代价是一个 20 ms 的空洞，丢一帧画面的代价是花屏。
    pub fn push(&mut self, opus: &[u8]) -> bool {
        if opus.is_empty() || opus.len() > MAX_OPUS_BYTES {
            UPLINK_DROPPED.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        if self.tx.try_send((seq, Bytes::copy_from_slice(opus))).is_err() {
            UPLINK_DROPPED.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.packets += 1;
        UPLINK_PACKETS.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// 只建管道，不起线程、不碰设备。单测用：`start()` 会起一条真的去开发备的
    /// 线程，在任何 CI 机器上都不该被一个断言触发。返回接收端，测试要拿它把
    /// 队列"占满"。
    #[cfg(test)]
    fn channel_only(capacity: usize) -> (Self, Receiver<(u16, Bytes)>) {
        let (tx, rx) = sync_channel::<(u16, Bytes)>(capacity);
        (
            Self {
                tx,
                stop: Arc::new(AtomicBool::new(false)),
                seq: 0,
                packets: 0,
            },
            rx,
        )
    }

    /// 这个连接送过多少帧。
    #[cfg(test)]
    pub fn packets(&self) -> u64 {
        self.packets
    }
}

impl Drop for MicUplink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // 不 `join`：这跑在 tokio 的 worker 上，阻塞等待会把整个运行时的一格
        // 卡住最多一个 `WRITE_TICK_MS`。线程会在这一格之内看到 `stop` 自己退出，
        // 退出时 `Output` 的 `Drop` 会停掉 render 客户端。
        log::info!("audio: ws uplink closing after {} packet(s)", self.packets);
    }
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

    // -----------------------------------------------------------------------
    // #110 WS 上行
    // -----------------------------------------------------------------------

    /// 空包和超长包在入口就被挡掉：`Opus` 解码器对畸形输入的容忍度是实现细节，
    /// 不该由一条网络帧来决定。挡住的同时要计数，否则"页面在发、声音没有"这条
    /// 时间线查不下去。
    #[test]
    fn an_empty_or_oversized_mic_frame_never_reaches_the_queue() {
        let (mut mic, mut rx) = MicUplink::channel_only(4);
        let before = UPLINK_DROPPED.load(Ordering::Relaxed);

        assert!(!mic.push(&[]), "空包必须拒绝");
        assert!(!mic.push(&vec![0u8; MAX_OPUS_BYTES + 1]), "超长包必须拒绝");
        assert!(rx.try_recv().is_err(), "被拒绝的包不该进队列");

        // 1275 是 RFC 6716 的实际上限，留了余量到 1500。
        assert!(mic.push(&vec![0u8; 1_275]), "合法长度的包必须接受");
        assert_eq!(mic.packets(), 1, "只有被接受的那一帧计数");
        assert_eq!(UPLINK_DROPPED.load(Ordering::Relaxed) - before, 2);
    }

    /// 队列满时丢帧而不是阻塞：WS 的读循环一旦被音频拖住，画面帧会跟着排队 ——
    /// 画面比语音更不能等。
    #[test]
    fn a_full_mic_queue_drops_the_frame_instead_of_blocking() {
        let (mut mic, _rx) = MicUplink::channel_only(1);
        assert!(mic.push(&[0xaa]), "第一条进队");
        let before = UPLINK_DROPPED.load(Ordering::Relaxed);
        assert!(!mic.push(&[0xbb]), "队列满了必须立刻返回 false");
        assert_eq!(UPLINK_DROPPED.load(Ordering::Relaxed) - before, 1);
        assert_eq!(mic.packets(), 1);
    }

    /// #110 最关键的一条假设：页面用 **24 kHz 单声道** 编码，而设备要的是
    /// **48 kHz 立体声**。Opus 允许解码到与编码不同的采样率和声道数（解码器
    /// 自己重采样、并把单声道复制到两个声道），所以不需要在服务端再写一个
    /// 重采样器 —— 但这条假设必须由测试固定住，否则哪天换个解码器参数，
    /// 症状会是"有声音但音调快一倍"，极难往回追。
    #[test]
    fn a_24k_mono_packet_decodes_into_one_48k_stereo_frame() {
        let mut encoder = Encoder::new(SampleRate::Hz24000, Channels::Mono, Application::Voip)
            .expect("24 kHz mono encoder");
        encoder
            .set_bitrate(Bitrate::BitsPerSecond(32_000))
            .expect("bitrate");
        // 480 samples = 20 ms @ 24 kHz，正是前端 worklet 的帧长。
        let pcm: Vec<i16> = (0..480)
            .map(|i| ((i as f32 * 0.1).sin() * 8_000.0) as i16)
            .collect();
        let mut packet = vec![0u8; 1_500];
        let n = encoder.encode(&pcm[..], &mut packet[..]).expect("encode");

        let mut decoder =
            Decoder::new(SampleRate::Hz48000, Channels::Stereo).expect("48 kHz stereo decoder");
        let mut decoded = vec![0i16; MAX_DECODE_INTS];
        let samples = decoder
            .decode(Some(&packet[..n]), &mut decoded[..], false)
            .expect("decode");

        assert_eq!(samples, SAMPLES_PER_FRAME, "解出来必须是 960 个采样/声道（20 ms @ 48 kHz）");
        let mut queue: VecDeque<i16> = VecDeque::new();
        push_frame(&mut queue, &decoded, samples);
        assert_eq!(
            queue.len(),
            INTS_PER_FRAME,
            "交织后必须正好是一帧立体声（960 × 2）"
        );
        // 左、右声道同源：单声道源复制到两个声道，不是一边有声音一边静音。
        assert!(
            queue.iter().step_by(2).any(|s| *s != 0),
            "左声道不该整帧静音"
        );
        assert_eq!(
            queue.iter().step_by(2).copied().collect::<Vec<_>>(),
            queue.iter().skip(1).step_by(2).copied().collect::<Vec<_>>(),
            "单声道必须复制到两个声道"
        );
    }

    /// 一包都没收到时"最后一包多老了"是 `None`，不是 0 —— 0 会被仪表盘读成
    /// "刚刚还有包"。这两个读数一个表示"从没说过话"，一个表示"正在说话"。
    #[test]
    fn the_packet_age_is_absent_until_the_first_packet_lands() {
        // 别的测试可能已经写过全局时间戳，所以只断言"写过之后有值"这一半，
        // 另一半（None）在进程内无法稳定复现。
        let stats = uplink_stats();
        let _ = stats.last_packet_age_ms;
        *UPLINK_LAST.lock().unwrap_or_else(|p| p.into_inner()) = None;
        assert_eq!(uplink_stats().last_packet_age_ms, None);
        *UPLINK_LAST.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(std::time::Instant::now());
        assert!(uplink_stats().last_packet_age_ms.is_some());
    }
}
