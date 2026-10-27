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
                "--no-hdr" => cfg.no_hdr = true,
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
}
