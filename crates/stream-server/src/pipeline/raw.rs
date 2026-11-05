//! #106: 原始 BGRA 帧的旁路广播 —— 只服务 JPEG 条纹（`0x03`）。
//!
//! # 为什么需要它
//!
//! 视频那条线的产物是**已编码**的 VP8/VP9 字节，而 JPEG 条纹要的是每一帧的像素。
//! 两者不可能共用一条下行：一个浏览器没有 `VideoDecoder` 时，页面会把编码器选成
//! `jpeg`（`Kc()` → `fa('jpeg', …)`），此时服务端必须从像素重新编一份 JPEG——
//! 拿 VP8 的字节解是解不出来的。
//!
//! # 为什么是全局旁路而不是改管线签名
//!
//! 采集循环手里有 BGRA，但它拿不到"这条连接想不想要原始帧"。把 fanout 一路传进
//! `run_blocking_loop` / `run_inner_beat` 要改四层签名，而这些函数的参数已经很长；
//! 相对地，这里只需要循环体里一行 `publish(...)`，没有订阅者时它是一次原子读
//! 加一次返回。
//!
//! 代价是**多一次拷贝**：`Bytes::copy_from_slice` 一份 BGRA（1080p 约 8 MB）。
//! 只在真有 JPEG 观众时发生 —— 那种观众是"浏览器缺 WebCodecs"的少数派，而且
//! 这时画面本来就走的是兜底路径。宁可多这一次 memcpy，也不要为了省它去改
//! 管线的控制流。
//!
//! # 队列与丢弃
//!
//! 队列只有 [`QUEUE`] 帧，满了就丢**新**帧：JPEG 编码比采集慢时，正确的惩罚是
//! "这个观众少看几帧"，而不是把整条管线拖住（`publish` 在采集线程上跑，一次
//! `blocking_send` 就能让编码器饿死）。

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;
use tokio::sync::mpsc;

/// 每个订阅者能积压的帧数。2 帧 ≈ 60 ms（30 fps）的余量。
pub const QUEUE: usize = 2;

/// 一帧原始像素。
///
/// `data` 是**紧凑**的 BGRA（每行 `w * 4` 字节，无 padding）—— 采集循环里
/// `scaled_buf` / `captured.data` 就是这个布局，JPEG 编码器要的也是它。
#[derive(Clone, Debug)]
pub struct RawFrame {
    /// 这块屏的索引。全局广播要带上它，否则双屏机器上的两个观众会互相串帧。
    pub display: u32,
    pub w: u32,
    pub h: u32,
    pub data: Bytes,
}

impl RawFrame {
    /// 第 `y` 行起的子切片，长度为 `rows` 行。`None` 表示越界。
    pub fn rows(&self, y: u32, rows: u32) -> Option<&[u8]> {
        let stride = self.w as usize * 4;
        let start = y as usize * stride;
        let end = start.checked_add(rows as usize * stride)?;
        self.data.get(start..end)
    }
}

struct Tap {
    subs: Mutex<Vec<(u64, mpsc::Sender<Arc<RawFrame>>)>>,
    /// 订阅者数量，单独放一个原子：没有观众时的快路径不能去抢 `subs` 的锁，
    /// 那样每一帧都要和一条可能正在退出的连接握手。
    count: AtomicUsize,
    next: AtomicU64,
}

fn tap() -> &'static Tap {
    static TAP: OnceLock<Tap> = OnceLock::new();
    TAP.get_or_init(|| Tap {
        subs: Mutex::new(Vec::new()),
        count: AtomicUsize::new(0),
        next: AtomicU64::new(1),
    })
}

/// 当前有多少个订阅者。采集循环用它决定要不要拷这一帧。
pub fn subscribers() -> usize {
    tap().count.load(Ordering::Relaxed)
}

/// 发布一帧。没有订阅者时立即返回（一次原子读）。
pub fn publish(display: u32, data: &[u8], w: u32, h: u32) {
    let tap = tap();
    if tap.count.load(Ordering::Relaxed) == 0 {
        return;
    }
    if w == 0 || h == 0 || data.len() < (w as usize * 4) * h as usize {
        return;
    }
    let frame = Arc::new(RawFrame {
        display,
        w,
        h,
        data: Bytes::copy_from_slice(data),
    });
    let mut closed: Vec<u64> = Vec::new();
    {
        let subs = tap.subs.lock().unwrap_or_else(|p| p.into_inner());
        for (id, tx) in subs.iter() {
            match tx.try_send(frame.clone()) {
                Ok(()) => {}
                // 满了：丢这一帧。慢观众的正确惩罚是少看几帧，不是拖住采集。
                Err(mpsc::error::TrySendError::Full(_)) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => closed.push(*id),
            }
        }
    }
    if closed.is_empty() {
        return;
    }
    let mut subs = tap.subs.lock().unwrap_or_else(|p| p.into_inner());
    subs.retain(|(id, _)| !closed.contains(id));
    tap.count.store(subs.len(), Ordering::Relaxed);
}

