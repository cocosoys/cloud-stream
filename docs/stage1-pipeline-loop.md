# 阶段 1 · #36 采集编码循环与统计装配记录（4.3 / 6.3 / 5.4 / 9.2）

> 路线 A：以 RustDesk fork `49c89bd` 为基底。本文件记录 `crates/stream-server/src/pipeline/`、
> `src/platform/sysinfo.rs`、`src/dispatch/output.rs` 与 `src/signaling/websession.rs` 中采集编码主体、
> repeat 帧补 Sample、流水线统计聚合、主机口径采样器从空壳占位到接通 WebRTC 轨道的取证结论、
> 编译期纠偏清单、#37 部署验收对接接口与阶段 1 边界。
> **编译口径：debug 构建，`toolchain\build-ws.cmd build -p stream-server -j2`，EXIT=0（强制重链 13.32s）；
> `cargo test -p stream-server` 2 个路由单测全部通过（2 passed; 0 failed）；stream-server 自身零 warning。
> 残留 warning 全部来自 vendor hbb_common / scrap 已知遗留项（见第 7 节）。**

## 1. 本任务范围与验收结论

### 1.1 已落地（对照设计稿 4.3 / 6.3 / 5.4 / 9.2）

| 设计稿小节 | 落地内容 | 代码位置 |
| --- | --- | --- |
| 4.3 采集源 | 枚举 `Display::all()`，优先 `is_primary()`，首台兜底；打开 `Capturer`；缓存原生宽高、GDI 标志、显示名 | `pipeline\capture.rs:35–69` |
| 4.3 VP8 编码器 | `EncoderCfg::VPX(VpxEncoderConfig{width, height, quality:1.0, codec:VP8, keyframe_interval:None})`；`Encoder::new(cfg, false)`；缓存 `yuvfmt()`；`VpxNoOutput` 归一为 `Ok(None)` | `pipeline\encoder.rs:37–72` |
| 4.3 / 6.3 编码帧切片 | `VideoFrame.union=Vp8s` 逐帧拆成 `EncodedChunk{data,key,pts}`；非 VP8 union 记 debug 后返回空列表；prost `data` 为 `bytes::Bytes`，直接 move 不拷贝 | `pipeline\sample_map.rs:34–53` |
| 4.3 repeat 帧补 Sample | `WouldBlock` 在 `REPEAT_ENCODE_MAX=10` 内复用缓冲 YUV 重编码；超上限静默跳帧；新帧到达清零 repeat 计数；每拍 `sleep_to(spf)` 补偿，**duration 恒为 spf**（设计稿 9.2 #4） | `pipeline\mod.rs:185–205`、`315–321` |
| 4.3 节拍与错误阈值 | 采集错误连续 ≥3 次终止循环；无效帧不计入编码分组；新帧 `frame.to()` 失败记 `encode_err` 后不 sleep 继续下一拍 | `pipeline\mod.rs:206–241` |
| 5.4 流水线统计 | `Timing`（sum/min/max/n）、`FrameCounters`（new/repeat/would_block/invalid/capture_err/encoded_ok/encode_no_output/encode_err/repeat_ok/repeat_skipped/write_sample_err）；`snapshot(window)` 出 `PipelineSnapshot`；`reset_window()` 清零 | `pipeline\stats.rs:16–122` |
| 5.4 统计口径 | `encoded_fps=(new_frames+repeat_ok)/wall`（total 口径，对齐 bench v2 `fps(total)`）；`encode_ms` 仅新帧分组（Ok/VpxNoOutput/Err 同 bench `new_encode`），repeat 帧只计数不带计时；`pipeline_ms` 仅成功新帧端到端 | `pipeline\stats.rs:95–106`、`pipeline\mod.rs:244–279` |
| 9.2 #3 主机采样器 | `cpu_percent`=本进程 `cpu_usage()`（单核口径）；`mem_used/mem_total`=主机口径；`refresh()` 只刷主机内存与本进程条目，不遍历全进程表；经 hbb_common re-export 的 sysinfo fork 0.29.10，不引第二份 | `platform\sysinfo.rs:27–63` |
| 5.4 统计上链 | 1s 周期任务取 `PipelineStats::snapshot(1s)` 后 `reset_window()`；`StreamStats::from_host(host).with_pipeline(&snapshot)` 合并三字段再发 `stream_stats` | `signaling\websession.rs:334–365`、`dispatch\output.rs:56–73` |
| 4.3 循环接线 | PC `Connected` 首次装配 `spawn_loop`（`loop_started` 防重入）；`START_VIDEO`/`STOP_VIDEO` 翻闸门；会话退出 `pipeline.stop_loop()` 停线程与闸门 | `signaling\websession.rs:316–319`、`133` |

