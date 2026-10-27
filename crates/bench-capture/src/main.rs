// 0e-3/0e-4: 本机/被控采集 + VP8 软编码基线采样。
// 对齐主仓 video_service.rs 933-955 范式: WouldBlock 时在 repeat_encode_max 内
// 复用缓冲里的上一帧 YUV 重复编码, 以维持目标帧率; 拿到新帧后 counter 清零。
// 不复制静态刷新、DXGI 恢复、QoS、发送与会话逻辑。
//
// 用法: bench-capture.exe [seconds] [target_fps]
//   seconds    采样总时长, 默认 20
//   target_fps 目标帧率(用于 spf 超时与补偿 sleep), 默认 30

use scrap::codec::{Encoder, EncoderCfg};
use scrap::{
    Capturer, Display, EncodeInput, EncodeYuvFormat, TraitCapturer, VpxEncoderConfig,
    VpxNoOutput, VpxVideoCodecId,
};
use std::error::Error;
use std::ffi::c_uint;
use std::io::ErrorKind;
use std::time::{Duration, Instant};

const REPEAT_ENCODE_MAX: u32 = 10;

struct Timing {
    sum: f64,
    min: f64,
    max: f64,
}

impl Timing {
    fn new() -> Self {
        Self { sum: 0.0, min: f64::INFINITY, max: 0.0 }
    }
    fn add(&mut self, ms: f64) {
        self.sum += ms;
        self.min = self.min.min(ms);
        self.max = self.max.max(ms);
    }
    fn row(label: &str, n: usize, t: &Timing) {
        let avg = if n > 0 { t.sum / n as f64 } else { 0.0 };
        let min = if n > 0 { t.min } else { 0.0 };
        let max = if n > 0 { t.max } else { 0.0 };
        println!(
            "  {:<18} avg={:7.3} ms  min={:7.3} ms  max={:7.3} ms  (n={})",
            label, avg, min, max, n
        );
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    let target_fps: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let spf = Duration::from_secs_f64(1.0 / target_fps as f64);

    // ---- 枚举显示器, 取第一台(0e 单屏) ----
    let displays = Display::all()?;
    if displays.is_empty() {
        eprintln!("[bench] no display found");
        std::process::exit(2);
    }
    let display = displays.into_iter().next().unwrap();
    let (ox, oy) = display.origin();
    let name = display.name();
    let online = display.is_online();
    let primary = display.is_primary();
    let width = display.width();
    let height = display.height();
    println!(
        "[bench] display: {}x{} origin=({},{}) online={} primary={} name={}",
        width, height, ox, oy, online, primary, name
    );

    let mut capturer = Capturer::new(display)?;
    let gdi = capturer.is_gdi();
    println!("[bench] capturer created, gdi={}", gdi);

    let cfg = EncoderCfg::VPX(VpxEncoderConfig {
        width: width as c_uint,
        height: height as c_uint,
        quality: 1.0,
        codec: VpxVideoCodecId::VP8,
        keyframe_interval: None,
    });
    let mut encoder = Encoder::new(cfg, false)?;
    println!("[bench] encoder: VP8  target_bitrate={} kbps", encoder.bitrate());
    let yuvfmt: EncodeYuvFormat = encoder.yuvfmt();

    let mut yuv: Vec<u8> = Vec::new();
    let mut mid_data: Vec<u8> = Vec::new();

    let mut cap = Timing::new();
    let mut convert = Timing::new();
    let mut new_encode = Timing::new();
    let mut repeat_encode = Timing::new();

    let mut frames_ok: u64 = 0;
    let mut frames_would_block: u64 = 0;
    let mut frames_invalid: u64 = 0;
    let mut capture_err: u64 = 0;
    let mut encoded_ok: u64 = 0;
    let mut encode_no_output: u64 = 0;
    let mut encode_err: u64 = 0;
    let mut repeat_ok: u64 = 0;
    let mut repeat_no_output: u64 = 0;
    let mut repeat_err: u64 = 0;
    let mut repeat_skipped: u64 = 0;

    let mut yuv_valid = false;
    let mut repeat_counter: u32 = 0;

    let start = Instant::now();
    let mut last_err: Option<String> = None;

    println!("[bench] running {}s @ {} fps ...", seconds, target_fps);

    while start.elapsed().as_secs() < seconds {
        let now = Instant::now();

        let frame = match capturer.frame(spf) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                frames_would_block += 1;
                // ---- 对齐主仓 933: 复用上一帧 YUV 重复编码, 上限 repeat_encode_max ----
                if yuv_valid && repeat_counter < REPEAT_ENCODE_MAX {
                    repeat_counter += 1;
                    let t_enc = Instant::now();
                    let input = EncodeInput::YUV(yuv.as_slice());
                    let ms = start.elapsed().as_millis() as i64;
                    match encoder.encode_to_message(input, ms) {
                        Ok(_) => {
                            repeat_ok += 1;
                            repeat_encode.add(t_enc.elapsed().as_secs_f64() * 1000.0);
                        }
                        Err(e) if e.is::<VpxNoOutput>() => {
                            repeat_no_output += 1;
                            repeat_encode.add(t_enc.elapsed().as_secs_f64() * 1000.0);
                        }
                        Err(e) => {
                            repeat_err += 1;
                            last_err = Some(e.to_string());
                            repeat_encode.add(t_enc.elapsed().as_secs_f64() * 1000.0);
                        }
                    }
                } else {
                    repeat_skipped += 1;
                }
                if now.elapsed() < spf {
                    std::thread::sleep(spf - now.elapsed());
                }
                continue;
            }
            Err(e) => {
                capture_err += 1;
                last_err = Some(e.to_string());
                if capture_err >= 3 {
                    eprintln!("[bench] capture failed repeatedly: {e}");
                    break;
                }
                continue;
            }
        };

