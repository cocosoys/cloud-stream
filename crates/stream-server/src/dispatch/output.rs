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
    /// #84: the QoS controller has the encoder below its base rate. Was
    /// hard-coded false until there was a controller to ask.
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

    /// #84: attach the QoS state. Same reason as `with_rtt` — the controller
    /// lives in the capture loop, which the stats task cannot see.
    pub fn with_throttled(mut self, throttled: bool) -> Self {
        self.throttled = throttled;
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
            // #94: whichever libvpx encoder the session actually opened. The
            // statistics tile reads this string, so it has to follow the live
            // pick — a tile that says VP8 over a VP9 stream is the #91 bug.
            "codec": crate::pipeline::codec::current().label(),
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
    //
    // The envelope is not decoration: `lib/webrtc.js` dispatches on `msg.type`
    // and hands `msg.data` to `StreamStats.serverSample`, and every other
    // builder here (`stream_info`, `connection`, `cursor`) ships the same
    // `{type, data}` pair. This one used to go out as the bare figure object, so
    // no branch matched and the browser dropped the whole sample: the panel's
    // client-side tiles (decode, jitter, loss, NACK, PLI) kept counting while
    // every server figure — encode / capture-to-encode / capture-to-send, the
    // video rate and its peak, host CPU and memory — drew a dash forever.
    json!({"type": "stream_stats", "data": data}).to_string()
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
/// `framerate.uncapped` advertises the panel's last slider stop: `framerate:
/// 0`, meaning "pace on the capture thread, no timer" (see
/// `pipeline::settings::FPS_UNCAPPED`). A client that does not know the flag
/// simply never sends it.
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
                // #91: the encoder, and the one place the panel could contradict
                // itself. `pipeline::encoder` builds exactly one encoder —
                // libvpx, software — so the row names which libvpx codec is
                // running. Before this key existed the panel fell back to its own
                // local default and printed "H.264" while the statistics tile
                // (which reads `stream_info`) said "libvpx · VP8": two answers to
                // one question, neither of them from here.
                //
                // #94: the row is a real choice now. Two entries, because this
                // build has two libvpx encoders; picking one records the pick and
                // restarts the session, since a negotiated codec cannot change
                // inside a live PeerConnection. H.264/AV1 stay absent on purpose:
                // this build has no encoder for them, and a menu entry that
                // cannot be encoded is a knob that only pretends to work.
                "encoder": {
                    "value": crate::pipeline::codec::current().encoder_id(),
                    "allowed": crate::pipeline::codec::VideoCodec::ALL
                        .iter()
                        .map(|c| c.encoder_id())
                        .collect::<Vec<_>>(),
                    "locked": false
                },
                // #91: the encoder has no CRF path — `set_quality` exists, but
                // the pipeline only ever drives a bitrate target — so CRF is not
                // offered. Announcing one option is how the panel learns to stop
                // drawing the quality/CRF knobs, which used to be selectable and
                // did nothing.
                //
                // #92: that is no longer true — the encoder now opens in CBR or
                // CQ (`scrap::VpxRateControl`), so both modes are real and the
                // panel's two rows (`rate_control_mode`, `video_crf`) work.
                "rate_control_mode": {
                    "value": s.rate_control().as_str(),
                    "allowed": ["cbr", "crf"],
                    "locked": false
                },
                // #92: the quantizer pinned in CRF mode. `min`/`max` are the
                // stops the panel can express (`CRF_STOPS`, 50..5) — the encoder
                // itself takes 0..=63, so every value the panel can send is
                // inside the range the server clamps to.
                "video_crf": {
                    "value": s.crf(),
                    "min": crate::pipeline::settings::CRF_MIN.max(5),
                    "max": 50,
                    "default": crate::pipeline::settings::CRF_DEFAULT,
                },
                // #56/#92: the bitrate slider. Without this key the panel's
                // disabled test (`min === max`, both undefined) greys it out —
                // the slider was dead in the UI while `vb,<kbps>` had been
                // working end to end all along (`pipeline::set_bitrate_override`).
                // The bounds are the ones the server accepts (`mod.rs` clamps to
                // 200..40_000); `default` matches the panel's own fallback so a
                // browser with nothing stored lands where it used to.
                "video_bitrate": {
                    "min": 200,
                    "max": 40_000,
                    "default": 8_000,
                },
                // #92: paint-over (incremental region redraw) is not implemented
                // anywhere in this pipeline, so its three knobs are `locked` —
                // the panel hides a locked key instead of drawing a slider that
                // cannot change anything.
                "video_paintover_crf": { "locked": true },
                "video_paintover_burst_frames": { "locked": true },
                "use_paint_over_quality": { "locked": true },
                // #91: the audio slider was disabled, not broken: the panel
                // greys a control out when `allowed` has fewer than two entries,
                // and with no `audio_bitrate` key at all that count was zero.
                // The stops are Opus targets this server will take
                // (`audio::BITRATE_STEPS`), and `value` is the live target, not
                // a remembered one.
                "audio_bitrate": {
                    "value": crate::pipeline::audio::current_bitrate_bps(),
                    "allowed": crate::pipeline::audio::BITRATE_STEPS,
                    "min": crate::pipeline::audio::BITRATE_STEPS[0],
                    "max": crate::pipeline::audio::BITRATE_STEPS[crate::pipeline::audio::BITRATE_STEPS.len() - 1],
                    "locked": false
                },
                "framerate": {
                    "value": s.fps(),
                    "min": 10,
                    // 240, not 60: the panel offers every rate up to the one a
                    // 240 Hz display runs at, and `uncapped` adds the last stop
                    // ("no limit", fps 0 — paced by the capture thread).
                    "max": crate::pipeline::settings::FPS_BUDGET,
                    "uncapped": true,
                    "locked": false
                },
                "manual_resolution": { "value": false, "locked": false },
                // Kept as a read-only *report* of the live request, not as a
                // framebuffer to adopt: the frontend shows it, and ignores it
                // while `manual_resolution` is false.
                "manual_width": { "value": s.width(), "locked": false },
                "manual_height": { "value": s.height(), "locked": false },
                "enable_resize": { "value": true, "locked": false },
                // #90: the Screen section's UI-scale dropdown. Without this key
                // the panel drew an empty (but enabled) `<select>`: its option
                // list is `serverSettings.scaling_dpi.allowed`, so an absent
                // payload meant no options at all. The values are DPI against
                // 96 — the panel prints `dpi / 96 * 100` as the percentage and
                // compares stored picks as strings — and they are exactly the
                // steps `hostui::apply_scale` can land on, so no option can
                // silently do nothing. `value` is read from the host, not
                // remembered, so a fresh browser shows the desktop it is
                // actually looking at.
                "scaling_dpi": {
                    "value": crate::dispatch::hostui::current_scale_dpi()
                        .unwrap_or(96)
                        .to_string(),
                    "allowed": crate::dispatch::hostui::SCALE_DPI
                        .map(|dpi| dpi.to_string()),
                    "overridden": false,
                    "locked": false
                },
                // #90: the pointer-DPI slider. `allowed` is every speed step
                // Windows has, as DPI against a 400 DPI baseline (step 10, the
                // OS default, is 400). A typed value snaps to one of these; the
                // host cannot express anything between two steps.
                "mouse_dpi": {
                    "value": crate::dispatch::hostui::current_mouse_dpi()
                        .unwrap_or(crate::dispatch::hostui::BASE_DPI),
                    "allowed": crate::dispatch::hostui::speed_steps_dpi(),
                    "base": crate::dispatch::hostui::BASE_DPI,
                },
                // #96: the game-mode ghost pointer. This payload is the only
                // way the page learns the deployment's fade time — the timer
                // itself runs in the browser, so `idle_secs` is a number to
                // announce, not a knob the server enforces. `value` is the
                // starting point of the panel's ladder (a stored client pick
                // still wins), and the row is not `locked`: which pointer a
                // player wants is their own choice, not the operator's.
                "ghost_cursor": {
                    "value": true,
                    "idle_secs": crate::dispatch::ghost::idle_secs(),
                    "locked": false
                },
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

    /// The sample is a message, not a bag of figures: `lib/webrtc.js` dispatches
    /// on `type` and reads `data`, so a bare object is dropped in silence and
    /// every server tile draws a dash while the client's own tiles keep counting.
    #[test]
    fn stats_arrive_as_a_stream_stats_message() {
        let line = stream_stats(&StreamStats::default().with_pipeline(&snapshot()));
        let parsed: hbb_common::serde_json::Value =
            hbb_common::serde_json::from_str(&line).expect("stats line is not JSON");
        assert_eq!(parsed["type"], json!("stream_stats"), "wrong envelope: {line}");
        assert!(parsed["data"]["encode_ms"].is_number(), "no figures under data: {line}");
        assert!(parsed["data"]["peak_mbps"].is_number(), "no figures under data: {line}");
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

    /// #91: this payload is the panel's only description of what the server can
    /// do, and every control in the sidebar is enabled or greyed by something in
    /// here. Two keys were missing and both lied in the same direction — the
    /// sidebar printed its own defaults over a server that could not honour
    /// them: the encoder row said "H.264" over a libvpx/VP8 stream, and the
    /// audio slider was disabled because `allowed.length <= 1` is exactly how
    /// "no choices" reads, so a missing list meant "one choice, take it".
    ///
    /// The assertions are about *shape*, not today's values: the encoder list
    /// holds exactly the codecs this build can encode (#94: VP8 and VP9, both
    /// libvpx), two rate-control modes (#92: CBR and the CQ path the encoder can
    /// actually open), and an audio list long enough to un-grey the slider with
    /// nothing above the Opus ceiling the encoder would clamp away.
    #[test]
    fn settings_describe_the_encoder_and_audio_the_pipeline_has() {
        let s = crate::pipeline::StreamSettings::from_config(&crate::config::Config::default());
        let line = server_settings(&s);
        let parsed: hbb_common::serde_json::Value =
            hbb_common::serde_json::from_str(&line).expect("server_settings is not JSON");
        let settings = &parsed["data"]["settings"];

        assert_eq!(
            settings["encoder"]["allowed"],
            json!(["vp8enc", "vp9enc"]),
            "the encoder list is the panel's only honest answer: {line}"
        );
        // #94: one entry would grey the row (and did, as `locked: true`); two
        // make it a choice, so the value has to name one of them.
        assert_eq!(settings["encoder"]["locked"], json!(false));
        let value = settings["encoder"]["value"].as_str().unwrap_or_default();
        assert!(
            ["vp8enc", "vp9enc"].contains(&value),
            "encoder value {value:?} is not one of the offered ids: {line}"
        );
        assert_eq!(
            settings["rate_control_mode"]["allowed"],
            json!(["cbr", "crf"]),
            "both modes must be real or the row has to say so: {line}"
        );
        assert_eq!(settings["rate_control_mode"]["locked"], json!(false));
        assert_eq!(settings["rate_control_mode"]["value"], json!("cbr"));

        // #92: a min/max pair that is equal or missing disables the panel's
        // slider (`min === max`, and `undefined === undefined` counts), which is
        // exactly how `video_bitrate` and `video_crf` used to be unreachable.
        for key in ["video_crf", "video_bitrate"] {
            let min = settings[key]["min"].as_u64();
            let max = settings[key]["max"].as_u64();
            assert!(
                matches!((min, max), (Some(a), Some(b)) if a < b),
                "{key} needs a real span or the panel greys it out: {line}"
            );
        }
        // The paint-over knobs belong to a path this server does not have; a
        // locked key is hidden rather than drawn dead.
        for key in [
            "video_paintover_crf",
            "video_paintover_burst_frames",
            "use_paint_over_quality",
        ] {
            assert_eq!(settings[key]["locked"], json!(true), "{key}: {line}");
        }

        // #96: the ghost pointer's row. The switch itself is the client's, so
        // `value` is only the ladder's starting point and must not be `locked`;
        // the fade time has to be a number the page can turn into milliseconds,
        // because a missing one leaves the ghost on screen forever.
        assert_eq!(settings["ghost_cursor"]["locked"], json!(false), "{line}");
        assert_eq!(settings["ghost_cursor"]["value"], json!(true), "{line}");
        let idle = settings["ghost_cursor"]["idle_secs"].as_u64();
        assert!(
            matches!(idle, Some(secs) if secs >= 1),
            "ghost_cursor.idle_secs must be a positive number: {line}"
        );

        let audio = settings["audio_bitrate"]["allowed"]
            .as_array()
            .unwrap_or_else(|| panic!("no audio stops: {line}"));
        assert!(audio.len() > 1, "one stop greys the slider out: {line}");
        let ceiling = crate::pipeline::audio::BITRATE_STEPS;
        assert_eq!(
            audio.last().unwrap(),
            &json!(ceiling[ceiling.len() - 1]),
            "a stop above the encoder's ceiling is one it silently clamps: {line}"
        );
    }
}
