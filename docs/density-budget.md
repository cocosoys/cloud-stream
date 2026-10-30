# 密度预算：一台宿主能跑几个云游戏实例（#97 待办，未实施）

> 状态：**只记录分析与结论，没有动任何代码**。这份文档是为了将来"要往低配服务器上
> 压密度"时不必重新测一遍。
>
> 采集时间：2026-10-30 下午，被控机 `192.168.59.148`（Win11 build 26200 / 4 vCPU /
> 8 GB / VMware SVGA 3D），当时 1 个控制器连着、桌面基本静止、60 fps / VP8 / 1080p。

---

## 0 结论

**这套架构里内存不是天花板，CPU 是。** 按 16 vCPU / 64 GB 宿主算，内存能给 17 台，
CPU 只能给 11 台；而且 CPU 超卖的代价（掉帧、编码排队）比内存超卖（气球回收、变慢）
更致命。所以"把内存从 4 GB 压到 1 GB"对密度**没有帮助**。

三个必须先接受的事实：

1. **1 台 Windows = 1 个实例**。DXGI Desktop Duplication 抓的是当前会话的桌面，而
   客户端版 Windows 只有一个 console 会话；RDP 会话里这个 API 不可用。想在一个 OS 里
   跑 N 个实例，必须改多会话/RDS，那是另一套东西。
2. **分配 4 GB ≠ 成本 4 GB**。VMware 气球只算 touched 页。要压的是 touched，不是分配值。
3. **没有 GPU 的宿主，1 个 1080p30 实例 ≈ 1 个物理核**，这是 libvpx 软件编码的物理
   成本，只能靠降分辨率/帧率绕，不能用"优化代码"消除。

---

## 1 实测（这是整篇文档的地基）

| 观测项 | 值 | 来源 |
|---|---|---|
| 客户机整体 | Win11 26200，commit **3.47 GB** / 分配 8 GB，104 个服务在跑 | `Win32_OperatingSystem` + `\Memory\Committed Bytes` |
| 整机 CPU | 4 vCPU 的 **27–34%** ≈ 1.1–1.36 核 | `/api/v1/health` → `host.cpu_percent` |
| `stream-server` 自身 | WS **18 MB**、private **160 MB**、23 线程 | `Get-Process` |
| 推流进程 CPU（静态桌面+1 观众） | 20 s 增量实测 **0.49 核**；忙时 `process_cpu_percent` 到 **82.7** | `Get-Process` / health |
| 每拍编码成本 | `enc_avg` **10.4 ms**（重复帧）/ **13–14 ms**（有变化的帧） | 5 s 统计行 |
| 前五大内存进程 | explorer 345 MB、MsMpEng(Defender) 229 MB、dwm 144 MB、StartMenu 134 MB、Edge 栈 ≈ 300 MB | `Get-Process` |

**关键反差**：那 3.47 GB 里，推流进程只占 0.16 GB。所以"系统要 1 GB 才能启动"其实是
"这个**完整镜像**带着 Defender + Edge + Widgets 要 3.4 GB"。开机瞬间确实 ~1 GB，
shell 加载完才爬到 3.4 GB。

---

## 2 每实例成本拆成四层

| 层 | 现在 | 可压到 | 谁决定 |
|---|---|---|---|
| 客户机 OS（Win11 全量 + Defender + Edge） | 2.9 GB / 0.87 核 | 0.7 GB / 0.30 核 | **镜像**，与代码无关 |
| 推流进程常驻（含缓冲） | 160 MB | 160 MB | `pipeline` 的帧缓冲：`pristine_bgra` + `cursor_buf` + `yuv` + `last_yuv` 各一份 1080p 量级 |
| libvpx 线程 | `g_threads = min(nproc, 4)`，4 vCPU 的 VM 开 4 线程，3 个 worker 各烧 ~1290 s | 1–2 线程 | `vendor/scrap/src/common/vpxcodec.rs:120` |
| 宿主层 | VMware 每 VM 100–300 MB + 显存 | 固定成本 | 虚拟化平台 |

已经做对的部分（不要再动）：`deadline = VPX_DL_REALTIME`、`VP8E_SET_CPUUSED = 7`
（VP9 走 12）、`rc_dropframe_thresh = 0`（云游戏不能丢 P 帧）。

---

## 3 杠杆，按性价比排

### L1 换镜像（最划算，零代码）

Win10 LTSC 2021 / IoT，删 Defender、关 Widgets/Search/OneDrive/Edge 自启、关
VBS/Core Isolation。每实例 **−2.2 GB、−0.5 核**。
注意：第三方精简版（X-Lite 之类）如果没删 Defender，白搭——Defender 单独就 229 MB
WS + 284 MB private，还白烧 CPU。

### L2 降编码规格

720p + 20 fps + `g_threads` 压到 1–2 + `repeat_every` 调大。像素降 56%、帧率降 33%，
编码成本约 ×0.3。代价是清晰度和鼠标跟手度。
坑：`--width/--height` 只省**编码**那一段；DXGI 抓屏和无变化帧仍按配置帧率走，
抓屏 + 缩放的成本还在。

### L3 空闲降频（要改代码）

静态桌面不该跑满 beat。用 `DXGI_OUTDUPL_FRAME_INFO.AccumulatedFrames` 或 dirty rect
判定"没变化"，把 beat 从 60 Hz 降到 5 Hz。**收益取决于空闲率**——云游戏机器只要有人
在玩就是满载，这条只对"挂着没人玩"有效。

### L4 硬件编码

宿主有 GPU 就 NVENC/QSV，每实例从 0.5 核掉到 0.05 核。但 VMware SVGA 3D 不给这个，
要 GPU passthrough 或 vGPU，成本比省下来的 CPU 贵。这条路和"低端服务器"是矛盾的。

### L5 换轨（数量级，要重写）

Linux + KVM + Wine/Proton + Xvfb + VA-API，每实例 0.3–0.5 GB、0.1 核。
代价是把采集（DXGI）、音频（WASAPI）、输入（`SendInput`）三个模块全部重写——
这套代码的 Windows 依赖正好集中在这三处。

---

## 4 密度阶梯（16 vCPU / 64 GB 宿主，留 4 GB 给宿主）

| 配置 | 每实例 CPU | 每实例内存 | CPU 允许 | 内存允许 | **实际** |
|---|---|---|---|---|---|
| 现状：Win11 全量 / 1080p30 | 1.36 | 3.4 GB | 11 | 17 | **11** |
| 精简镜像（LTSC，去 Defender） | 0.84 | 1.1 GB | 19 | 54 | **19** |
| + 720p20 / 编码 1 线程 | 0.55 | 1.1 GB | 29 | 54 | **29** |
| + 空闲降频（仅静态桌面有效） | 0.40 | 1.1 GB | 40 | 54 | **40** |
| + 硬件编码（需 GPU 透传） | 0.15 | 0.55 GB | 106 | 109 | **106** |

前两档全是 CPU 先撞墙，第三档之后内存才开始成为瓶颈。

---

## 5 还没做、但登记在案的两件事

- [ ] 关掉 Defender + Edge，量稳态 commit 掉到多少（决定 L1 值多少）
- [ ] `--width 1280 --height 720 --fps 20` 起一个实例，量 `process_cpu_percent`（决定 L2 值多少）
- [ ] 若决定做 L3：`pipeline/mod.rs` 的 beat 需要 dirty-rect 判定 + 空闲降频，改动面在
      主循环，回归风险不小，要配 servertest
- [ ] L5 只在"要上百实例"时才值得，属于换产品而不是改配置

两个实验加起来十分钟，拿到数字再决定要不要动代码。
