# 阶段 1 · #42-1 方案 B：DXGI 有限重建 + GDI 回退 + 前端错误可见（设计稿）

> 路线 A：以 RustDesk fork `49c89bd` 为基底。本文件是 #37-5 MC 共存复测被 DXGI 失效阻塞后的
> 补充设计，对齐 RustDesk `src/server/video_service.rs` 采集恢复范式（方案 B，用户已拍板），并新增
> 硬要求：**后端采集/连接错误必须经下行消息推到前端状态栏可见，不允许只埋在后端日志里。**
>
> 前置装配记录：`docs/stage1-pipeline-loop.md`（#36 采集编码循环与统计）、`docs/stage1-webrtc-media.md`（#34）、
> `docs/stage1-signaling-session.md`（#35）。本文件只描述 #42-1 增量改动，既有线程模型、统计口径、
> 信令握手与 WebRTC 装配沿用前述记录，不重复展开。

## 1. 本次范围与拍板口径

### 1.1 已拍板约束（用户原话口径，实现不得回退）

| 项 | 口径 |
| --- | --- |
| 采集恢复方案 | **方案 B，对齐 RustDesk 完整版**：10 秒窗口内最多重建 DXGI 3 次，超限永久回退 GDI；WouldBlock 连续 3 次也切 GDI；仅 GDI 阶段采集错误才终止循环 |
| 错误可见性 | **连接/采集失败必须在前端反馈**，避免问题只出现在后端日志、浏览器侧无任何提示 |
| 落点 | 直接在 143 交互计划任务环境验证，不先做 142 本机闭环 |
| 验收顺序 | 先单验串流出画面恢复，再启动 MC 共存复测（画面可见 + 资源水位可接受） |
| 统计口径 | `cpu_percent`=整机 CPU（0–100，2026-10-27 由「进程自身单核」改口径，`process_cpu_percent` 保留旧值）；`mem_used/mem_total`=主机口径；本轮不新增 stream_stats 恢复计数器，恢复事件走日志 + 下行消息 |
| 不做项 | 桌面分辨率变更检测重建、静态刷新重复编码、动态码率、VP9、输入面、QoS NACK/PLI 响应（均归阶段 2/3） |

### 1.2 不在本轮范围（沿用阶段 1 边界）

- 不做多显示器枚举与切换；固定 `is_primary()`，首台兜底（#36 既有边界）。
- 不做 UAC / 桌面切换 (`try_change_desktop`) 探测；不挂隐私模式采集器。
- 不做编码器硬件回退链；固定 libvpx VP8 软编码。
- 不处理 MC 连不上 142:25574 的游戏链路问题（阶段 4）；本轮共存只验证"串流与 MC 同机运行不崩、画面可见、资源不爆"。

## 2. 故障取证（#37-5 现场证据链）

### 2.1 资源水位结论：不是 CPU / 内存问题

143 现场三轮 12s 间隔采样（`toolchain/deploy143/mc-sample.ps1`）：

| 时点 | java(MC) 工作集 | stream-server 工作集 | 主机空闲内存 | java CPU 增量(单核) | 串流 CPU 增量(单核) |
| --- | --- | --- | --- | --- | --- |
| T0 14:10:46 | 80 MB | 85 MB | 4724 MB | — | — |
| T1 14:10:59 | 144 MB | 83 MB | 4638 MB | 17.7% | 34.6% |
| T2 14:11:11 | 221 MB（Forge 加载爬升） | 69 MB | 4582 MB | 25.8% | 29.2% |
| 故障后 | 825 MB（-Xmx1G 内，进程存活） | 10 MB（worker 终止空转） | 4087 MB | 累计 83.4s | 累计 401.4s |

结论：MC 加载期 + 串流编码单核增量均在 VM 预算内，内存充裕；**崩溃点在采集链路自身，不是资源不足。**

### 2.2 服务端日志闭环（`C:\dev\stream\logs\stream-server.log`）

```
INFO  capture source opened: 1714x922 gdi=false display=\.\DISPLAY1     ← DXGI 打开正常
INFO  VP8 encoder opened: 1714x922 target=2500kbps hw=false
INFO  pipeline loop started: target 1920x1080@30fps spf=33.333ms
WARN  capturer error #1: connection reset
WARN  capturer error #2: invalid data
WARN  capturer error #3: invalid data
ERROR pipeline loop ended: capturer failed three consecutive times: invalid data
```

### 2.3 根因判定

