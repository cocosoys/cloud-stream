//! #28: 截图 —— 把"当前这一帧"落成 PNG，不等下一次编码。
//!
//! ## 为什么不是"取最后一帧再编码"
//!
//! 编码器交出来的是 **YUV**，而 PNG 要 RGB：把 I420 转回 RGB 等于在这个文件里
//! 再实现一遍色彩矩阵和色度上采样，还会因为 4:2:0 而丢一半色度分辨率 —— 截图上
//! 就是文字边缘发彩。所以这里取的是管线本来就留着的那张 **BGRA 快照**
//! （`pristine_bgra`，见 `pipeline::mod` 的 #70 注释：每拍都留一份、不含合成
//! 指针），它已经是目标分辨率、紧排、无 padding，正好是 `png::encode_bgra` 的输入。
//!
//! ## 为什么不直接在采集循环里编
//!
//! 756×488 一帧 PNG 大约 10–30 ms（zlib 默认压缩）。放在 60 Hz 的节拍里就是一次
//! 可见的卡顿，而截图是"人点一下"的操作，不值得让所有人陪着掉两帧。所以循环只做
//! 一次 `clone()`（1.5 MB，约 0.2 ms）并把活交给独立线程；HTTP 侧最多等
//! [`CAPTURE_TIMEOUT`]。
//!
//! ## 没有会话时
//!
//! 管线停着就没有拍子，也就没有帧。这里明确超时并返回原因，而不是返回一张旧图
//! 或空 PNG —— "截图是黑的"和"根本没有画面"在排障时是两件事。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::png;

/// 一次已经编好的截图。`Arc` 是因为 handler 与超时路径都可能持有它。
pub struct Shot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// 这一帧是什么时候落下的（用于 `Last-Modified` 与日志）。
    pub at: Instant,
    /// 编码本身花了多久。与"请求到响应"的差就是等下一拍的时间 —— 桌面静止时
    /// 新帧只有 2–5 张/秒，这段等待是主要的延迟来源，所以单独记出来。
    pub encode_ms: u64,
}

/// 等一帧的时间上限。60 Hz 下是 60 个节拍，静止桌面靠 repeat 帧也一定够；
/// 超时只可能是"没有会话"。
pub const CAPTURE_TIMEOUT: Duration = Duration::from_millis(1500);

static WANT: AtomicBool = AtomicBool::new(false);
static ENCODING: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static SLOT: Mutex<Option<Arc<Shot>>> = Mutex::new(None);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// 采集循环每拍问一次：有人要截图吗？
pub fn wanted() -> bool {
    WANT.swap(false, Ordering::SeqCst)
}

/// 采集循环把这一拍的 BGRA 交出来，编码在独立线程上做。
///
/// 同一时刻只允许一个编码线程：截图是手动操作，重叠的请求共用同一张图就够了，
/// 而并行编两张 1.5 MB 的 PNG 只会在 4 vCPU 上和编码器抢核。
pub fn submit(width: u32, height: u32, mut bgra: Vec<u8>) {
    if width == 0 || height == 0 {
        return;
    }
    if ENCODING.swap(true, Ordering::SeqCst) {
        // 已经有一张在编：这一次的请求由那张顶上。
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("screenshot-png".to_owned())
        .spawn(move || {
            let started = Instant::now();
            let png_bytes = png::encode_bgra(width, height, &bgra);
            let encode_ms = started.elapsed().as_millis() as u64;
            bgra.clear();
            if png_bytes.is_empty() {
                *LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some("frame buffer was shorter than width*height*4".to_owned());
                ENCODING.store(false, Ordering::SeqCst);
                return;
            }
            let shot = Arc::new(Shot {
                png: png_bytes,
                width,
                height,
                at: Instant::now(),
                encode_ms,
            });
            *SLOT.lock().unwrap_or_else(|p| p.into_inner()) = Some(shot);
            GENERATION.fetch_add(1, Ordering::SeqCst);
            ENCODING.store(false, Ordering::SeqCst);
        });
    if spawned.is_err() {
        ENCODING.store(false, Ordering::SeqCst);
        *LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner()) =
            Some("could not spawn the png encoder thread".to_owned());
    }
}

/// 要一张**新的**截图：置位请求，然后等编码线程交活。
///
/// 返回的是本次请求之后编出来的那一张（`GENERATION` 变了才算），所以不会把
/// 上一次的旧图当成"现在"发出去 —— 那正是这类端点最容易骗人的地方。
pub async fn capture(timeout: Duration) -> Result<Arc<Shot>, String> {
    let before = GENERATION.load(Ordering::SeqCst);
    WANT.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + timeout;
    loop {
        {
            let slot = SLOT.lock().unwrap_or_else(|p| p.into_inner());
            if GENERATION.load(Ordering::SeqCst) > before {
                if let Some(shot) = slot.as_ref() {
                    return Ok(Arc::clone(shot));
                }
            }
        }
        if Instant::now() >= deadline {
            WANT.store(false, Ordering::SeqCst);
            let err = LAST_ERROR
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            return Err(err.unwrap_or_else(|| {
                "no frame arrived in time: the pipeline is not running (no session attached?)"
                    .to_owned()
            }));
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The module state is process-wide, so the tests below must not run at the
    /// same time as each other.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// The whole point of `submit`/`capture`: a request that arrives while the
    /// pipeline is beating gets exactly the frame submitted after it, not an
    /// older one.
    #[tokio::test]
    async fn capture_returns_the_frame_submitted_after_the_request() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        *SLOT.lock().unwrap() = None;
        GENERATION.store(0, Ordering::SeqCst);
        LAST_ERROR.lock().unwrap().take();

        // A frame nobody asked for: it must not satisfy the request below.
        submit(1, 1, vec![0, 0, 255, 255]);
        while ENCODING.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let waiter = tokio::spawn(async { capture(Duration::from_secs(2)).await });
        // Give the waiter a chance to raise the flag before the next beat.
        while !WANT.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(wanted(), "the pipeline would see the request here");
        submit(2, 1, vec![255, 0, 0, 255, 0, 255, 0, 255]);

        let shot = waiter.await.expect("task").expect("a fresh shot");
        assert_eq!((shot.width, shot.height), (2, 1));
        assert_eq!(&shot.png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    }

    /// With nothing beating, the wait must end with a reason instead of hanging
    /// the connection or sending an empty body.
    #[tokio::test]
    async fn capture_without_a_pipeline_times_out_with_a_reason() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        *SLOT.lock().unwrap() = None;
        GENERATION.store(0, Ordering::SeqCst);
        *LAST_ERROR.lock().unwrap() = None;
        let err = match capture(Duration::from_millis(120)).await {
            Ok(_) => panic!("a pipeline with no beats must not produce a shot"),
            Err(err) => err,
        };
        assert!(err.contains("pipeline is not running"), "{err}");
        assert!(!WANT.load(Ordering::SeqCst), "the flag must not be left raised");
    }

    /// `wanted()` is a one-shot: the beat that sees it clears it, so a missing
    /// frame cannot turn every following beat into an encode.
    #[test]
    fn wanted_is_consumed_once() {
        WANT.store(true, Ordering::SeqCst);
        assert!(wanted());
        assert!(!wanted());
    }
}
