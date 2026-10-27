//! Upbound command classification (design 5.5).
//!
//! Groups:
//! * A: stage1 must act on (start/stop video, stats gating, compression ack, ping).
//! * B: record at debug then drop; no reply, no state change.
//! * C: input commands; routed to the `InputSink` stub in stage1, mapped to enigo
//!   in stage2.
//!
//! The classifier only tags the line; applying the side effects lives in
//! `dispatch::mod` so the C-group parsing stays isolated behind the trait.

/// Dispatched action for one inbound line.
#[derive(Debug, Clone)]
pub enum Action {
    /// `START_VIDEO` — gate the pipeline on; idempotent repeats are harmless.
    StartVideo,
    /// `STOP_VIDEO` — stop writing samples; keep PC / DC and cycle state.
    StopVideo,
    /// `_stats,1` — start the 1 s stats timer (deduplicated by the caller).
    StatsOn,
    /// `_stats,0` — stop the stats timer; cleanup on close is caller-side.
    StatsOff,
    /// `_gz,1` — reply `_gz,0`; stage1 never compresses outbound frames.
    CompressedNegotiate,
    /// `ping,<ts>` — echo `pong,<ts>`.
    Pong(String),
    /// B group: debug-logged then ignored; carries the recognized prefix.
    Ignore(String),
    /// C group: handed to the input sink stub; carries the raw line.
    InputStub(String),
    /// `_arg_fps,<N>` (#50) — client frame-rate request; clamped by settings.
    SetFps(u32),
    /// `r,<W>x<H>` (#50) — client resolution request (scaler target).
    SetResolution(u32, u32),
    /// `SETTINGS,{json}` (#50) — client settings batch; JSON parsed by the
    /// session (framerate / manual_width / manual_height / ...).
    SetSettings(String),
    /// `REQUEST_CLIPBOARD` (#55) — Ctrl/Cmd+C in the browser: read the host
    /// clipboard and reply with the `clipboardContentUpdate` JSON.
    ClipboardRequest,
    /// `cw/cws/cwd/cwe` (text) and `cb/cbs/cbd/cbe` (binary) (#55) — browser
    /// Ctrl/Cmd+V uplink; the ClipboardSink reassembles text chunks and writes
    /// the host clipboard. Carries the raw line.
    ClipboardChunk(String),
    /// `vb,<N>` (#56) — client video bitrate request; 0 restores automatic.
    SetBitrate(u32),
    /// `SET_NATIVE_CURSOR_RENDERING,<0|1>` (#57) — frontend pointer-rendering
    /// handover. 1 = pointer lock, the frontend hides its native cursor and
    /// the pipeline composites the OS cursor into frames (C1); 0 = the
    /// frontend draws its own pointer, no server compositing.
    SetNativeCursorRendering(bool),
    /// Unrecognized line; debug-logged, never crashes the session.
    Unknown(String),
}

/// Split a line into leading command token and remainder. Commands use a comma
/// separator; bare commands carry an empty remainder.
fn split_command(line: &str) -> (&str, &str) {
    match line.split_once(',') {
        Some((head, rest)) => (head, rest),
        None => (line, ""),
    }
}

/// Classify one inbound control-channel line. The match order mirrors the
/// frontend emission points in design 5.5 so ambiguous prefixes (e.g. `kr`
/// versus `kh,`) resolve correctly: bare tokens before comma-prefixes.
pub fn classify(line: &str) -> Action {
    let line = line.trim();
    if line.is_empty() {
        return Action::Unknown(String::from("<empty>"));
    }

    // ---- A group: exact bare tokens first. ----
    match line {
        "START_VIDEO" => return Action::StartVideo,
        "STOP_VIDEO" => return Action::StopVideo,
        "START_AUDIO" | "STOP_AUDIO" => return Action::Ignore(String::from(line)),
        "REQUEST_CLIPBOARD" => return Action::ClipboardRequest,
        "cr" | "pong" => return Action::Ignore(String::from(line)),
        "kr" => return Action::InputStub(String::from(line)),
        "sfe" => return Action::InputStub(String::from(line)),
        "pointer,none" => return Action::InputStub(String::from(line)),
        _ => {}
    }

    let (head, rest) = split_command(line);

    match head {
        "_stats" if rest == "1" => Action::StatsOn,
        "_stats" if rest == "0" => Action::StatsOff,
        "_gz" if rest == "1" => Action::CompressedNegotiate,
        "ping" => Action::Pong(rest.to_owned()),

        // ---- Setting requests (#50): the frontend sends these from the
        // settings panel and on connect (selkies-wr-core.js:1364 / :1667 /
        // :1451). `_arg_fps,N` and `r,WxH` are separated here; the `SETTINGS`
        // JSON payload is parsed by the session because several keys share one
        // line. ----
        "_arg_fps" => {
            match rest.parse::<u32>() {
                Ok(fps) => Action::SetFps(fps),
                Err(_) => Action::Unknown(line.to_owned()),
            }
        }
        "r" => {
            match rest.split_once('x') {
                Some((w, h)) => match (w.trim().parse::<u32>(), h.trim().parse::<u32>()) {
                    (Ok(w), Ok(h)) if w > 0 && h > 0 => Action::SetResolution(w, h),
                    _ => Action::Unknown(line.to_owned()),
                },
                None => Action::Unknown(line.to_owned()),
            }
        }
        "SETTINGS" => Action::SetSettings(rest.to_owned()),

        // ---- B group: ignored after debug log. ----
        "s" | "ab" | "_crf" | "_rc" | "_ebc"
        | "cmd" | "_stats_video" | "_f" | "_l" => {
            Action::Ignore(head.to_owned())
        }

        // ---- #56: client bitrate request `vb,<N>` (settings panel / RELP). ----
        "vb" => {
            match rest.parse::<u32>() {
                Ok(kbps) => Action::SetBitrate(kbps),
                Err(_) => Action::Unknown(line.to_owned()),
            }
        }

        // ---- #57: pointer-rendering handover `SET_NATIVE_CURSOR_RENDERING,<0|1>` ----
        "SET_NATIVE_CURSOR_RENDERING" => match rest.trim().parse::<u8>() {
            Ok(0) => Action::SetNativeCursorRendering(false),
            Ok(1) => Action::SetNativeCursorRendering(true),
            _ => Action::Unknown(line.to_owned()),
        },

        // ---- Clipboard group (#55): text uplink reassembled by the sink. ----
        "cw" | "cws" | "cwd" | "cwe" | "cb" | "cbs" | "cbd" | "cbe" => {
            Action::ClipboardChunk(line.to_owned())
        }

        // ---- C group: input stub. ----
        "kd" | "ku" | "kh" | "m" | "m2" | "p" | "js"
        | "_pointer_echo" | "pointer" | "sf" | "vp" => Action::InputStub(line.to_owned()),

        _ => Action::Unknown(line.to_owned()),
    }
}