1. MC `--fullscreen=true` 切换显示模式 / LWJGL 窗口独占桌面，DXGI `IDXGIOutputDuplication`
   返回 `DXGI_ERROR_ACCESS_LOST`，scrap 映射为 `io::ErrorKind::ConnectionReset`
   （vendor `scrap/src/dxgi/mod.rs:1017`）；
2. 访问丢失后后续帧 `DXGI_ERROR_INVALID_CALL`，映射为 `InvalidData`（:1019）；
3. 当前代码 `pipeline/mod.rs:219–227` 把**任意采集错误连续 3 次**直接 `bail!` 终止 worker：
   不重建采集器、不 DXGI→GDI 回退、不通知前端；
4. worker 终止后 `VideoTrack` 静默停帧，进程内存退到 10MB 空转；前端看门狗 3 次重发
   `START_VIDEO` 无人消费，PC `disconnected` → 重连后新会话再次打开 DXGI，又撞上失效状态，
   形成"重连即失败"死循环；浏览器停在 Play Stream 按钮，统计面板 0 帧 / 0 Mbps。

### 2.4 前端可见性缺口取证

- 下行既有类型（`lib/webrtc.js:735–805`）：`pipeline{status}`、`stream_info`、`stream_stats`、
  `connection{poor}`、`system{action}`、`cursor`、`ping`、`server_settings` 等；
- `webrtc.onstatus` / `webrtc.onerror`（`selkies-wr-core.js:3298–3305`）只进 `logEntries` +
  `console.log`，**不写状态栏 DOM**；
- `system.action` 除 `video_declined` 外均只走 debug（`:3519–3539`）；
- 状态栏 `statusDisplayElement`（`:2970–2973`）只由 PC 连接状态 `updateStatusDisplay()` 驱动，
  `connected` 即隐藏；采集恢复 / 回退 / 致命丢失事件在前端**没有任何挂载点**。

## 3. RustDesk 对照范式（方案 B 事实依据）

取证自本机副本 `C:\Users\user\Desktop\rustdesk\src\server\video_service.rs` 与 `service.rs`。

### 3.1 恢复常量（video_service.rs:69–74）

| 常量 | 值 | 含义 |
| --- | --- | --- |
| `DXGI_RECOVERY_LIMIT` | 3 | 10 秒窗口内最多重建 DXGI 的次数 |
| `DXGI_RECOVERY_WINDOW` | 10s | 计数窗口；窗口外计数清零重新计时 |
| `DXGI_RECOVERY_FRAME_GRACE` | 2s | 重建成功后的首帧宽限期，期间 WouldBlock 不立即计入 GDI 回退 |

### 3.2 恢复状态机（video_service.rs:77–120）

`DxgiRecoveryState { attempts, window_started: Option<Instant>, restart_pending, fallback_pending }`：

- `next_attempt()`：窗口已过（或未启动）→ `attempts=0`、记 `window_started=now`；
  `attempts >= LIMIT` → `fallback_pending=true`、返回 `None`（该重建已无可恢复额）；
  否则 `attempts+=1`、`restart_pending=true`、返回 `Some(attempt)`；
- `take_restart_pending()` / `take_fallback_pending()`：一次性取走标志，重建循环消费后复位。

### 3.3 采集错误分类（video_service.rs:818–1000）

| 错误类别 | RustDesk 处理 | 行号 |
| --- | --- | --- |
| `Ok(frame)` 且 `frame.valid()` | 清零 repeat 计数；走 YUV 转换 + 编码 | :819–887 |
| `WouldBlock` | 复用末帧重复编码；非恢复期内 `try_gdi>0 && !is_gdi()` 且 `try_gdi>3` → `set_gdi()`，否则 `try_gdi+=1`；新帧到达 `try_gdi=0` | :894、:902–915 |
| `ConnectionReset` 且非 GDI | `next_attempt()`：`Some(n)` → `bail!("SWITCH")` 让外层重建采集器；`None` → `take_fallback_pending()` + `set_gdi()`，`continue` 不退出 | :969–997 |
| 其他错误且非 GDI | 与 ConnectionReset 同路径：超限后 `set_gdi()` 回退，`continue` | :981–997 |
| GDI 下任意错误 | `return Err(err.into())` 终止本次 `run()` | :999 |

### 3.4 外层重建调度（service.rs:294–328）

`GenericService::run` 外层 `while sp.active()`：`callback(sp)` 返回任意 `Err`（含 `bail!("SWITCH")`）
→ 记 error 日志、按退避表（`HIBERNATE_TIMEOUT` 起倍增，封顶 `MAX_ERROR_TIMEOUT`）sleep → 重新调用
`run()`，**重建 Capturer + Encoder**。内层 `run()` 开头（video_service.rs:632–651）：
先 `get_capturer(...)`，再消费 `fallback_pending`（置 GDI）与开关选项，然后 `setup_encoder(...)`。