### 1.2 验收动作与结果

- 编译命令：`toolchain\build-ws.cmd build -p stream-server -j2` → `EXIT=0`，`Finished dev [unoptimized + debuginfo]
  target(s) in 13.32s`（强制重链轮），`Compiling stream-server` 之后本 crate 零 warning。
- 测试命令：`toolchain\build-ws.cmd test -p stream-server -j2` → `running 2 tests`，
  `routes_signaling_wildcard` / `routes_status_and_static` 均 `ok`，`2 passed; 0 failed; 0 ignored`。
- 尚未做端到端出画面验收（属 #37：编译产物部署 143、`schtasks /it` 交互会话、浏览器
  `https://192.168.59.143:8080/` 出画面、面板编码器/捕获/解码器三项可用、MC 共存复测）；
  本任务只闭环采集 → VP8 编码 → channel → track Sample 的进程内装配链与统计聚合。

## 2. 线程模型（RustDesk `video_service.rs` 范式）

### 2.1 为什么不能把采集循环放进 `tokio::spawn`

`scrap` 的采集与编码句柄持有**非 `Send`** 的 Win32 / DXGI 原始指针：

- `Capturer`：DXGI `ID3D11Device` / `ID3D11DeviceContext` / `IDXGIOutputDuplication` / `IDXGISurface` /
  `ID3D11Texture2D` / `IDXGIOutput1` / `IDXGIAdapter1` / `HMONITOR__`；GDI 路径另有 HDC / HBITMAP；
- `Frame<'_>`：`Texture(*mut c_void, usize)` 变体；
- `Encoder`：`Box<dyn EncoderApi>`，vpx 实现内部持有 `VpxCodecCtx` 与帧缓冲裸指针。

这些类型全部不实现 `Send`，一旦和 `tokio::time::sleep(spf).await`、`video.write_sample().await`
出现在同一个 `async move` future 里，编译器即报 `future cannot be sent between threads safely`
（本轮第三轮构建一次性给出 11 条非 Send 诊断，见第 3 节 #5）。RustDesk 主仓同样把采集编码主体
放在独立线程循环，只把编码帧经通道交给 webrtc 发送协程——本任务按同一边界重构。

### 2.2 两任务 + 一通道结构

```
                        ┌───────────────────────────────────────────────────┐
  ctx.pipeline (Handle) │ video_on: Arc<AtomicBool>  gate (START/STOP_VIDEO)  │
                        │ stopped : Arc<AtomicBool>  session cleanup stop_loop│
                        │ stats   : Arc<Mutex<PipelineStats>>  std Mutex      │
                        └───────┬───────────────────────┬────────────────────┘
                                │                       │
            ┌───────────────────▼─────────┐   ┌─────────▼──────────────────┐
  Task A    │ tokio::task::spawn_blocking │   │ tokio::spawn (async)       │
  阻塞 worker│ CaptureSource / VideoEncoder│  │ forwarder                  │
  (非 Send  │ 只活在本线程内，不跨 await    │  │ 持有 VideoTrack            │
  类型闭包) │ 抓帧 → YUV → VP8 编码        │  │ rx.recv().await →          │
            │ repeat 帧复用上一缓冲         │  │ video.write_sample(Bytes,  │
            │ bump() 短临界区写统计         │  │                     spf)   │
            └───────────────┬─────────────┘   └────────────▲───────────────┘
                            │ mpsc::unbounded_channel<Bytes> │
                            └────────────────────────────────┘
```

