//! Outbound message builders. Only the four whitelisted types from design 5.3
//! may ever reach the data channel; design 5.7 forbids every other type because
//! the frontend logs `Unhandled message received` for unknown ones.
//!
//! All outbound frames are UTF-8 text JSON in stage1 (design 5.6). The builders
//! return the serialized line so the caller hands it to `send_text`.

use hbb_common::serde_json::json;

use crate::pipeline::PipelineSnapshot;
use crate::platform::host::capture_backend_name;
use crate::platform::{node_name, HostSample};

/// `stream_info` one-shot snapshot (design 5.3.1). Fields the panel treats as
/// required are populated; the renderer group is omitted on Windows.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    /// Capture backend short name: `"dxgi"` or `"gdi"`.
    pub capture: &'static str,
    /// Whether the active capturer is the GDI fallback (drives `capture` above).
    pub is_gdi: bool,
    /// Capture-only explanatory reason; empty on the normal DXGI path.
    pub capture_reason: String,
}

impl Default for StreamInfo {
    fn default() -> Self {
        Self {
            capture: "dxgi",
            is_gdi: false,
            capture_reason: String::new(),
        }
    }
}

impl StreamInfo {
    /// Build the snapshot from the live capturer backend (#42-1 real backfill).
    /// `reason` is the short English switch tag from the recovery chain; empty on
    /// the normal path.
    pub fn from_capture(is_gdi: bool, reason: impl Into<String>) -> Self {
        Self {
            capture: capture_backend_name(is_gdi),
            is_gdi,
            capture_reason: reason.into(),
        }
    }
}

/// Per-second `stream_stats` server sample (design 5.4). Client-side fps,
/// present_ms, mbps and received_mb are collected by the browser and must not
/// be forged here.
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamStats {
    /// Host-wide CPU utilisation, percent of all logical cores (0..=100). The
    /// panel's `CPU` meter reads this: see `platform::sysinfo` for why it is not
    /// this process's per-core figure.
    pub cpu_percent: f32,
    /// This process alone, percent of one core. Diagnostics ("is the stream
    /// server the one eating the CPU"), not the meter.
    pub process_cpu_percent: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    /// fps(total): new frames plus repeat frames over the stat window.
    pub encoded_fps: f32,
    /// New-frame `encode_to_message` mean cost, milliseconds.
    pub encode_ms: f32,
    /// Capture + YUV conversion + encode end-to-end mean cost, milliseconds.
    pub pipeline_ms: f32,
    /// Stage1 has no QoS throttle; always false.
    pub throttled: bool,
}

impl StreamStats {
    /// Fill the host memory / CPU part from a sampler sample; pipeline numbers
    /// are attached by the stats aggregator in `pipeline::stats`.
    pub fn from_host(host: HostSample) -> Self {
        Self {
            cpu_percent: host.cpu_percent,
            process_cpu_percent: host.process_cpu_percent,
            mem_used: host.mem_used,
            mem_total: host.mem_total,
            throttled: false,
            ..Default::default()
        }
    }

    /// Attach the pipeline-side fps / encode / pipeline means measured over the
    /// current stats window; host fields are untouched.
    pub fn with_pipeline(mut self, snap: &PipelineSnapshot) -> Self {
        self.encoded_fps = snap.encoded_fps;
        self.encode_ms = snap.encode_ms;
        self.pipeline_ms = snap.pipeline_ms;
        self
    }
}

/// `pipeline` ready notice sent once the PC reaches connected state.
pub fn pipeline(status: &str) -> String {
    json!({"type":"pipeline","data":{"status": status}}).to_string()
}

/// One-shot `stream_info` snapshot sent after connected; a missing snapshot is
/// exactly why the old panel showed "encoder unavailable / capture unavailable".
pub fn stream_info(info: &StreamInfo) -> String {
    json!({
        "type": "stream_info",
        "data": {
            "backend": "windows",
            "capture": info.capture,
            "is_gdi": info.is_gdi,
            "zero_copy": false,
            "zero_copy_available": false,
            "capture_reason": info.capture_reason,
            "encoder": "libvpx",
            "hardware": false,
            "gpu_present": false,
            "hardware_expected": false,
            "encoder_reason": "CPU software encoder; no GPU passthrough",
            "codec": "VP8",
            "fullcolor": false,
            "bit_depth": 8,
            "striped": false,
            "gpu": "",
            "driver": "",
            "encode_node": node_name(),
        }
    })
    .to_string()
}