### 3.5 scrap 侧既有能力（已核实可用，不新增 vendor 改动）

- `TraitCapturer` 在 Windows 下已实现 `is_gdi()` / `set_gdi()`：
  `vendor/scrap/src/common/mod.rs:135–138`、`common/dxgi.rs:52–58`、`dxgi/mod.rs:421–433`；
- `set_gdi()` 仅挂载 `gdi_capturer`，不释放 DXGI 对象；`frame()` 内部 GDI 分支优先（`dxgi/mod.rs:533–545`），
  之后不再触碰已失效的 duplication；`cancel_gdi()` 可取回 DXGI 路径（本轮不使用）；
- `Capturer::new(display)` 内部 D3D11 / DuplicateOutput 失败时自动挂 GDI（`dxgi/mod.rs:111–127`），
  因此"重建采集器"等价于重新执行 `CaptureSource::open_primary()`，新对象后端由 scrap 自行判定；
- 错误码映射：`ACCESS_LOST→ConnectionReset`、`INVALID_CALL→InvalidData`、`WAIT_TIMEOUT→TimedOut`
  （`dxgi/mod.rs:1017–1019`）。

## 4. 后端设计（Rust / stream-server）

### 4.1 模块改动清单

| 文件 | 改动 | 说明 |
| --- | --- | --- |
| `pipeline/capture.rs` | 补 `set_gdi(&mut self) -> bool` 转发 `capturer.set_gdi()`，同步刷新 `self.gdi`；`is_gdi()` 去除 `#[allow(dead_code)]` 暴露给循环与状态快照 | 既有 `open_primary()` / `frame()` / `dimensions()` / `display_name()` 不变 |
| `pipeline/mod.rs` | 新增 `DxgiRecoveryState`；新增 `PipelineEvent` 与事件回调挂载；`run_blocking_loop` 改为"内层采集节拍 + 外层恢复调度"双层结构；错误按 4.3 表分类；事件经回调上抛 | 线程边界不变：采集/编码主体仍在 `spawn_blocking`，事件只发枚举值，不携带非 Send 句柄 |
| `pipeline/stats.rs` | 本轮**不新增计数器**；`capture_err` 口径改为仅统计 GDI 阶段致命错误；DXGI 重建 / GDI 回退只记日志与事件 | 保持 #36 统计快照字段不变，`stream_stats` 契约不破坏 |
| `dispatch/output.rs` | 新增 `system(action: &str) -> String`；`StreamInfo` 允许由采集器回填 `capture` / `is_gdi` / `capture_reason`；新增 `stream_info` 复用构造路径供重建后刷新面板 | 既有 `pipeline/stream_info/stream_stats/connection` 构造器签名不变 |
| `signaling/websession.rs` | `arm_connected_notices` 向 `PipelineHandle` 注入事件回调：收到事件后经 `control.send_text(system(action))`；回退 / 重建成功后补发一次 `stream_info` 刷新面板捕获行 | 回调在 async 上下文持有 `control` 句柄，blocking worker 只调用 `Fn(PipelineEvent)` 闭包，不跨 await 传采集句柄 |
| `webrtc/channels.rs` | 不改；`send_control` 预检查 + `send_text` 既有能力满足事件下行 | — |

### 4.2 恢复状态机（对齐 3.2，裁剪到本轮需要）

```rust
/// Windows-only DXGI recovery counter, mirroring video_service.rs:77–120.
struct DxgiRecoveryState {
    attempts: u32,
    window_started: Option<Instant>,
    fallback_pending: bool,
}

const DXGI_RECOVERY_LIMIT: u32 = 3;
const DXGI_RECOVERY_WINDOW: Duration = Duration::from_secs(10);
const DXGI_RECOVERY_FRAME_GRACE: Duration = Duration::from_secs(2);
const TRY_GDI_LIMIT: u32 = 3;
const GDI_FATAL_LIMIT: u64 = 3;
```

状态迁移规则（与 RustDesk 逐字对齐语义）：

1. `next_attempt() -> Option<u32>`：
   - `window_started` 为空或已超 `DXGI_RECOVERY_WINDOW` → 清零 `attempts`、置新窗口起点；
   - `attempts >= LIMIT` → `fallback_pending = true`，返回 `None`；
   - 否则 `attempts += 1`，返回 `Some(attempts)`；
