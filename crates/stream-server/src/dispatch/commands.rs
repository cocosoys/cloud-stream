//! Upbound command classification (design 5.5).
//!
//! Groups:
//! * A: stage1 must act on (start/stop video, stats gating, compression ack,
//!   the browser's reply to the server's RTT ping).
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
    /// `pong,<epoch_s>` — the browser's reply to the server's `{"type":"ping"}`
    /// (#78). The payload is the browser clock and is ignored by the
    /// measurement; the session only needs to know that a reply arrived.
    PongReply(String),
    /// B group: debug-logged then ignored; carries the recognized prefix.
    Ignore(String),
    /// C group: handed to the input sink stub; carries the raw line.
    InputStub(String),
    /// `_arg_fps,<N>` (#50) — client frame-rate request; clamped by settings.
    SetFps(u32),
    /// `r,<W>x<H>` (#50) — client resolution request (scaler target).
    SetResolution(u32, u32),
    /// `dr,<W>x<H>` (#89) — change the *host display mode* itself so the
    /// capture is 1:1 with the client viewport. Unlike `r,`, this reflows the
    /// physical desktop; the browser asks for it explicitly from the toolbar.
    SetDisplayResolution(u32, u32),
    /// `dr,reset` (#89) — put the host back to the mode it had before the first
    /// `dr,` of this process.
    ResetDisplayResolution,
    /// `md,<dpi>` (#90) — set the host's *pointer speed* so its pointer DPI is
    /// that number (400 = the OS default). Unlike a client-side gain, this rules
    /// the host's real pointer, so the desktop and every game agree.
    SetMouseDpi(u32),
    /// `dd,<dpi>` (#90) — set the host's *UI scaling* (96 = 100 %), the
    /// Settings > Display > Scale dropdown, in DPI against 96.
    SetUiScale(u32),
    /// `ma,<0|1>` (#96) — 指针直通（游戏模式）。`1` 把主机指针钉成 1:1
    /// （关闭「提高指针精确度」+ 速度档归 10，一个计数一个像素），`0` 把进入
    /// 游戏模式前的档位与加速曲线原样放回去。客户端画本地幽灵指针时，只有
    /// 主机的映射真是 1:1，幽灵才追得上真实指针。
    SetPointerDirect(bool),
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
    /// `_rc,<cbr|crf>` (#92) — the panel's rate-control pick. libvpx fixes
    /// `rc_end_usage` at encoder init, so this makes the loop rebuild the
    /// encoder rather than reconfigure it in place.
    SetRateControl(String),
    /// `_crf,<N>` (#92) — the quantizer pinned under constant quality.
    SetCrf(u32),
    /// `START_AUDIO` / `STOP_AUDIO` (#80) — the frontend's sound toggle. The
    /// browser mutes its own `<video>` either way; this is what stops the server
    /// from spending 64 kbps on audio nobody hears.
    SetAudioOn(bool),
    /// `ab,<N>` (#80) — client audio bitrate request, bits per second
    /// (`selkies-wr-core.js:2346` sends 128000 by default). 0 restores it.
    SetAudioBitrate(u32),
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
        "START_AUDIO" => return Action::SetAudioOn(true),
        "STOP_AUDIO" => return Action::SetAudioOn(false),
        "REQUEST_CLIPBOARD" => return Action::ClipboardRequest,
        "cr" => return Action::Ignore(String::from(line)),
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
        // #78: the browser answers a server ping with this line
        // (`lib/webrtc.js:777`). It is not a request and must never be echoed.
        "pong" => Action::PongReply(rest.to_owned()),

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
        // #89: `dr,<W>x<H>` switches the host *display mode*; `dr,reset` puts
        // it back. Parsed next to `r` because the payload shape is identical —
        // keep them apart: `r` is a scaler target, `dr` moves the real desktop.
        "dr" if rest.trim() == "reset" => Action::ResetDisplayResolution,
        "dr" => {
            match rest.split_once('x') {
                Some((w, h)) => match (w.trim().parse::<u32>(), h.trim().parse::<u32>()) {
                    (Ok(w), Ok(h)) if w > 0 && h > 0 => Action::SetDisplayResolution(w, h),
                    _ => Action::Unknown(line.to_owned()),
                },
                None => Action::Unknown(line.to_owned()),
            }
        }
        "SETTINGS" => Action::SetSettings(rest.to_owned()),
        // #90: `md,<dpi>` — the host's pointer speed expressed as DPI (400 =
        // the OS default step 10). Base-10 digits only: a stray suffix must be
        // a rejection, not a silently different number.
        "md" => match rest.trim().parse::<u32>() {
            Ok(dpi) => Action::SetMouseDpi(dpi),
            Err(_) => Action::Unknown(line.to_owned()),
        },
        // #90: `dd,<dpi>` — the host's UI scaling, in DPI against 96 (96 = 100 %).
        "dd" => match rest.trim().parse::<u32>() {
            Ok(dpi) => Action::SetUiScale(dpi),
            Err(_) => Action::Unknown(line.to_owned()),
        },
        // #96: `ma,<0|1>` — pin the host pointer to 1:1 for game mode, or put
        // back what it was. Only the two literals are accepted: a third value
        // would have to guess whether it means "pin" or "release".
        "ma" => match rest.trim() {
            "1" => Action::SetPointerDirect(true),
            "0" => Action::SetPointerDirect(false),
            _ => Action::Unknown(line.to_owned()),
        },

        // ---- #80: client audio bitrate request `ab,<N>` (bits per second). ----
        "ab" => {
            match rest.trim().parse::<u32>() {
                Ok(bps) => Action::SetAudioBitrate(bps),
                Err(_) => Action::Unknown(line.to_owned()),
            }
        }

        // ---- B group: ignored after debug log. ----
        "s" | "_ebc"
        | "cmd" | "_stats_video" | "_f" | "_l" => {
            Action::Ignore(head.to_owned())
        }

        // ---- #92: the panel's rate-control pair. ----
        "_rc" => Action::SetRateControl(rest.trim().to_owned()),
        "_crf" => match rest.trim().parse::<u32>() {
            Ok(v) => Action::SetCrf(v),
            Err(_) => Action::Unknown(line.to_owned()),
        },

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