/// Periodic `stream_stats` sample; sent only while `_stats,1` is active.
pub fn stream_stats(s: &StreamStats) -> String {
    let v = json!({
        "type": "stream_stats",
        "data": {
            // Whole-machine share, the figure the panel's CPU meter shows.
            "cpu_percent": s.cpu_percent,
            // Per-core figure for this process; the panel keeps it for the
            // tooltip/diagnostics and never draws it as a share of the host.
            "process_cpu_percent": s.process_cpu_percent,
            "mem_used": s.mem_used,
            "mem_total": s.mem_total,
            "encoded_fps": s.encoded_fps,
            "encode_ms": s.encode_ms,
            "pipeline_ms": s.pipeline_ms,
            "throttled": s.throttled,
        }
    });
    // gpu_percent / gpu_mem_used / gpu_mem_total / rtt_ms are omitted on
    // purpose: no GPU passthrough, stage1 runs no RTT measurement (design 5.4).
    v.to_string()
}

/// `connection` state notice; stage1 always reports a healthy link.
pub fn connection(poor: bool) -> String {
    json!({"type":"connection","data":{"poor": poor}}).to_string()
}

/// `server_settings` capabilities payload (#50). The frontend consumes
/// `settings.framerate.min/max` for the frame-rate options and switches to
/// manual resolution mode when `settings.manual_resolution.value === true`,
/// adopting `manual_width/height` as the fixed framebuffer. The values are the
/// *live* `StreamSettings`, so a client-requested fps / resolution change is
/// re-announced here without a reconnect.
pub fn server_settings(s: &crate::pipeline::StreamSettings) -> String {
    json!({
        "type": "server_settings",
        "data": {
            "settings": {
                "framerate": {
                    "value": s.fps(),
                    "min": 10,
                    "max": 60,
                    "locked": false
                },
                "manual_resolution": { "value": true, "locked": false },
                "manual_width": { "value": s.width(), "locked": false },
                "manual_height": { "value": s.height(), "locked": false },
                "enable_resize": { "value": true, "locked": false },
                "video_fullcolor": { "value": false, "locked": true },
                "video_10bit": { "value": false, "locked": true },
                "use_cpu": { "value": true, "locked": true },
                "webcam_encoder": { "value": "vp9", "locked": true }
            }
        }
    })
    .to_string()
}

/// `system` action notice for capture recovery / fallback / lost / restored
/// events (#42-1). Reuses the existing frontend `system{action}` route
/// (`lib/webrtc.js:767-774`); the action string itself carries the comma-separated
/// `<action>,<detail>` whitelist from the design document section 4.5.1.
pub fn system(action: &str) -> String {
    json!({"type":"system","data":{"action": action}}).to_string()
}

/// `cursor` message (#51 C2). The frontend `updateServerCursor`
/// (`lib/input.js:1902`) hides the remote cursor when `curdata` is empty or
/// `handle === 0`, otherwise renders `curdata` (base64 PNG) with the hotspot.
/// The PNG is encoded by the capture worker on identity change; an id-0 cursor
/// carries an empty payload.
pub fn cursor(cd: &crate::pipeline::CursorData) -> String {
    let (curdata, w, h): (String, usize, usize) = if cd.id == 0 {
        (String::new(), 0, 0)
    } else {
        (crate::pipeline::cursor_png_b64(cd), cd.width, cd.height)
    };
    json!({
        "type": "cursor",
        "data": {
            "curdata": curdata,
            "width": w,
            "height": h,
            "hotx": cd.hotx,
            "hoty": cd.hoty,
            "handle": cd.id,
        }
    })
    .to_string()
}