2. 重建采集器成功后，进入 2 秒 grace：grace 内 `WouldBlock` 不计入 `try_gdi`；
3. `fallback_pending` 被外层重建循环消费一次后复位；
4. `try_gdi`：新帧到达清零；`WouldBlock` 在 grace 外累加；超过 `TRY_GDI_LIMIT` 切 GDI 并清零；
5. GDI 阶段连续错误达 `GDI_FATAL_LIMIT` → 作为致命错误返回外层终止任务（前端可见 `capture_lost`）。

### 4.3 错误分类决策表（本轮实现口径）

| `source.frame(spf)` 返回 | 当前后端 | 本轮处理 | 事件 |
| --- | --- | --- | --- |
| `Ok(Frame)` 且 `frame.valid()` | 走编码；清零 repeat / capture_err | 不变；首帧成功后发 `capture_restored`，清零 `try_gdi` / GDI 错误计数；刷新 `stream_info` 捕获后端 | `Restored`（仅从异常态恢复后发） |
| `Ok(Frame)` 但 `!valid()` | 记 `invalid`，跳拍 | 不变，不触发恢复 | 无 |
| `Err(WouldBlock)` | repeat 帧补 Sample | grace 外 `try_gdi += 1`；`try_gdi > 3` 且非 GDI → `set_gdi()`、清零 `try_gdi`、发 `FallbackGdi("no frame three times")`；其余沿用 #36 repeat 路径 | `FallbackGdi`（仅切换瞬间发一次） |
| `Err(ConnectionReset)` 且非 GDI | 计入 capture_err，3 次终止 | `next_attempt()=Some(n)` → 置 `restart_pending`，break 内层节拍交外层重建；`next_attempt()=None` → `set_gdi()` 永久回退，`continue`；发 `RecoveryStart(n)` / `FallbackGdi("dxgi recovery exhausted")` | `RecoveryStart(n)`、`FallbackGdi(reason)` |
| `Err(InvalidData)` 且非 GDI | 同上 | 归入 DXGI 访问失效族，与 ConnectionReset 同路径处理（scrap 在访问丢失后持续返回 INVALID_CALL） | 同上 |
| `Err(Other/PermissionDenied/ConnectionRefused/Interrupted/ConnectionAborted/InvalidInput)` 且非 GDI | 同上 | 先按 ConnectionReset 族处理一轮恢复；若重建后仍非 GDI 环境报同类错误，落入 GDI 致命计数 | `RecoveryStart(n)` / `FallbackGdi` |
| 已在 GDI 后端，任意 `Err` | 终止 | `gdi_err += 1`；`<3` 记 warn 后 `continue`；`>=3` 致命返回，发 `Lost(reason)` 后终止 | `Lost(reason)` |
| 重建 `CaptureSource::open_primary()` 返回 `Err` | — | 外层退避 sleep（250ms 起步，封顶 2s）后重试；达到 `DXGI_RECOVERY_LIMIT` 次仍失败 → 致命 `Lost(reason)` 终止 | `Lost(reason)` |
| 分辨率与重建前不一致 | — | 本轮不做编码器分辨率热重建（阶段 3 桌面变更检测事项）；仅记 warn 日志，编码器沿用旧尺寸；若帧尺寸与编码器不符导致编码错误，归入编码器错误计数，不放大为采集致命错误 | 无（日志 warn） |

### 4.4 双层循环结构（线程边界保持 #36 不变）

```
spawn_blocking:
└─ outer_recovery_schedule(cfg, tx, handle, event_cb)        // 外层：可重建 / 可退避
   ├─ loop until stopped
   │   ├─ source = CaptureSource::open_primary()?          // 打开失败 → 退避重试 / 致命 Lost
   │   ├─ encoder = VideoEncoder::new(src_w, src_h, bitrate)?
   │   ├─ 消费 fallback_pending：置 GDI，发 FallbackGdi
   │   ├─ enter_inner_beat(source, encoder, ...)          // 内层节拍循环
   │   │  ├─ Ok(valid frame) → 编码 → forward_chunks
   │   │  ├─ WouldBlock → repeat / try_gdi 计数 / 切换 GDI
   │   │  ├─ recoverable DXGI error → 发事件 → break 内层（回到外层重建）
   │   │  ├─ GDI fatal / 不可恢复 → 返回 Err(InnerFatal) 给外层终止
   │   │  └─ gate closed → sleep spf continue
   │   └─ 内层 break = 一次重建事件；外层重新打开采集器 / 编码器
   └─ fatal → 发 Lost(原始错误摘要)，返回 Err 终止 worker
```