| 任务 | 调度器 | 可跨线程对象 | 禁止跨边界对象 | 职责 |
| --- | --- | --- | --- | --- |
| Task A 采集编码 worker | `spawn_blocking` 专用线程池 | `Arc<Config>`、`PipelineHandle`、`mpsc::UnboundedSender<Bytes>`、std `MutexGuard`（短临界区即 drop） | `Capturer`、`Frame`、`Encoder`、`yuv`/`mid_data`、`EncodeInput` 借用 | 开采集源/编码器；节拍抓帧；WouldBlock repeat；编码；把 `Bytes` 投入通道；写计数器与计时组；连续 3 次采集错误终止 |
| Task B Sample 转发器 | `tokio` 异步调度器 | `VideoTrack`（内部 `Arc<TrackLocalStaticSample>`，`#[derive(Clone)]`）、`spf`、stopped 标志 | 不碰任何 scrap 句柄 | 收通道 `Bytes`；未 stopped 时 `write_sample(data, spf).await`；通道关闭后自然退出 |
| 统计周期任务（#35 既有） | `tokio` 异步调度器 | `Arc<Mutex<PipelineStats>>`（std Mutex）、`Sampler`、`Arc<Mutex<MediaSession>>` | 锁 guard 不跨 await：`snapshot()` 在块内完成、`PipelineSnapshot` 是 `Copy` 先出锁再 `reset_window()` | 每秒在 `stats_on=true` 时采样主机、取流水线快照、合并 `StreamStats` 上 DC |

### 2.3 阻塞 worker 单拍时序（对齐 bench v2 规则）

1. `stopped` 置位 → 退出循环，记 info 日志；
2. `video_on` 关闭 → `std::thread::sleep(spf)` 后 continue，不抓帧、不碰编码器；
3. 记录 `beat=Instant::now()`；
4. `source.frame(spf)`：
   - `WouldBlock`：`would_block += 1`；`yuv_valid && repeat_counter < 10` 时 `repeat_counter += 1`，
     以 `EncodeInput::YUV(yuv.as_slice())` 重编码：`Some` 帧记 `repeat_ok + repeat_frames`，
     **repeat 耗时不进新帧计时分组**（对齐 bench `repeat_encode` 独立分组，阶段 1 不上报该组）；
     `VpxNoOutput` / 错误记 `repeat_skipped`；否则 `repeat_skipped += 1`；
   - `Err`：`capture_err += 1` 写统计并 warn；连续 ≥3 次 `bail!` 终止；否则 continue（不 sleep）；
   - `Ok(frame)` 且 `!frame.valid()`：`invalid += 1`，sleep 到节拍后 continue；
5. 新帧到达：`repeat_counter=0`、`capture_err=0`；`frame.to(yuvfmt.clone(), &mut yuv, &mut mid_data)`
   失败记 `encode_err` 后 continue（不 sleep）；成功置 `yuv_valid=true`；
6. 新帧编码：计时 `enc_start`；`encode_to_message(input, pts_ms)`：
   - `Some(encoded)`：记 `new_frames + encoded_ok`，`encode_timing.add(enc_ms)`，
     `pipeline_timing.add(beat.elapsed())`，`first_frame_seen/warmed_up=true`，`forward_chunks` 投通道；
   - `None`：`encode_no_output += 1`，计时入组；
   - `Err`：`encode_err += 1`，计时入组；
7. `sleep_to(spf, beat)`：已超时则不睡，未到点睡剩余时长。

### 2.4 通道容量与失败处理

- `mpsc::unbounded_channel::<Bytes>()`：阶段 1 单会话单生产者；阻塞 worker 不阻塞在 channel 发送上；
- 转发器退出条件：worker 返回 → sender drop → `rx.recv()` 返回 `None` → forwarder 循环退出；
  会话清理 `stop_loop()` 置 `stopped`，worker 下一拍退出，不依赖强制 abort；
- `tx.send` 失败（转发器已退）记 `write_sample_err += 1` 并 break 当前帧循环，不再投后续 chunk；
- `write_sample` 内部错误仅 debug 日志：轨道未绑定时库内静默 Ok（#34 已核实），绑定后 packetizer 分包。

## 3. 编译期纠偏清单（本轮七轮收敛的真实问题）

