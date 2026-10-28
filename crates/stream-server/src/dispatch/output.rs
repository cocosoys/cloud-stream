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
    /// #78: capture -> written to the track, mean / min / max of the window.
    /// Unlike `pipeline_ms` this includes the hand-off queue wait, so the
    /// difference between the two is the transport's backlog.
    pub send_ms: f32,
    pub send_min_ms: f32,
    pub send_max_ms: f32,
    /// #78: payload actually produced, its encoder target, and the widest
    /// 250 ms of the window — all Mbps.
    pub video_mbps: f32,
    pub target_mbps: f32,
    pub peak_mbps: f32,
    /// #78: smoothed control-channel RTT; `None` until the first pong.
    pub rtt_ms: Option<f32>,
    /// #78: PLI/FIR received this session (the panel's "Keyframe requests").
    pub keyframe_requests: u64,
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
        self.send_ms = snap.send_ms;
        self.send_min_ms = snap.send_min_ms;
        self.send_max_ms = snap.send_max_ms;
        self.video_mbps = snap.video_mbps;
        self.target_mbps = snap.target_mbps;
        self.peak_mbps = snap.peak_mbps;
        self.keyframe_requests = snap.pli_requests;
        self
    }

    /// #78: attach the control-channel RTT and the queue-wait figure. Kept
    /// apart from `with_pipeline` because the probe lives in the signaling
    /// session, not in the pipeline.
    pub fn with_rtt(mut self, rtt_ms: Option<f32>) -> Self {
        self.rtt_ms = rtt_ms;
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
///
/// #78: the field names are the frontend's, not ours. `lib/stream-stats-view.js`
/// reads `video_mbps` / `target_mbps` / `peak_mbps` / `send_ms` (+ its
/// `send_min_ms` / `send_max_ms` range) / `rtt_ms` / `keyframe_requests` off the
/// sample it merged from this payload, so a renamed key is a tile that draws a
/// dash forever. `rtt_ms` is omitted while no pong has come back, which the
/// panel renders as "–" rather than a fake 0 ms.
pub fn stream_stats(s: &StreamStats) -> String {
    let mut data = json!({
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
        // Capture -> written, with the range the panel prints under the tile.
        "send_ms": r1(s.send_ms),
        "send_min_ms": r1(s.send_min_ms),
        "send_max_ms": r1(s.send_max_ms),
        // Produced rate against the encoder's target; the peak is the widest
        // 250 ms, which is what a burst actually hits on the wire.
        "video_mbps": r2(s.video_mbps),
        "target_mbps": r2(s.target_mbps),
        "peak_mbps": r2(s.peak_mbps),
        "keyframe_requests": s.keyframe_requests,
        "throttled": s.throttled,
    });
    if let (Some(rtt), Some(obj)) = (s.rtt_ms, data.as_object_mut()) {
        obj.insert("rtt_ms".into(), json!(r1(rtt)));
    }
    // gpu_percent / gpu_mem_used / gpu_mem_total stay out on purpose: no GPU
    // passthrough, so there is nothing measured to report.
    data.to_string()
}

/// One decimal, the precision the panel's time tiles print. Sending raw f32
/// means a sample carries the full float noise for a figure shown as "12.3".
fn r1(v: f32) -> f64 {
    ((v as f64) * 10.0).round() / 10.0
}

/// Two decimals: every Mbps figure is displayed with two.
fn r2(v: f32) -> f64 {
    ((v as f64) * 100.0).round() / 100.0
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
///
/// #71 — `manual_resolution` MUST stay `false` here.
/// This server does not pin a framebuffer: `run_blocking_loop` sizes every
/// beat as `min(client request, native display)` and follows the client when it
/// asks for something else. Announcing `true` therefore does not describe a
/// capability, it *locks the client into manual mode*: `wr-core` reads the flag
/// (`Applying server-enforced manual resolution`) and calls
/// `applyManualStyle(manual_width, manual_height, scaleLocally)`, so the stream
/// is drawn at a fixed CSS box with `object-fit: contain` instead of following
/// the window. Worse, `manual_width/height` below echo `s.width()/height()`,
/// which on the next connect is whatever the client asked for last time — a
/// self-referencing loop that froze a session at 1920x1072 (its window height
/// captured once, then re-imposed forever, `scale=true` on a 1920x1080 host).
/// Leaving it `false` keeps the frontend on its window-following path, which is
/// what this pipeline actually implements.
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
                "manual_resolution": { "value": false, "locked": false },
                // Kept as a read-only *report* of the live request, not as a
                // framebuffer to adopt: the frontend shows it, and ignores it
                // while `manual_resolution` is false.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::PipelineSnapshot;

    fn snapshot() -> PipelineSnapshot {
        PipelineSnapshot {
            encoded_fps: 59.5,
            encode_ms: 6.25,
            pipeline_ms: 18.5,
            send_ms: 19.75,
            send_min_ms: 12.0,
            send_max_ms: 41.0,
            video_mbps: 2.4567,
            target_mbps: 5.0,
            peak_mbps: 9.874,
            pli_requests: 3,
            total_frames: 60,
            warm: true,
        }
    }

    /// The names are the frontend's (`lib/stream-stats-view.js` reads them off
    /// the merged sample), so a rename is a tile that shows a dash forever.
    #[test]
    fn stats_carry_every_field_the_panel_reads() {
        let line = stream_stats(&StreamStats::default().with_pipeline(&snapshot()));
        for key in [
            "encode_ms",
            "pipeline_ms",
            "send_ms",
            "send_min_ms",
            "send_max_ms",
            "video_mbps",
            "target_mbps",
            "peak_mbps",
            "keyframe_requests",
            "encoded_fps",
            "cpu_percent",
            "mem_used",
            "mem_total",
            "throttled",
        ] {
            assert!(line.contains(&format!("\"{key}\"")), "missing {key} in {line}");
        }
    }

    /// An unmeasured RTT must be absent, not zero: the panel draws 0 ms as a
    /// real round trip, and this channel may simply never have answered.
    #[test]
    fn rtt_is_omitted_until_measured() {
        let no_rtt = stream_stats(&StreamStats::default().with_pipeline(&snapshot()));
        assert!(!no_rtt.contains("rtt_ms"), "fake rtt in {no_rtt}");

        let with_rtt = stream_stats(
            &StreamStats::default()
                .with_pipeline(&snapshot())
                .with_rtt(Some(23.456)),
        );
        assert!(with_rtt.contains("\"rtt_ms\":23.5"), "unexpected: {with_rtt}");
    }

    /// Two decimals on rates, one on times: the payload should not carry f32
    /// noise for a figure the panel prints as "2.46".
    #[test]
    fn rates_are_rounded_to_the_displayed_precision() {
        let line = stream_stats(&StreamStats::default().with_pipeline(&snapshot()));
        assert!(line.contains("\"video_mbps\":2.46"), "unexpected: {line}");
        assert!(line.contains("\"peak_mbps\":9.87"), "unexpected: {line}");
        assert!(line.contains("\"send_ms\":19.8"), "unexpected: {line}");
    }
}