- 外层退避：首次 250ms，连续失败倍增至 2s 封顶；`handle.stopped` 每轮开头检查，会话清理立即退出；
- 重建期间 `tx` 通道不发 Sample；前端看门狗在重建 10s 窗口内不会触发 PC 重连（重建节拍远快于 3s 看门狗？
  注：看门狗按"播放时钟未走"3s 重发 `START_VIDEO`，重建事件在 250ms–2s 内完成，首帧恢复后时钟恢复走动；
  若单次重建失败退避超过 3s，看门狗重发命令无害——`start()` 幂等置闸门 true，已在 #35 闭环）；
- 编码器重建：分辨率不变时本可复用；本轮按 RustDesk 做法随采集器一并重建，结构最简单，代价是重建瞬间
  VP8 编码器初始化几十毫秒，纳入恢复退避窗口。

### 4.5 事件通道与下行消息契约

**后端枚举**（`pipeline/mod.rs`，纯值类型，跨线程安全）：

```rust
pub enum PipelineEvent {
    /// 第 n 次 DXGI 重建尝试（1..=LIMIT），消息体 `capture_recovery,<n>`。
    RecoveryStart(u32),
    /// 永久回退到 GDI，reason 为短英文原因，消息体 `capture_fallback_gdi,<reason>`。
    FallbackGdi(String),
    /// 不可恢复，reason 为错误摘要，消息体 `capture_lost,<reason>`；发完即终止任务。
    Lost(String),
    /// 异常态之后重新拿到有效帧，消息体 `capture_restored`；供前端清除提示条。
    Restored,
    /// 后端切换 / 重建后刷新面板捕获行，消息体为整条 `stream_info`（非 system action）。
    RefreshStreamInfo,
}
```

**PipelineHandle 挂载点**：`Arc<dyn Fn(PipelineEvent) + Send + Sync + 'static>`，`new()` 内为 no-op
默认闭包；新增 `set_event_sink(Arc<dyn Fn(...)>)` 方法，会话装配时替换。worker 内事件触发即调用，
不经过 mpsc 通道（避免再引入一个跨线程消费者；回调内 `control.send_text(...).await` 由 async 侧闭包
`async move` 包装，blocking 侧只持有 `Arc<dyn Fn(PipelineEvent) + Send + Sync>` 同步句柄——实现期
若 Sync 闭包无法包 async 发送，退化为 `mpsc::UnboundedSender<PipelineEvent>` + 会话侧消费任务，
见 4.5.2 退化路径）。

#### 4.5.1 `system` 下行消息（复用既有路由，前端 `system.action` 族）

JSON 结构沿用既有 `system` 契约（`dispatch/output.rs` 新增构造器）：

```json
{"type":"system","data":{"action":"capture_recovery,1"}}
{"type":"system","data":{"action":"capture_fallback_gdi,dxgi_recovery_exhausted"}}
{"type":"system","data":{"action":"capture_fallback_gdi,no_frame_three_times"}}
{"type":"system","data":{"action":"capture_lost,invalid data after gdi fallback"}}
{"type":"system","data":{"action":"capture_restored"}}
```

`reason` 取值白名单（避免把原始 `Debug` 错误串直接推前端，保持前端文案稳定）：

| reason 串 | 触发条件 |
| --- | --- |
| `dxgi_recovery_exhausted` | 10s 窗口内 3 次重建失败，永久回退 GDI |
| `no_frame_three_times` | WouldBlock 连续超 `TRY_GDI_LIMIT`，切 GDI |
| `open_failed` | 外层 `open_primary()` 连续失败后致命终止 |
| `gdi_fatal` | GDI 阶段连续 3 次采集错误后致命终止 |
| 其他 | 仅允许 64 字符以内的英文错误摘要，逗号转义为 `;` |

#### 4.5.2 退化路径（实现期编译受阻时启用）

若同步 `Fn` 闭包内拿不到 `control` 的 async 发送能力：`PipelineHandle` 内挂
`Arc<Mutex<Option<mpsc::UnboundedSender<PipelineEvent>>>>`，worker `try_send`（满了静默丢弃，
不阻塞节拍）；会话侧新增一个常驻消费任务 `while let Some(ev)=rx.recv().await`，按 4.5.1 表调
`control.send_text(system(...))`，`RefreshStreamInfo` 分支组 `stream_info` JSON 发送。两条路径前端
契约完全相同。

#### 4.5.3 `stream_info` 刷新

