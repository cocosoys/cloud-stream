//! #109 虚拟麦克风的"读数端"：从指定录音端点抓 N 秒，报电平。
//!
//! 为什么需要它：装上 VB-CABLE 之后，`virtual_mic` 只能告诉我们"端点找到了、
//! 决议是往 `CABLE Input` 写"。写操作返回 0 并不等于声音真的进了线缆 ——
//! 线缆两头是否接上、驱动有没有把 render 的数据搬到 capture，只有从
//! `CABLE Output` 读一遍才知道。没有这一步，"装好了"和"装了个名字"看起来
//! 一模一样。
//!
//! 用法（服务端可以同时在跑，shared 模式允许多客户端）：
//!
//! ```text
//! mic-probe.exe                          # 抓 CABLE Output，10 秒
//! mic-probe.exe --capture Steam --seconds 15
//! mic-probe.exe --list                   # 只列出录音端点
//! ```
//!
//! 输出是纯 ASCII：中文 Windows 控制台是 OEM 代码页，源码里的中文注释不影响
//! 编译，但打印出来会变成乱码，所以运行期只输出数字和英文。
//!
//! 判定标准写在最后一行：`VERDICT=SIGNAL`（有超过 1% 的帧不是静音）或
//! `VERDICT=SILENT`（全程静音）。脚本可以直接 grep 这一行。

#[cfg(not(windows))]
fn main() {
    eprintln!("mic-probe only supports Windows");
}

#[cfg(windows)]
fn main() {
    std::process::exit(win::run());
}

