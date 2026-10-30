//! Stage1 server configuration.
//!
//! CLI surface mirrors the retired Python service so the deploy wrapper and
//! the dist probe line up; defaults follow design 4.4. Parsing is hand-rolled
//! to avoid a second clap copy beside hbb_common's dev-only clap.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// Listen interface.
    pub host: String,
    /// Listen TCP port (HTTPS when a certificate chain is supplied).
    pub port: u16,
    /// Frontend dist root served on non-API GET paths.
    pub dist: PathBuf,
    /// WebRTC app segment echoed into the WS path; the client hardcodes
    /// `webrtc`, stage1 accepts the segment without validation.
    pub app_name: String,
    /// Output capture width requested from the capturer/encoder.
    pub width: u32,
    /// Output capture height requested from the capturer/encoder.
    pub height: u32,
    /// Target frames per second; `spf = 1.0 / fps` drives the pipeline beat.
    pub fps: u32,
    /// Encoder target bitrate in kbps (VP8 base-bitrate preset ratio applies
    /// inside scrap; this value selects the quality ratio later, design 4.4).
    pub bitrate_kbps: u32,
    /// TLS PEM certificate; empty means plain HTTP.
    pub tls_cert: Option<PathBuf>,
    /// TLS PEM private key; empty means plain HTTP.
    pub tls_key: Option<PathBuf>,
    /// File log path; empty writes to stdout only.
    pub log_file: Option<PathBuf>,
    /// #60: skip the HDR (R16G16B16A16_FLOAT) DuplicateOutput1 attempt. VMware
    /// SVGA 3D has no HDR desktop, so every process start pays one guaranteed
    /// `DuplicateOutput1` failure before falling back to the legacy call; the
    /// flag silences that warning and opens the duplication path directly.
    pub no_hdr: bool,
    /// #60: diagnostic frame dump directory. When set, the pipeline writes the
    /// pre-scale BGRA frame and the post-scale BGRA frame every N-th beat to
    /// this directory as BMP files, so a smear/glitch can be attributed to the
    /// capturer, the scaler or the encoder instead of guessing.
    pub dump_frame: Option<PathBuf>,
    /// #64/#66: emit one static-screen repeat frame every N WouldBlock beats.
    /// 1 = hold the full target cadence on an idle desktop; 2 halves the repeat
    /// rate for a browser whose software VP8 decoder cannot keep up (measured
    /// 3.6 % dropped frames at ~54 repeats/s on this host).
    pub repeat_every: u32,
    /// #86: wall-clock period of the periodic keyframe, in seconds (default
    /// 10). This replaces `kf_max_dist = 60` as the authority: libvpx counts
    /// that budget in **encoded frames**, and this pipeline only encodes when
    /// the desktop changes or a repeat is due, so "60 frames" is ~1 s at 60 fps
    /// on a moving picture and minutes on a static one. A keyframe is 100-200 KB
    /// at 1080p, so one per second spends the entire bitrate budget and pins the
    /// quantizer at `q_max` (the #61 experiment D finding); 10 s keeps the
    /// reference chain anchored without paying for it, and the PLI path still
    /// re-anchors on demand.
    pub keyframe_secs: u32,
    /// #67: file-transfer root. When set, `/api/files/{rel}` lists/downloads
    /// files under this directory and `/api/upload` writes into it; `None`
    /// disables both endpoints (403), matching the retired Python service's
    /// `file_manager_path` behavior. Every path component is validated against
    /// traversal, drive letters and reserved characters before touching the
    /// filesystem (RustDesk `fs::join_validated_path` style).
    pub upload_dir: Option<PathBuf>,
    /// #94: the video codec sessions negotiate with, `vp8` (default) or `vp9`.
    ///
    /// Only the starting value: the panel can change it at runtime, and the
    /// choice lives in `pipeline::codec` (a pick has to outlive the session that
    /// carried it, or the reload it triggers would revert it). This flag exists
    /// so a fresh process can start on VP9 without anyone touching the UI.
    pub video_codec: String,
    /// #96: how long the client's game-mode ghost pointer stays on screen
    /// without an effective move, in seconds (default 5).
    ///
    /// The pointer itself is drawn by the browser, so this is a *number to
    /// announce*, not a timer the server runs: it rides `server_settings` as
    /// `ghost_cursor.idle_secs` and the page fades its ghost after that. A
    /// game draws its own crosshair, so a ghost that never disappears sits on
    /// top of the action and lies about where the pointer is; one that
    /// disappears too early is gone before the hand starts moving again.
    pub ghost_cursor_idle_secs: u64,
    /// #88: 监听面鉴权策略名（`allow-all` / `deny-all`）。
    ///
    /// 默认 `allow-all`，与插桩前行为完全一致 —— 骨架本身不改变任何访问
    /// 控制。`deny-all` 不是安全模式，是**覆盖率探针**：装上它跑一遍，凡是
    /// 还能用的入口就是漏插桩的地方。真正的策略由上层项目实现 `Policy`
    /// 后注入。
    pub policy: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_owned(),
            port: 8080,
            dist: PathBuf::from(r"C:\dev\selkies-web"),
            app_name: "webrtc".to_owned(),
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate_kbps: 2500,
            tls_cert: None,
            tls_key: None,
            // Stage1 decision (design 9.2 #5): file log lands under logs\ relative
            // to the working directory; rotation is deferred to stage4 serviceization.
            log_file: Some(PathBuf::from(r"logs\stream-server.log")),
            no_hdr: false,
            dump_frame: None,
            repeat_every: 2,
            keyframe_secs: 10,
            upload_dir: None,
            video_codec: "vp8".to_owned(),
            ghost_cursor_idle_secs: crate::dispatch::ghost::DEFAULT_IDLE_SECS,
            policy: "allow-all".to_owned(),
        }
    }
}