回退 GDI / 重建成功后发一条完整 `stream_info`（复用既有 `dispatch/output.rs:83–107` 构造器），
`capture` 取采集器后端名（`dxgi` / `gdi`）、`capture_reason` 记切换原因短串；前端面板"捕获"行据此
刷新后端标签，不必等 1s 周期 stats 轮询。

### 4.6 StreamInfo 回填（解决 #36 遗留占位）

`dispatch/output.rs` 现状 `StreamInfo::default()` 固定 `dxgi=false` 占位；本轮由采集器真实状态构造：

| 字段 | 取值 |
| --- | --- |
| `capture` | `crate::platform::capture_backend_name(source.is_gdi())`（`platform/host.rs` 既有：gdi→`gdi`，否则 `dxgi`） |
| `is_gdi` | `source.is_gdi()` |
| `capture_reason` | 启动为空；GDI 回退后填 4.5.1 的 reason 短串；重建恢复后清空 |
| 其余字段 | 沿用既有固定值（encoder=libvpx、codec=VP8、硬件=false 等），本轮不改 |

## 5. 前端设计（selkies-webclient）

### 5.1 改动落点表

| 文件 | 改动 | 既有不动项 |
| --- | --- | --- |
| `lib/webrtc.js` | 不改下行路由：`system` 已在 `:767–774` 派发到 `onsystemaction`；`stream_info` 已在 `:738–741` 派发到 `onstreaminfo` | `_gz,0` 静默、HEAD 405 两条遗留噪声仍归阶段 3 |
| `selkies-wr-core.js` | 新增 `captureNotice` 模块级状态；`onsystemaction` 新增 4.5.1 五个 action 分支；`updateStatusDisplay()` 增加采集提示优先级；`onstreaminfo` 已有 `streamStats.setInfo(info)` 链路，回退后的刷新自动进面板 | 既有 `video_declined`、`rumble`、`pointer`、`command_error/done`、`apps_installed`、`auth_success` 等分支顺序不变；`:2069` 用户手势音频切换、`:3679` 默认静音不动 |
| `lib/stream-stats-view.js` | 不改：`captureRow` 读取 `info.capture` / `info.capture_reason` 的既有路径已满足后端标签显示 | 面板布局、编码/解码行逻辑不动 |

### 5.2 状态标志与状态栏优先级

新增模块级变量（与 `videoDeclined`、`status` 同级，`:1012` 附近）：

```javascript
// null = 无采集提示；string = 非致命临时提示；致命失败走 videoDeclined。
let captureNotice = null;
// 采集后端是否已降级 GDI（面板 / 日志区分）；不影响状态栏显示。
let captureFallbackGdi = false;
```

`updateStatusDisplay()`（`:1019–1029`）显示优先级，从上到下：

1. `videoDeclined === true`：显示 `captureNotice`（致命错误文案），强制可见，不被 connected 覆盖——沿用 `:1020` 既有 `!videoDeclined` 门控语义；
2. `captureNotice !== null`（非致命重建 / 回退提示）：显示提示文案，`remove('hidden')`；即使 `status==='connected'` 也不隐藏；
3. 其余：走原逻辑——`status==='connected'` 隐藏状态栏，否则显示 `status` 首字母大写文案；
4. `capture_restored` 到达：`captureNotice=null`、`captureFallbackGdi=false`，调用 `updateStatusDisplay()`；下一帧节拍 connected 状态下状态栏自动隐藏。

### 5.3 action 分支与文案表

`onsystemaction`（`:3519` 起，在 `video_declined` 分支之后、`rumble/pointer` 同级位置插入）：

| action | 前端动作 | 状态栏文案（英文，对齐既有 `Connecting...` / `Connected to server` 体系） | 严重级 |
| --- | --- | --- | --- |
| `capture_recovery,<n>` | `captureNotice = 'Rebuilding screen capture (attempt ' + n + ' of 3)...'`；`updateStatusDisplay()`；console.warn 记录 | Rebuilding screen capture (attempt n of 3)… | 提示（橙） |
| `capture_fallback_gdi,<reason>` | `captureFallbackGdi=true`；`captureNotice = reason==='no_frame_three_times' ? 'No new frames; switched to software capture mode' : 'Capture recovery failed; switched to software capture mode (frame rate may drop)'`；`updateStatusDisplay()`；console.warn | Software mode hint | 警告（黄） |
| `capture_lost,<reason>` | `videoDeclined=true`；`captureNotice = 'Screen capture failed: ' + reason`；`statusDisplayElement` 移除 hidden；console.error | Screen capture failed: … | 致命（红） |
| `capture_restored` | 清 `captureNotice` / `captureFallbackGdi`；`updateStatusDisplay()`；console.info `capture restored` | 无（隐藏提示条） | 恢复（绿，仅日志） |
| 其他未知前缀 | 不吞错：落到既有 `webrtc._setStatus("Executing system action: " + action)` 日志路径，保持 `:3539` 兜底行为 | — | — |