#[cfg(windows)]
mod win {
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::{
        eCapture, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceCollection,
        IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE, WAVE_FORMAT_PCM, WAVEFORMATEX,
    };
    use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
    };
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows::Win32::System::Variant::{VT_BSTR, VT_LPWSTR};
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

    use std::time::{Duration, Instant};

    /// 请求的采集格式。48k/16/2 与下行一致，AUTOCONVERTPCM 让引擎替我们转换，
    /// 于是不用去解析设备原生的 WAVEFORMATEXTENSIBLE（那可能还是 32 位浮点）。
    const SAMPLE_RATE: u32 = 48_000;
    const CHANNELS: u16 = 2;

    struct Args {
        capture: String,
        seconds: u64,
        list_only: bool,
    }

    fn parse_args() -> Result<Args, String> {
        let mut args = Args {
            capture: "CABLE Output".to_owned(),
            seconds: 10,
            list_only: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--list" | "-l" => args.list_only = true,
                "--capture" | "-c" => {
                    args.capture = it.next().ok_or("--capture needs a value")?;
                }
                "--seconds" | "-s" => {
                    let raw = it.next().ok_or("--seconds needs a value")?;
                    args.seconds = raw.parse().map_err(|_| format!("bad --seconds {raw:?}"))?;
                }
                "--help" | "-h" => {
                    println!("usage: mic-probe [--capture SUBSTR] [--seconds N] [--list]");
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        Ok(args)
    }

    pub fn run() -> i32 {
        let args = match parse_args() {
            Ok(a) => a,
            Err(err) => {
                eprintln!("{err}");
                return 2;
            }
        };
        match unsafe { capture(&args) } {
            Ok(code) => code,
            Err(err) => {
                eprintln!("probe failed: {err}");
                2
            }
        }
    }

    unsafe fn capture(args: &Args) -> Result<i32, String> {
        // RPC_E_CHANGED_MODE 之类的失败不算错：这个进程本来就没有 apartment。
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        let list: IMMDeviceCollection = enumerator
            .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
            .map_err(|e| format!("EnumAudioEndpoints(eCapture): {e}"))?;

        let mut names = Vec::new();
        let count = list.GetCount().unwrap_or(0);
        for i in 0..count {
            let Ok(device) = list.Item(i) else { continue };
            if let Some(name) = friendly_name(&device) {
                names.push((i, name, device));
            }
        }

        if args.list_only {
            println!("recording endpoints:");
            for (_, name, _) in &names {
                println!("  {name}");
            }
            return Ok(0);
        }

        let needle = args.capture.to_ascii_lowercase();
        let Some((_, name, device)) = names
            .iter()
            .find(|(_, n, _)| n.to_ascii_lowercase().contains(&needle))
        else {
            eprintln!("no recording endpoint matches {:?}", args.capture);
            for (_, name, _) in &names {
                eprintln!("  have: {name}");
            }
            return Ok(2);
        };
        println!("capture: {name}");

        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| format!("Activate(IAudioClient): {e}"))?;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: CHANNELS,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * CHANNELS as u32 * 2,
            nBlockAlign: CHANNELS * 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let event: HANDLE = CreateEventW(None, false, false, None).map_err(|e| e.to_string())?;
        if event.is_invalid() {
            return Err("CreateEventW returned an invalid handle".to_owned());
        }
        // 0 时长 = 引擎默认周期；AUTOCONVERTPCM 允许端点不是 48k/16/2。
        let init = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            0,
            0,
            &format,
            None,
        );
        if let Err(err) = init {
            let _ = CloseHandle(event);
            return Err(format!("Initialize(48k/16/2 shared): {err}"));
        }
        client.SetEventHandle(event).map_err(|e| e.to_string())?;
        let capture: IAudioCaptureClient = client
            .GetService()
            .map_err(|e| format!("GetService(IAudioCaptureClient): {e}"))?;
        client.Start().map_err(|e| format!("Start: {e}"))?;
        println!(
            "listening {} s at {} Hz / {} ch (Ctrl-C to stop early)",
            args.seconds, SAMPLE_RATE, CHANNELS
        );

        let deadline = Instant::now() + Duration::from_secs(args.seconds);
        let mut seg_start = Instant::now();
        let mut seg_sq = 0f64;
        let mut seg_n = 0u64;
        let mut seg_frames = 0u64;
        let mut seg_peak = 0i32;
        let mut total_n = 0u64;
        let mut total_loud = 0u64;

        while Instant::now() < deadline {
            // 100 ms 只是安全网的超时：没有信号时端点不一定会 signal。
            if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
                continue;
            }
            loop {
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                if let Err(err) = capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) {
                    let _ = CloseHandle(event);
                    let _ = client.Stop();
                    return Err(format!("GetBuffer: {err}"));
                }
                if frames == 0 {
                    let _ = capture.ReleaseBuffer(0);
                    break;
                }
                seg_frames += frames as u64;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                if !silent && !data.is_null() {
                    let n = frames as usize * CHANNELS as usize;
                    let samples = std::slice::from_raw_parts(data as *const i16, n);
                    for &s in samples {
                        let v = s as i32;
                        seg_sq += (v as f64) * (v as f64);
                        seg_peak = seg_peak.max(v.abs());
                    }
                    seg_n += n as u64;
                    total_n += n as u64;
                }
                let _ = capture.ReleaseBuffer(frames);
                if capture.GetNextPacketSize().unwrap_or(0) == 0 {
                    break;
                }
            }

            // 每 500 ms 报一行，太久就看不出"哪一秒没声音"。
            if seg_start.elapsed() >= Duration::from_millis(500) {
                let elapsed = args.seconds as f64
                    - deadline.saturating_duration_since(Instant::now()).as_secs_f64();
                if seg_n == 0 {
                    println!("  t={elapsed:>5.1}s no data");
                } else {
                    let rms = 20.0 * ((seg_sq / seg_n as f64).sqrt() / 32768.0).log10();
                    let peak = if seg_peak == 0 {
                        -120.0
                    } else {
                        20.0 * (seg_peak as f64 / 32768.0).log10()
                    };
                    println!("  t={elapsed:>5.1}s rms={rms:>6.1} dBFS peak={peak:>6.1} dBFS");
                }
                // -50 dBFS ≈ 104 的采样值。整段都在这个数以下就当没声音：
                // 虚拟线缆即使没接任何源，也可能吐本底噪声，不能只看"有包"。
                if seg_peak > 103 {
                    total_loud += seg_frames;
                }
                seg_start = Instant::now();
                seg_sq = 0.0;
                seg_n = 0;
                seg_frames = 0;
                seg_peak = 0;
            }
        }

        let _ = client.Stop();
        let _ = CloseHandle(event);

        // 采样数换算成时间：48k/2ch，所以 96 个采样 = 1 ms。
        let captured_ms = total_n / (SAMPLE_RATE as u64 * CHANNELS as u64 / 1000);
        let loud_ms = total_loud * 1000 / SAMPLE_RATE as u64;
        println!("total: {captured_ms} ms captured, {loud_ms} ms above -50 dBFS");
        if loud_ms == 0 {
            println!("VERDICT=SILENT (nothing above -50 dBFS reached this endpoint)");
        } else {
            println!("VERDICT=SIGNAL");
        }
        Ok(0)
    }

    /// 读端点的友好名。与 `platform/virtual_mic.rs` 里那段一致：这个 crate
    /// 版本没有 `PropVariantToStringAlloc`，直接读联合体两种字符串类型。
    unsafe fn friendly_name(device: &IMMDevice) -> Option<String> {
        let store: IPropertyStore = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut value: PROPVARIANT = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let inner = &value.Anonymous.Anonymous;
        let name = match inner.vt {
            VT_LPWSTR => {
                let raw = inner.Anonymous.pwszVal;
                if raw.is_null() {
                    None
                } else {
                    raw.to_string().ok()
                }
            }
            VT_BSTR => {
                let text = inner.Anonymous.bstrVal.to_string();
                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
            _ => None,
        };
        let _ = PropVariantClear(&mut value);
        name
    }
}