> 下列条目是实际编译/类型检查报错后按 scrap / prost / sysinfo / webrtc fork 源码修正的结论；
> 后续编码不得回退到草稿写法。

| # | 轮次 | 最初写法 / 直觉 | 实测结论与修法 | 证据位置 |
| --- | --- | --- | --- | --- |
| 1 | 1 | `pipeline\mod.rs` 经 `crate::webrtc::VideoTrack` 导入 | `webrtc\mod.rs` 原 `use track::VideoTrack` 是私有 use，E0603 `private struct import`；改为 `pub use track::VideoTrack` | `webrtc\mod.rs:55` |
| 2 | 1 | `Sampler::new().unwrap_or_default()` | 新 `Sampler` 持有 `System` 与 `Pid`，不满足 `Default`（E0277）；改为 `match` 构造：成功进任务，失败记 warn 后 `return` 退出统计任务，不再造零值回退 | `signaling\websession.rs:340–346` |
| 3 | 1 | `EncodedChunk.data: Vec<u8>`，`f.data` 直接赋值 | 本工程 prost bytes 映射为 **`bytes::Bytes`**（非 `Vec<u8>`），E0308 mismatched types；字段改 `Bytes`，`into_iter().map` 直接 move `f.data`，转发器 `chunk.data` 直接喂 `write_sample`，不再 `Bytes::from` 二次拷贝 | `pipeline\sample_map.rs:23–30`、`pipeline\mod.rs:290–302` |
| 4 | 2 | `stats_pipeline.stats().lock().await` 链式临时取锁 | E0716：临时 `Arc` 在语句末释放，guard 借用悬空；先 `let stats_arc = stats_pipeline.stats();` 绑定句柄，再两次取锁；std Mutex 后改用 `.lock().unwrap()` | `signaling\websession.rs:355–360` |
| 5 | 3 | 采集循环整体放 `tokio::spawn(async move …)`，循环内 `sleep(spf).await`、写轨 `await` | E0277 Send 违规：scrap `Capturer`/`Frame`/`Encoder`/`EncoderApi` 全部非 Send，跨 await 使 future 不可 Send（11 条指针诊断）。**重构**：主体沉到 `tokio::task::spawn_blocking`，线程内零 await；`Bytes` 经 unbounded channel 交独立 async forwarder 写轨；统计锁从 tokio `Mutex` 换成 `std::sync::Mutex`，worker 与统计任务都只持短临界区、锁不跨 await | `pipeline\mod.rs:108–137`、`139–285` |
| 6 | 5 | forwarder 与 worker 的 `async move` 共用同一个 `cfg: Arc<Config>` | 第一个 spawn 把 Arc move 走后，worker 再借触发 E0382 `use of moved value`；spawn 前 `forward_cfg = cfg.clone()`，两任务各持一份 | `pipeline\mod.rs:108–137` |
| 7 | 6 | 保留五处骨架期 `#![allow(dead_code)]` / `#![allow(unused_imports)]` | 移除后编译器暴露 14 条真实告警；按「#2/#37 预留符号局部标注、纯占位直接删除、未用 re-export 精简」三类收敛（明细见第 4.3 节） | 各 mod 与模块文件，第 4.3 表 |
| 8 | 6 | `capture.rs` `let mut capturer = Capturer::new(…)` | `is_gdi()` 取 `&self`，绑定后无需 mut，E0499 风格告警；去掉 `mut` | `pipeline\capture.rs:57` |

## 4. 落盘文件清单与职责映射

### 4.1 文件职责