#[cfg(test)]
mod tests {
    use super::*;

    /// #92: `_rc,` / `_crf,` were in the ignored B group, which is why the
    /// panel's rate-control row and CRF slider could be dragged and changed
    /// nothing — the frontend has sent both since the WR core was written, and
    /// only the server half was missing.
    #[test]
    fn rate_control_pair_reaches_the_settings() {
        assert!(matches!(
            classify("_rc,crf"),
            Action::SetRateControl(ref m) if m == "crf"
        ));
        assert!(matches!(
            classify("_rc,cbr"),
            Action::SetRateControl(ref m) if m == "cbr"
        ));
        assert!(matches!(classify("_crf,25"), Action::SetCrf(25)));
        // Not a number: refuse rather than silently applying 0 (a near-lossless
        // quantizer nobody asked for).
        assert!(matches!(classify("_crf,"), Action::Unknown(_)));
        assert!(matches!(classify("_crf,high"), Action::Unknown(_)));
    }

    /// #80: both audio switches used to land in `Action::Ignore`, which is why
    /// the panel's mute button only muted the browser's own `<video>` while the
    /// server kept encoding 64 kbps nobody could hear.
    #[test]
    fn audio_switches_reach_the_encoder() {
        assert!(matches!(classify("START_AUDIO"), Action::SetAudioOn(true)));
        assert!(matches!(classify("STOP_AUDIO"), Action::SetAudioOn(false)));
        assert!(matches!(
            classify("ab,128000"),
            Action::SetAudioBitrate(128_000)
        ));
    }

    /// A garbled payload must stay `Unknown` rather than being read as "mute",
    /// which is the failure mode a bare `matches!` on the head would produce.
    #[test]
    fn malformed_audio_bitrate_is_not_a_mute() {
        assert!(matches!(classify("ab,"), Action::Unknown(_)));
        assert!(matches!(classify("ab,fast"), Action::Unknown(_)));
    }

    /// #96: only the two literals pin and release. `ma,2` and `ma,` must not be
    /// read as "release" — releasing is what puts the user's acceleration curve
    /// back, and doing it on a typo would move their pointer mid-session.
    #[test]
    fn the_pointer_direct_pair_is_two_literals() {
        assert!(matches!(classify("ma,1"), Action::SetPointerDirect(true)));
        assert!(matches!(classify("ma,0"), Action::SetPointerDirect(false)));
        assert!(matches!(classify("ma,2"), Action::Unknown(_)));
        assert!(matches!(classify("ma,"), Action::Unknown(_)));
        assert!(matches!(classify("ma"), Action::Unknown(_)));
    }
}
