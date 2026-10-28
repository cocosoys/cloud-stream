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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use audiopus::{coder::Encoder, Application, Bitrate, Channels, SampleRate};
use bytes::Bytes;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVE_FORMAT_PCM, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::webrtc::AudioTrack;

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

// Diagnostics, same shape as the video counters so one log line tells the story.
static CAPTURED_FRAMES: AtomicU64 = AtomicU64::new(0);
static ENCODED_PACKETS: AtomicU64 = AtomicU64::new(0);
static ENCODED_BYTES: AtomicU64 = AtomicU64::new(0);
static SILENT_FRAMES: AtomicU64 = AtomicU64::new(0);

/// (captured 20 ms frames, encoded packets, encoded bytes, silence-flagged frames).
pub fn totals() -> (u64, u64, u64, u64) {
    (
        CAPTURED_FRAMES.load(Ordering::Relaxed),
        ENCODED_PACKETS.load(Ordering::Relaxed),
        ENCODED_BYTES.load(Ordering::Relaxed),
        SILENT_FRAMES.load(Ordering::Relaxed),
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
pub fn spawn(track: AudioTrack, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    let handle = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("audio-loopback".to_owned())
        .spawn(move || {
            if let Err(err) = run(track, stop, handle) {
                log::warn!("audio downlink stopped: {err:#}");
            }
        })
        .expect("spawn audio-loopback thread")
}

fn run(
    track: AudioTrack,
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

    while !stop.load(Ordering::Relaxed) {
        unsafe { loopback.drain(&mut pcm)? };

        // Emit whole frames only. Whatever is left over stays in `pcm` for the
        // next signal — that carry is what keeps the sample clock honest.
        while pcm.len() >= INTS_PER_FRAME {
            let frame: Vec<i16> = pcm.drain(..INTS_PER_FRAME).collect();
            let n = encoder
                .encode(&frame, &mut opus_buf)
                .map_err(|e| anyhow!("opus encode: {e}"))?;
            if n == 0 {
                continue;
            }
            ENCODED_PACKETS.fetch_add(1, Ordering::Relaxed);
            ENCODED_BYTES.fetch_add(n as u64, Ordering::Relaxed);
            CAPTURED_FRAMES.fetch_add(1, Ordering::Relaxed);
            let data = Bytes::copy_from_slice(&opus_buf[..n]);
            if let Err(err) = handle.block_on(track.write_sample(data, Duration::from_millis(FRAME_MS as u64))) {
                log::debug!("audio write_sample: {err}");
            }
        }
    }
    Ok(())
}