| 文件 | 职责 | 关键公开符号 |
| --- | --- | --- |
| `pipeline\capture.rs` | 显示器枚举（主显优先）、采集器生命周期、帧获取透传、原生尺寸/后端/显示名缓存 | `CaptureSource`（`open_primary` / `frame` / `dimensions` / `is_gdi` / `display_name`） |
| `pipeline\encoder.rs` | VP8 `EncoderCfg`/`Encoder` 封装、YUV 格式缓存、`VpxNoOutput` 归一、码率与硬件标志透传 | `VideoEncoder`（`new` / `yuvfmt` / `encode` / `bitrate` / `is_hardware`） |
| `pipeline\sample_map.rs` | `VideoFrame` oneof 切片为 RTP-ready chunk；非 VP8 union 兜底空列表；codec 标签 debug | `EncodedChunk`、`video_frame_to_chunks` |
| `pipeline\stats.rs` | 计时组、帧计数器、流水线统计聚合、1s 窗口快照与清零、轻量 Copy 快照结构 | `Timing`、`FrameCounters`、`PipelineStats`、`PipelineSnapshot` |
| `pipeline\mod.rs` | 闸门/停止/统计句柄、两任务装配、阻塞采集编码主体、通道转发、节拍补偿、短临界区统计更新 | `PipelineHandle`、`spawn_loop` |
| `platform\sysinfo.rs` | 主机内存与本进程 CPU 采样；sysinfo fork 经 hbb_common re-export，不引第二份 | `Sampler`、`HostSample` |
| `dispatch\output.rs` | 下行 JSON 构造；主机样本与流水线快照分别合并进 `StreamStats` | `StreamStats::from_host`、`StreamStats::with_pipeline` |
| `signaling\websession.rs` | Connected 首次装配循环、1s 统计任务合并流水线快照、会话退出停循环、Sampler 构造失败分支 | `SessionCtx.pipeline`、`start_media_session` 内 watcher / stats 任务 |

### 4.2 一次会话内循环装配时序

1. 浏览器完成 WS 握手与 SDP/ICE 协商（#35/#34 链路）；
2. PC `Connected` → watcher 任务 `arm_connected_notices` 挂 control open 三件套与上行命令 hook；
3. `loop_started=false` 首次 Connected：`media.video().clone()` 交出轨道副本 → `spawn_loop(cfg.clone(), video, pipeline.clone())`；
4. `spawn_loop`：建 `unbounded_channel<Bytes>`，先起 async forwarder，再起 `spawn_blocking` worker；
5. worker 打开 `CaptureSource::open_primary()` 与 `VideoEncoder::new(…)`，日志输出分辨率/后端/编码器硬件标志；
6. 前端 DC open → 发 `_stats,1`、`START_VIDEO`：`stats_on=true`、`video_on=true`；
7. worker 节拍抓帧编码 → chunk 入通道 → forwarder `write_sample(data, spf).await` 进 track；
8. 前端发 `STOP_VIDEO`：`video_on=false`，worker 每拍空转 sleep，不抓帧不编码；
9. 1s 统计任务：`sampler.refresh()` → 主机 `HostSample`；`PipelineStats::snapshot(1s)` → `reset_window()`；
   `StreamStats::from_host(host).with_pipeline(&snapshot)` → `stream_stats` 下行；
10. 会话终态（PC Failed/Closed、WS Close、4001 顶号）：`media.close()` 后 `pipeline.stop_loop()`，
    worker 下一拍退出，sender drop 后 forwarder 自然退出。

### 4.3 骨架 allow 移除后的真实告警处置表（第 3 节 #7 明细）