/// 一个订阅。`Drop` 时自动退订 —— 连接断掉不需要任何人记得清理。
pub struct RawSubscription {
    id: u64,
    rx: mpsc::Receiver<Arc<RawFrame>>,
}

impl RawSubscription {
    /// 下一帧。通道关闭（发布端没了）时返回 `None`。
    pub async fn recv(&mut self) -> Option<Arc<RawFrame>> {
        self.rx.recv().await
    }

    /// 阻塞版本，给编码线程用（JPEG 编码是同步 CPU 活）。
    pub fn blocking_recv(&mut self) -> Option<Arc<RawFrame>> {
        self.rx.blocking_recv()
    }
}

impl Drop for RawSubscription {
    fn drop(&mut self) {
        let tap = tap();
        let mut subs = tap.subs.lock().unwrap_or_else(|p| p.into_inner());
        subs.retain(|(id, _)| *id != self.id);
        tap.count.store(subs.len(), Ordering::Relaxed);
    }
}

/// 订阅原始帧。
pub fn subscribe() -> RawSubscription {
    let tap = tap();
    let (tx, rx) = mpsc::channel(QUEUE);
    let id = tap.next.fetch_add(1, Ordering::Relaxed);
    let mut subs = tap.subs.lock().unwrap_or_else(|p| p.into_inner());
    subs.push((id, tx));
    tap.count.store(subs.len(), Ordering::Relaxed);
    RawSubscription { id, rx }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这组用例共用进程级的 tap，而 `subscribers()` 是**全局**计数：Rust
    /// 默认并行跑测试，断言绝对数量的用例会互相踩 —— 一个刚 `subscribe`，
    /// 另一个的 `assert_eq!(subscribers(), 1)` 就看见了 2。串起来跑。
    ///
    /// 这不是"测试写得不够好"：`publish` 的早退判据本来就是这个全局计数，
    /// 想测它就得独占它。五个微秒级用例，串行的代价是零。
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 独占 tap。用例 panic 会 poison 这把锁，`into_inner` 照常取用。
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn frame(display: u32, w: u32, h: u32) -> Vec<u8> {
        vec![0x80; (w * h * 4) as usize]
    }

    #[test]
    fn nobody_listening_means_nothing_happens() {
        let _serial = serial();
        assert_eq!(subscribers(), 0);
        // 没有订阅者时这条调用必须是无害的，哪怕指针是空的。
        publish(0, &[], 0, 0);
        publish(0, &frame(0, 2, 2), 2, 2);
    }

    #[test]
    fn a_subscriber_gets_the_frame_with_its_display() {
        let _serial = serial();
        let mut sub = subscribe();
        assert_eq!(subscribers(), 1);
        let data = frame(1, 4, 2);
        publish(1, &data, 4, 2);
        let got = sub.blocking_recv().expect("the frame just published");
        assert_eq!(got.display, 1);
        assert_eq!((got.w, got.h), (4, 2));
        assert_eq!(&got.data[..], &data[..], "the pixels must survive intact");
    }

    #[test]
    fn dropping_the_subscription_empties_the_tap() {
        let _serial = serial();
        let sub = subscribe();
        assert_eq!(subscribers(), 1);
        drop(sub);
        assert_eq!(subscribers(), 0);
    }

    #[test]
    fn rows_are_a_tight_window_into_the_pixels() {
        let mut data = vec![0u8; 4 * 3 * 4]; // 4x3 BGRA
        for (i, b) in data.iter_mut().enumerate() {
            *b = i as u8;
        }
        let f = RawFrame {
            display: 0,
            w: 4,
            h: 3,
            data: Bytes::from(data),
        };
        let row1 = f.rows(1, 1).expect("row 1 exists");
        assert_eq!(row1.len(), 16);
        assert_eq!(row1[0], 16, "row 1 starts after one stride of 16 bytes");
        assert!(f.rows(2, 2).is_none(), "two rows from the last one overruns");
        assert!(f.rows(3, 1).is_none(), "past the end");
    }

    #[test]
    fn a_full_queue_drops_instead_of_blocking() {
        let _serial = serial();
        let mut sub = subscribe();
        // 采集线程不能被拖住：多出来的帧直接丢，`publish` 立刻返回。
        for _ in 0..QUEUE + 3 {
            publish(0, &frame(0, 2, 2), 2, 2);
        }
        assert!(sub.blocking_recv().is_some());
    }
}