impl Config {
    /// Parse `argv` after the program name. Unknown flags keep the default and
    /// are logged once; a non-numeric value for a numeric flag keeps the
    /// default rather than aborting startup.
    pub fn parse(argv: Vec<String>) -> Self {
        let mut cfg = Self::default();
        let mut iter = argv.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--host" => {
                    if let Some(v) = iter.next() {
                        cfg.host = v;
                    }
                }
                "--port" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u16>() {
                            cfg.port = n;
                        }
                    }
                }
                "--dist" => {
                    if let Some(v) = iter.next() {
                        cfg.dist = PathBuf::from(v);
                    }
                }
                "--app" => {
                    if let Some(v) = iter.next() {
                        cfg.app_name = v;
                    }
                }
                "--width" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u32>() {
                            cfg.width = n;
                        }
                    }
                }
                "--height" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u32>() {
                            cfg.height = n;
                        }
                    }
                }
                "--fps" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u32>() {
                            cfg.fps = n;
                        }
                    }
                }
                "--bitrate" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u32>() {
                            cfg.bitrate_kbps = n;
                        }
                    }
                }
                "--tls-cert" => {
                    if let Some(v) = iter.next() {
                        cfg.tls_cert = Some(PathBuf::from(v));
                    }
                }
                "--tls-key" => {
                    if let Some(v) = iter.next() {
                        cfg.tls_key = Some(PathBuf::from(v));
                    }
                }
                "--log" => {
                    if let Some(v) = iter.next() {
                        if v.is_empty() {
                            cfg.log_file = None;
                        } else {
                            cfg.log_file = Some(PathBuf::from(v));
                        }
                    }
                }
                "--repeat-every" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u32>() {
                            cfg.repeat_every = n.clamp(1, 10);
                        }
                    }
                }
                "--keyframe-secs" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u32>() {
                            cfg.keyframe_secs = n.clamp(1, 120);
                        }
                    }
                }
                "--no-hdr" => cfg.no_hdr = true,
                "--video-codec" => {
                    if let Some(v) = iter.next() {
                        // 未知值保留默认：本项目的默认是 VP8，拼错不该变成
                        // "启动失败"，但也别静默接受 —— main 里会记一条 warn。
                        cfg.video_codec = v;
                    }
                }
                // #96: optional, like the two above — the default (5 s) is what
                // almost every host wants, so a launch script need not carry it.
                "--ghost-cursor-idle" => {
                    if let Some(v) = iter.next() {
                        if let Ok(n) = v.parse::<u64>() {
                            cfg.ghost_cursor_idle_secs = crate::dispatch::ghost::clamped(n);
                        }
                    }
                }
                "--policy" => {
                    if let Some(v) = iter.next() {
                        // 未知名保留默认并在 main 里告警，不在这里吞掉 ——
                        // "策略名拼错了" 必须看得见，否则会以为鉴权已经生效。
                        cfg.policy = v;
                    }
                }
                "--upload-dir" => {
                    if let Some(v) = iter.next() {
                        if v.is_empty() {
                            cfg.upload_dir = None;
                        } else {
                            cfg.upload_dir = Some(PathBuf::from(v));
                        }
                    }
                }
                "--dump-frame" => {
                    if let Some(v) = iter.next() {
                        if !v.is_empty() {
                            cfg.dump_frame = Some(PathBuf::from(v));
                        }
                    }
                }
                other => {
                    log::warn!("unknown CLI argument ignored: {other}");
                }
            }
        }
        cfg
    }

    /// Spacing between pipeline beats, per the stage1 decision: duration is
    /// always exactly spf; repeat frames re-use that interval instead of a
    /// shorter or longer one (design 9.2 #4).
    pub fn spf(&self) -> std::time::Duration {
        let fps = self.fps.max(1) as f64;
        std::time::Duration::from_secs_f64(1.0 / fps)
    }

    /// True when both certificate and key paths are present.
    pub fn tls_enabled(&self) -> bool {
        self.tls_cert.is_some() && self.tls_key.is_some()
    }

    /// #86: wall-clock spacing between periodic keyframes. Clamped again here
    /// because `Config` can also be built as a struct literal (tests, benches),
    /// where `parse` never ran the clamp.
    pub fn keyframe_period(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.keyframe_secs.clamp(1, 120) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Config {
        Config::parse(args.iter().map(|s| (*s).to_owned()).collect())
    }

    #[test]
    fn keyframe_secs_defaults_to_ten_seconds() {
        let cfg = parse(&[]);
        assert_eq!(cfg.keyframe_secs, 10);
        assert_eq!(cfg.keyframe_period(), std::time::Duration::from_secs(10));
    }

    #[test]
    fn keyframe_secs_reads_the_flag() {
        assert_eq!(parse(&["--keyframe-secs", "4"]).keyframe_period().as_secs(), 4);
    }

    #[test]
    fn keyframe_secs_is_clamped_to_one_second_upwards() {
        // 0 would mean "force a keyframe on every beat" — one IDR per frame eats
        // the whole budget on a moving picture, so clamp instead of trusting it.
        assert_eq!(parse(&["--keyframe-secs", "0"]).keyframe_period().as_secs(), 1);
    }

    #[test]
    fn keyframe_secs_is_clamped_to_two_minutes_downwards() {
        // The other end matters too: past ~2 min the reference chain drifts
        // (the #53 quantizer-drift finding) and a late joiner waits for a PLI.
        assert_eq!(
            parse(&["--keyframe-secs", "999"]).keyframe_period().as_secs(),
            120
        );
    }

    #[test]
    fn a_bad_keyframe_value_keeps_the_default() {
        assert_eq!(parse(&["--keyframe-secs", "soon"]).keyframe_secs, 10);
    }

    /// #96: the ghost pointer's fade time is optional on purpose — the default
    /// is what almost every host wants, so a launch script need not carry it.
    #[test]
    fn ghost_cursor_idle_defaults_to_five_seconds() {
        assert_eq!(parse(&[]).ghost_cursor_idle_secs, 5);
    }

    /// A value outside the range is clamped rather than announced: 0 would read
    /// as "never fade" on the client, which is the behavior the knob exists to
    /// remove.
    #[test]
    fn ghost_cursor_idle_reads_the_flag_and_clamps_it() {
        assert_eq!(parse(&["--ghost-cursor-idle", "8"]).ghost_cursor_idle_secs, 8);
        assert_eq!(parse(&["--ghost-cursor-idle", "0"]).ghost_cursor_idle_secs, 1);
        assert_eq!(
            parse(&["--ghost-cursor-idle", "99999"]).ghost_cursor_idle_secs,
            600
        );
        assert_eq!(
            parse(&["--ghost-cursor-idle", "soon"]).ghost_cursor_idle_secs,
            5,
            "a value that is not a number keeps the default"
        );
    }
}