| # | 告警 | 处置 | 理由 |
| --- | --- | --- | --- |
| 1 | `dispatch\mod.rs` 未用 re-export `InputCounters`、`InputSink` | 精简为 `InputSinkRef`、`NullInputSink`；trait 与计数结构定义留在 `input_stub` 模块内供 `InputSinkRef` 约束使用 | 阶段 2 enigo 实现再按需导出 |
| 2 | `pipeline\mod.rs` 未用 re-export `FrameCounters`、`Timing`、`EncodedChunk` | 精简为 `PipelineSnapshot`、`PipelineStats`、`CaptureSource`、`VideoEncoder`、`video_frame_to_chunks`；内部类型留在各自模块 | 外部装配只消费快照与句柄 |
| 3 | `platform\mod.rs` 未用 re-export `capture_backend_name` | 函数不导出；本体加 `#[allow(dead_code)]` | #37 stream_info 回填真实 gdi/后端名时直接调用 |
| 4 | `NullInputSink::counters()` 未用 | 方法加 `#[allow(dead_code)]`；`InputCounters` 结构保留 | 阶段 2 输入面健康日志快照使用 |
| 5 | `CaptureSource.gdi/name` 字段、`is_gdi()`、`display_name()` 未读 | 字段与方法各加 `#[allow(dead_code)]`，构造时照常缓存 | #37 `stream_info.capture` / `capture_reason` / `encode_node` 回填真实后端与显示名 |
| 6 | `VideoEncoder.width/height` 字段与 `width()`、`height()` 未用 | **删除**字段与访问器；`yuvfmt()` 已携带实际尺寸，构造日志保留入参宽高 | 无消费方，保留只会造成尺寸双口径漂移 |
| 7 | `Timing::count()` 未用 | **删除**该方法 | 快照只取 `avg_ms()`，n 字段保留供未来调试 |
| 8 | `host::capture_backend_name` 函数未用 | 保留函数 + `#[allow(dead_code)]` | #37 gdi → `"gdi"` / `"dxgi"` 标签映射 |
| 9 | `HandshakeState::Connected/Streaming/Closed` 未构造、`is_connected()` 未用 | 枚举整体 `#[allow(dead_code)]`；方法 `#[allow(dead_code)]` | #37 状态审计与阶段 3 resume 判定沿用状态机预留位 |
| 10 | `MediaSession::ice_rx()` 旧访问器未用 | **删除**；`take_ice_rx()` 为唯一取接收器入口 | 旧借用语义与 tokio mpsc 非 Clone 约束冲突，已被取代 |
| 11 | `MediaSession::is_closed()` 未用 | **删除**；`closed` 原子字段保留给 `close()` 内部 `swap` | 外部清理走 `close()`，无需独立读位方法 |

## 5. #37 部署出画面对接接口面（落码直接引用）

### 5.1 循环句柄与会话状态

```rust
// 会话上下文已持有：
ctx.pipeline.start();    // START_VIDEO 上行后翻闸门（#35 hook 已接线）
ctx.pipeline.stop();     // STOP_VIDEO 上行后关闸门
ctx.pipeline.is_on();    // worker 每拍首步读取
ctx.pipeline.stop_loop();// 会话终态清理：stopped=true 且 video_on=false
ctx.pipeline.stats();    // Arc<std::sync::Mutex<PipelineStats>>，统计任务取快照用
```

### 5.2 轨道写入口径

```rust
// forwarder 唯一写轨点；duration 恒为 cfg.spf()，repeat 帧同口径（设计稿 9.2 #4）
video.write_sample(chunk.data, cfg.spf()).await?;
// 轨道未绑定时库内静默 Ok；绑定后 packetizer 负责 RTP 序列号与媒体时钟；
// Sample 无 keyframe 字段，key 仅在统计/调试侧记账，不进 Sample 结构。
```

### 5.3 待 #37 回填的真实采集/编码信息

当前 `arm_connected_notices` 三件套中 `stream_info(StreamInfo::default())` 仍发占位值
（`capture="dxgi"`、`is_gdi=false`、空 reason），#37 接线时：

| 前端字段（`lib/stream-stats.js:46–71`） | 数据源（本任务已备好但未接出） | 接法 |
| --- | --- | --- |
| `capture` / `is_gdi` | `CaptureSource::is_gdi()`、`host::capture_backend_name(gdi)` | worker 首次打开后经句柄传出；或在 Connected 前把采集源信息缓存进 `PipelineHandle`，三件套发送时读取 |
| `capture_reason` | `CaptureSource::display_name()` | 同上，备用展示位 |
| `encoder` / `hardware` | `VideoEncoder::bitrate()`、`VideoEncoder::is_hardware()` | 句柄持有编码器引用后取内部码率；阶段 1 固定 `encoder="libvpx"`、`hardware=false` |
| `codec` | 固定 `"VP8"`（阶段 3 切 VP9 时改） | 现状已一致 |
| `encode_node` | `platform::host::node_name()` | `output.rs:stream_info` 已接入，#37 核对 COMPUTERNAME 取值 |

> 注意：`VideoEncoder` 句柄目前只活在阻塞 worker 线程内，#37 把后端信息带出线程时应走
> `Arc<AtomicBool>` / `Arc<Mutex<…>>` 或一次性快照结构，**不得**把 `Encoder` / `Capturer` 句柄
> 跨线程暴露（Send 边界见第 2.1 节）。