        let capture_ms = now.elapsed().as_secs_f64() * 1000.0;
        cap.add(capture_ms);

        if !frame.valid() {
            frames_invalid += 1;
            if now.elapsed() < spf {
                std::thread::sleep(spf - now.elapsed());
            }
            continue;
        }
        frames_ok += 1;
        repeat_counter = 0;

        let t_conv = Instant::now();
        let input: EncodeInput = match frame.to(yuvfmt.clone(), &mut yuv, &mut mid_data) {
            Ok(i) => i,
            Err(e) => {
                encode_err += 1;
                last_err = Some(e.to_string());
                continue;
            }
        };
        let convert_ms = t_conv.elapsed().as_secs_f64() * 1000.0;
        convert.add(convert_ms);
        yuv_valid = true;

        let t_enc = Instant::now();
        let ms = start.elapsed().as_millis() as i64;
        match encoder.encode_to_message(input, ms) {
            Ok(_) => {
                encoded_ok += 1;
                new_encode.add(t_enc.elapsed().as_secs_f64() * 1000.0);
            }
            Err(e) if e.is::<VpxNoOutput>() => {
                encode_no_output += 1;
                new_encode.add(t_enc.elapsed().as_secs_f64() * 1000.0);
            }
            Err(e) => {
                encode_err += 1;
                last_err = Some(e.to_string());
                new_encode.add(t_enc.elapsed().as_secs_f64() * 1000.0);
            }
        }

        if now.elapsed() < spf {
            std::thread::sleep(spf - now.elapsed());
        }
    }

    let wall = start.elapsed().as_secs_f64();
    let fps_new = if wall > 0.0 { frames_ok as f64 / wall } else { 0.0 };
    let fps_total = if wall > 0.0 { (frames_ok + repeat_ok) as f64 / wall } else { 0.0 };

    println!("=== bench-capture summary ===");
    println!("  wall_s              : {:.3}", wall);
    println!("  display             : {}x{}  gdi={}", width, height, gdi);
    println!("  target_fps          : {}", target_fps);
    println!("  frames_ok(new)      : {}", frames_ok);
    println!("  frames_would_block  : {}", frames_would_block);
    println!("  frames_invalid       : {}", frames_invalid);
    println!("  capture_err         : {}", capture_err);
    println!("  encoded_ok(new)     : {}", encoded_ok);
    println!("  encode_no_output    : {}", encode_no_output);
    println!("  encode_err          : {}", encode_err);
    println!("  repeat_ok           : {}", repeat_ok);
    println!("  repeat_no_output    : {}", repeat_no_output);
    println!("  repeat_err          : {}", repeat_err);
    println!("  repeat_skipped     : {}", repeat_skipped);
    println!("  measured_fps(new)   : {:.2}", fps_new);
    println!("  measured_fps(total) : {:.2}", fps_total);
    Timing::row("capture(new)", frames_ok as usize, &cap);
    Timing::row("convert(new)", frames_ok as usize, &convert);
    Timing::row("encode(new)", encoded_ok as usize, &new_encode);
    Timing::row("encode(repeat)", repeat_ok as usize, &repeat_encode);
    if let Some(err) = last_err {
        println!("  last_error          : {}", err);
    }

    Ok(())
}