CSS：`.status-bar` 既有样式（`:208` 附近）只支持默认色；本轮不新增独立颜色类——提示条文字保持既有状态栏配色，通过文案与日志级别区分严重度；颜色分级留阶段 3 工具栏对接时统一做。

### 5.4 面板捕获行联动

- `capture_fallback_gdi` / `capture_restored` 后端事件触发的同时，服务端补发 `stream_info`（4.5.3）；
- `onstreaminfo` → `streamStats.setInfo(info)` 既有链路自动刷新"捕获"行：DXGI 常态 `Readback (dxgi)`，
  回退后 `Readback (gdi)`；`capture_reason` 在 Windows 分支不展开为 reason 文案（`stream-stats-view.js:112`
  仅 readbackToGpu 时取 reason），本轮不改造视图条件。

## 6. 阶段边界与不做项（#42-1 明确排除）

| 事项 | 归属 | 理由 |
| --- | --- | --- |
| 桌面分辨率 / 刷新率变更检测与采集器-编码器联动重建 | 阶段 3 | 本轮固定单屏 1714×922；分辨率热变更触发的编码器重建与尺寸协商在阶段 3 QoS 轮次统一处理 |
| 静态刷新（静态帧复用末帧、周期性关键帧） | 阶段 3 | #36 既有边界；本轮 repeat 帧仅服务 WouldBlock 静默期 |
| WouldBlock grace 计时联动前端提示 | 阶段 3 | `DXGI_RECOVERY_FRAME_GRACE` 只用于后端不重复计 try_gdi，前端不展示 grace 细节 |
| NACK / PLI / 丢包反馈循环 | 阶段 3 | webrtc-rs 发送侧 RTCP 缓冲读取未挂载（#34 遗留） |
| 输入注入、指针回显、工具栏命令对接 | 阶段 2 | 本轮媒体面独立验收 |
| 多显示器切换、隐私模式采集、录屏 / 摄像头 | 已裁剪 | 0e 阶段已移出 vendor |
| MC 服务端起服与进世界链路 | 阶段 4 | 142:25574 未监听属游戏链路事项 |

## 7. 编译、测试、部署与验收判据

### 7.1 编译与单元测试（本机 142）

- 构建命令沿用既有：`toolchain\build-ws.cmd build -p stream-server -j2`；
- 测试：`toolchain\build-ws.cmd test -p stream-server -j2`；#35 两条路由单测保持通过；
  本轮为状态机 / 错误分类补单元测试（纯函数，不碰 Win32 句柄）：
  1. `DxgiRecoveryState::next_attempt`：窗口内第 1/2/3 次返回 Some，第 4 次返回 None 并置 fallback；
     窗口超时后计数清零；
  2. 错误分类函数：ConnectionReset / InvalidData 归 recoverable；WouldBlock 走 repeat；GDI 错误归 fatal；
  3. `system()` / `stream_info()` 构造器输出 JSON 字段与前端契约一致；
- 编译期警告只允许既有 vendor 遗留项；stream-server crate 自身零 warning。

### 7.2 部署 143（SSH 免密通道，沿用 #37 包装脚本）

1. 本机 `cargo build` 产物 `target\debug\stream-server.exe` 经 scp 推 `C:\dev\stream\stream-server.exe`；
2. 前端 core → dashboard 双轮打包，`deploy-frontend.ps1` 清空 `C:\dev\selkies-web` 残留后解压新 dist；
3. `restart-stream.cmd`：`taskkill /F /IM stream-server.exe` → `schtasks /End MCStreamSrv` →
   `schtasks /Run MCStreamSrv` → 等待 5s → 校验 8080 LISTEN、日志尾部无致命错误；
4. 旧进程清理沿用远端 `taskkill /F /IM`（跨会话 Stop-Process 不可靠，已验证）。

### 7.3 验收动作与通过判据

**A. 单验串流出画面恢复（不启动 MC）**