### 5.4 统计字段映射对账（`dispatch\output.rs` ↔ 前端 `StreamSample`）

| `stream_stats.data` 字段 | 本任务来源 | 前端面板位置 |
| --- | --- | --- |
| `cpu_percent` | `Sampler.sample().cpu_percent`（本进程单核口径） | 状态栏 CPU 行 |
| `mem_used` / `mem_total` | 主机口径 `used_memory` / `total_memory` | 状态栏内存行 |
| `encoded_fps` | `PipelineSnapshot.encoded_fps`（new+repeat 窗口均摊） | 面板 fps 服务端口 |
| `encode_ms` | `PipelineSnapshot.encode_ms`（新帧分组均值，repeat 帧不计入） | 面板编码耗时行 |
| `pipeline_ms` | `PipelineSnapshot.pipeline_ms`（仅新帧端到端均值） | 面板采集到编码行 |
| `throttled` | 阶段 1 恒 `false`（无 QoS 节流） | 面板节流标志 |
| `gpu_*` / `rtt_ms` | 有意省略：无 GPU 直通、阶段 1 不做 RTT 测量 | 前端显示占位值，不计入异常 |

### 5.5 #37 部署前置条件（本任务不负责装配）

- TLS acceptor：`Config::tls_enabled()` 双路径齐备，#37 接 `tokio-rustls 0.26`；
- 143 侧计划任务 `/it` 交互令牌运行产物；旧 `MCStreamSrv`（Python）保持 disabled；
- dist 目录托管、自签证书 SAN（IP=192.168.59.143 + localhost）沿用 #35 部署清单；
- 验收顺序：先单验串流出画面 → 面板编码器/捕获/解码器三项可用 → 再启动 MC `-Xmx1G` 共存复测
  （用户拍板口径，设计稿 8.4）。

## 6. 阶段 1 已知边界与遗留项

| 项 | 阶段 1 口径 | 后续阶段 |
| --- | --- | --- |
| 采集后端 | 单屏主显优先、首台兜底；不做多显示器轮询；DXGI 三次失败回退 GDI 链路未实现；GDI 只在枚举期标志位缓存 | 阶段 3 静态刷新与后端恢复 |
| 编码配置 | VP8、quality 1.0、`keyframe_interval=None`、i444=false；`bitrate_kbps` 当前不作用于 libvpx 内部基码率，编码器按分辨率内部曲线起算 | 阶段 3 VP9 切换 + 动态码率协商（`vb,kbps` B 组命令接线） |
| 静态刷新 / 周期关键帧 | 未实现；无丢包静态帧重推；RTCP sender 缓冲区 NACK/PLI 响应循环未读（#34 同列边界） | 阶段 3 QoS |
| repeat 上限 | 10 拍；超上限静默跳帧，不发告警帧；duration 恒 spf，不随跳帧变化 | 阶段 3 按实测抖动微调上限 |
| 采集错误阈值 | 连续 3 次 `frame()` 错误终止循环；终止后不自动重开采集源，会话由终态清理关闭 | 阶段 3 采集源重建与重试 |
| 输入注入 | C 组命令全部进 `NullInputSink::note_stub` 计数，不映射 keysym→VK、不调 enigo；`pointer` DC 空挂 | 阶段 2 输入面闭环 |
| stream_info 真实后端 | 发 `StreamInfo::default()` 占位值；真实 gdi/显示名/码率未回填 | #37 联调 |
| 日志轮转 | 文件固定落 `logs\stream-server.log`，`WriteMode::Direct`；无按大小/日期轮转与保留策略 | 阶段 4 服务化补齐（设计稿 9.2 #5） |
| 多会话 / 多实例 | 单会话注册表顶号；循环句柄随会话生命周期创建与销毁 | 阶段 4 多实例调度 |
| 内存/CPU 采样 | `Sampler` 首次 `refresh()` 只打 CPU 基线，首个 tick `cpu_usage()` 为 0（sysinfo 0.29.10 语义）；第二个 tick 起为可比值 | 阶段 3 面板冷启动展示口径微调 |

## 7. 编译口径与遗留 warning（非阻塞，勿当错误）

