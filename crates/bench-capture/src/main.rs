// 0e-3/0e-4: 本机/被控采集 + VP8 软编码基线采样。
// 对齐主仓 video_service.rs 933-955 范式: WouldBlock 时在 repeat_encode_max 内
// 复用缓冲里的上一帧 YUV 重复编码, 以维持目标帧率; 拿到新帧后 counter 清零。
// 不复制静态刷新、DXGI 恢复、QoS、发送与会话逻辑。
//
// 用法: bench-capture.exe [seconds] [target_fps] [--codec vp8|vp9] [--tune LIST]
//   seconds    采样总时长, 默认 20
//   target_fps 目标帧率(用于 spf 超时与补偿 sleep), 默认 30
//   --codec    vp8 (默认) 或 vp9
//   --tune     不传 = #95 调优组; legacy/none = #95 之前的编码参数;
//              逗号分隔的子集 buf,er,screen,peak;  all = 全开
//
// #95: 存在这个开关是为了同一份二进制内做 A/B —— 换构建再测会把
// 「参数变了」和「机器状态变了」混在一起, 而这两件事必须分开。
use scrap::codec::{Encoder, EncoderCfg};
use scrap::{
    Capturer, Display, EncodeInput, EncodeYuvFormat, TraitCapturer, VpxEncoderConfig, VpxNoOutput,
    VpxTuning, VpxVideoCodecId,
};
use std::error::Error;
use std::ffi::c_uint;
use std::io::ErrorKind;
use std::time::{Duration, Instant};

const REPEAT_ENCODE_MAX: u32 = 10;

/// `--tune` 的解析: 空 = 默认调优组; `legacy`/`none` = 旧参数;
/// 其余按逗号分组的名字从旧参数上逐项打开, 便于单变量推进。
fn parse_tuning(spec: &str) -> VpxTuning {
    let spec = spec.trim().to_ascii_lowercase();
    if spec.is_empty() || spec == "all" || spec == "tuned" {
        return VpxTuning::tuned();
    }
    if spec == "legacy" || spec == "none" {
        return VpxTuning::legacy();
    }
    let mut t = VpxTuning::legacy();
    for part in spec.split(',') {
        match part.trim() {
            "" => {}
            // 码率控制缓冲窗: 6s -> 1s
            "buf" => {
                t.buf_sz_ms = 1000;
                t.buf_initial_ms = 600;
                t.buf_optimal_ms = 700;
            }
            // 关掉 error-resilient, 恢复 libvpx 默认
            "er" => t.error_resilient = false,
            // VP9 屏幕内容工具
            "screen" => t.screen_content = true,
            // 单帧(关键帧)码率削峰
            "peak" => t.max_intra_pct = 300,
            other => {
                eprintln!("[bench] unknown --tune group '{other}', ignored");
            }
        }
    }
    t
}

/// #95: bytes carried by one encoded `VideoFrame`, plus whether it is a
/// keyframe and how big that keyframe is. `Encoder::encode_to_message` returns
/// the protobuf wrapper, so the payload has to be summed out of the union.
fn frame_stats(vf: &base::message_proto::VideoFrame) -> (usize, bool) {
    use base::message_proto::video_frame::Union;
    let frames = match &vf.union {
        Some(Union::Vp8s(f)) | Some(Union::Vp9s(f)) => &f.frames,
        _ => return (0, false),
    };
    let bytes = frames.iter().map(|f| f.data.len()).sum();
    (bytes, frames.iter().any(|f| f.key))
}

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
    let mut positional: Vec<String> = Vec::new();
    let mut codec = VpxVideoCodecId::VP8;
    let mut tune_spec = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--codec" => {
                let v = args.next().unwrap_or_default();
                codec = match v.to_ascii_lowercase().as_str() {
                    "vp9" => VpxVideoCodecId::VP9,
                    "vp8" => VpxVideoCodecId::VP8,
                    other => {
                        eprintln!("[bench] unknown --codec '{other}'; using vp8");
                        VpxVideoCodecId::VP8
                    }
                };
            }
            "--tune" => tune_spec = args.next().unwrap_or_default(),
            other => positional.push(other.to_owned()),
        }
    }
    let mut positional = positional.into_iter();
    let seconds: u64 = positional.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    let target_fps: u32 = positional.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let spf = Duration::from_secs_f64(1.0 / target_fps as f64);
    let tuning = parse_tuning(&tune_spec);

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
        codec,
        keyframe_interval: None,
        // #92: the bench measures the CBR path, same as the default session.
        rc: scrap::VpxRateControl::Cbr,
        cq_level: 0,
        // #95: what is under test. `--tune legacy` reproduces the old encoder.
        tuning,
    });
    let mut encoder = Encoder::new(cfg, false)?;
    println!(
        "[bench] encoder: {:?}  target_bitrate={} kbps  tune={:?}",
        codec,
        encoder.bitrate(),
        tuning
    );
    // #95: read back from libvpx, not from the request -- a clamped control
    // would otherwise make this bench compare two identical encoders.
    println!("[bench] effective: {}", encoder.effective_config());
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
    // #95: payload actually produced, so the run reports real kbps and the
    // biggest keyframe instead of only how long the encoder took.
    let mut out_bytes: u64 = 0;
    let mut key_count: u64 = 0;
    let mut key_bytes_max: usize = 0;

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
                        Ok(vf) => {
                            repeat_ok += 1;
                            let (b, key) = frame_stats(&vf);
                            out_bytes += b as u64;
                            if key {
                                key_count += 1;
                                key_bytes_max = key_bytes_max.max(b);
                            }
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
            Ok(vf) => {
                encoded_ok += 1;
                let (b, key) = frame_stats(&vf);
                out_bytes += b as u64;
                if key {
                    key_count += 1;
                    key_bytes_max = key_bytes_max.max(b);
                }
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
    // #95: the two numbers a tuning change has to move.
    let kbps = if wall > 0.0 {
        (out_bytes as f64 * 8.0 / wall / 1000.0) as u64
    } else {
        0
    };
    println!("  output_bytes        : {}", out_bytes);
    println!("  actual_kbps         : {}  (target {})", kbps, encoder.bitrate());
    println!("  keyframes           : {}  max_key_bytes={}", key_count, key_bytes_max);
    Timing::row("capture(new)", frames_ok as usize, &cap);
    Timing::row("convert(new)", frames_ok as usize, &convert);
    Timing::row("encode(new)", encoded_ok as usize, &new_encode);
    Timing::row("encode(repeat)", repeat_ok as usize, &repeat_encode);
    if let Some(err) = last_err {
        println!("  last_error          : {}", err);
    }

    Ok(())
}