- 浏览器 InPrivate 打开 `https://192.168.59.143:8080/`，被控桌面自动渲染、Play 按钮不出现；
- 面板编码器 `libvpx · VP8 4:2:0`、捕获 `Readback (dxgi)`、解码器 `<video> element` 三项非"不可用"；
- 控制台无 `Unhandled message received`；`capture_restored` 日志可被人工复现前暂不强制。

**B. MC 共存复测（#37-5 闭环）**

- `schtasks /Run MCCloudStart` 拉起 MC（`--fullscreen=true`，`-Xmx1G`）；
- 判定通过条件：
  1. 浏览器画面在 MC 全屏切换后**不挂起 Play 按钮**；若后端触发重建，状态栏出现
     `Rebuilding screen capture (attempt n of 3)...`，重建成功后提示消失、画面恢复；若回退 GDI，状态栏出现
     软件模式提示，捕获行变为 `Readback (gdi)`，画面持续可见；
  2. 连续观察 ≥60s：无 `pipeline loop ended: capturer failed...` 致命日志；浏览器 FPS 与 0e 基线同量级
     （GDI 回退后允许帧率下降，不要求追平 DXGI 常态）；
  3. 资源：java 工作集在 `-Xmx1G` 内；stream-server 稳态工作集不超过 120MB；主机空闲内存 ≥2GB；
  4. 日志链可观测：`capture source opened`、`capturer error #n`、`dxgi recovery restart attempt n`、
     `fall back to gdi`、`capture restored` 各行时间戳完整。

**C. 致命失败可见性（构造一次可观察场景，允许人工触发后还原）**

- 后端主动下发 `capture_lost,<reason>` 路径走通：前端状态栏显示红色/可见失败文案，
  不出现"后端报错、前端黑屏无提示"的沉默状态；
- 服务端日志与前端文案 reason 串一一对应（4.5.1 白名单）。

### 7.4 复测记录要求

- 采集三轮 12s 间隔资源快照（沿用 `mc-sample.ps1` 口径：java / stream-server 工作集、空闲内存、
  双逻辑槽 CPU 累计秒）；
- 留存浏览器控制台截图与 `stream-server.log` 尾部 80 行到 `toolchain\deploy143\` 对应输出文件，
  补写 #42-1 装配记录（对齐 `stage1-pipeline-loop.md` 体例）。

## 8. 风险与回退

| 风险 | 触发条件 | 缓解 / 回退 |
| --- | --- | --- |
| 重建采集器时窗口句柄 / DXGI 对象释放时序导致进程崩溃 | 重建旧 `Capturer` 与 `Capturer::new` 之间 | 沿用 RustDesk 整对象丢弃重建方式：旧对象随外层迭代 dropped，不在节拍中途释放；首轮部署观察日志无 `access violation`；异常崩溃即回退到方案 A 首次失效 set_gdi（不做重建轮次） |
| 2s grace 内 WouldBlock 被误判为静态帧 | 重建后首帧延迟 >2s | grace 计数只作用于 `try_gdi`，不影响 repeat 帧路径；阶段 3 再依据实测抖动调整 grace |
| GDI 采集帧率低于浏览器看门狗容忍度 | VM 软件刷墙 + 动态画面 | 回退 GDI 后若 3s 内仍无有效帧，看门狗重连属既有自愈链路；本轮允许 GDI 低帧率，性能优化归阶段 3 |
| 事件回调阻塞采集节拍 | 前端 DC 背压 / 发送慢 | `try_send` 非阻塞，满则丢弃事件只留日志；事件非帧数据，丢失不影响画面 |
| 分辨率热变更后编码器尺寸不符 | 本轮未处理 | 仅 warn 日志记录尺寸漂移；若实测触发编码异常高频，阶段 3 优先补编码器重建 |
| 前端 bundle 缓存旧版本 | 部署后浏览器加载旧 JS | `deploy-frontend.ps1` 先清空旧 hash 文件；验收用 InPrivate 新会话；`entryPageTag` 405 噪声不影响 |

## 9. 装配顺序（对应任务编号）

1. `#43-1` 后端：`capture.rs` set_gdi 暴露 + `mod.rs` 状态机 / 双层循环 / 事件枚举 + `output.rs` system 构造器；
2. `#43-2` 后端：`websession.rs` 事件回调挂载 + 重建后 stream_info 补发 + 单元测试；
3. `#43-3` 前端：`onsystemaction` 分支 + `captureNotice` 状态 + `updateStatusDisplay` 优先级；
4. `#43-4` 本机构建 / 测试 → 打包 exe + 前端 dist → 部署 143；
5. `#43-5` 单验出画面 → MC 共存复测 → 资源快照 → 补写装配记录，闭环 #42。