本轮 `build-ws.log` 中 stream-server 自身零 warning；下列残留全部来自 vendor 依赖，与 #36 改动无关：

| 来源 | warning | 处置 |
| --- | --- | --- |
| `vendor\hbb_common\src\password_security.rs:213` | `secretbox` 冗余导入 | 沿用 vendor 现状，阶段 4 服务化时统一清 |
| `vendor\hbb_common\src\fingerprint.rs:251 / 283` | unknown lint `static_mut_refs` | rustc 1.75 无此 lint，忽略 |
| `vendor\scrap\src\common\vpxcodec.rs:181` | unreachable pattern | 沿用裁剪后现状 |
| `vendor\scrap` | `cargo:rustc-check-cfg requires -Zcheck-cfg flag` | 构建脚本提示，不影响产物 |

编译/测试命令与产物口径：

- 构建：`toolchain\build-ws.cmd build -p stream-server -j2`，日志 `toolchain\build-ws.log`，EXIT=0，强制重链 13.32s；
- 测试：`toolchain\build-ws.cmd test -p stream-server -j2`，日志 `toolchain\test-ws.log`，EXIT=0，2 passed / 0 failed；
- stream-server 为 binary crate，测试命令**不得**加 `--lib`（报 no library targets）；
- dev profile `codegen-units=2`，未启用 Release/LTO；0e 基线为 debug 口径，阶段 3 出 release 基线前不复用 0e CPU 数字对比。

## 8. 取证基线

- 装配链对照基准：`crates\bench-capture\src\main.rs`（0e repeat 版：WouldBlock repeat 上限 10、新帧清零、
  VpxNoOutput 单列、节拍 sleep 补偿、capture_err≥3 终止，本任务循环规则逐项对齐）。
- scrap 类型与 API 取证：
  - `vendor\scrap\src\common\mod.rs`：`TraitCapturer::frame`、`Frame` 枚举（PixelBuffer/Texture）、
    `Frame::valid()`、`Frame::to()`、`EncodeInput`、`EncodeYuvFormat`（Clone/Copy）；
  - `vendor\scrap\src\common\codec.rs`：`Encoder` / `EncoderApi`、`Encoder::new(cfg, i444)`、
    `encode_to_message`、`yuvfmt()`、`bitrate()`、`is_hardware()`、`EncoderCfg::VPX`、
    `VpxEncoderConfig`、`VpxVideoCodecId::{VP8, VP9}`、`VpxNoOutput`。
- protobuf 权威：`vendor\base\protos\message.proto:4–36`
  - `EncodedVideoFrame{bytes data=1; bool key=2; int64 pts=3}`；
  - `VideoFrame` oneof `vp8s=12` 等；prost bytes 在本工程映射 `bytes::Bytes`；
  - Rust 路径 `base::message_proto::{video_frame, VideoFrame, EncodedVideoFrames}`。
- sysinfo fork：本地 checkout `C:\Users\user\.cargo\git\checkouts\sysinfo-21cfb02656c5bb64\90b1705\src\`，
  版本 0.29.10；`hbb_common::sysinfo` 为直接 re-export，stream-server 不引第二份 sysinfo。
- 前端契约基线：`C:\Users\user\Desktop\selkies-webclient\addons\selkies-web-core\`
  - `lib\stream-stats.js:46–110`：StreamInfo / StreamClient / StreamSample 字段 typedef；
  - `lib\stream-stats-view.js:79–134`：encoder / capture / decoder 行渲染与 null 判定；
  - `lib\webrtc.js:735–805`：下行 DC 消息路由表。
- 媒体面 / 信令面取证基线沿用：`docs\stage1-webrtc-media.md` 第 8 节、`docs\stage1-signaling-session.md` 第 8 节。
- 0e 两端基线数据：本机 `toolchain\bench-local-20s-30fps.txt`（r3：468 帧 / 23.36fps / 采集 27.721ms /
  编码 10.106ms / CPU 4 核 6.0%、单核 24.0%）；143 `toolchain\check-bench-143-r2.txt`
  （r2：53 新帧 / repeat 329 / 19.08fps / 采集 51.749ms 最大 250.905ms / 编码 8.429ms / CPU 4.7%、单核 18.7%）。
