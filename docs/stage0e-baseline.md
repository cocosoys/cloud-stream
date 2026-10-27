# 0e 采集 + VP8 编码基线记录（本机 SERVER001 / 被控 CLIENT001）

> 路线 A：以 RustDesk 后端源码为基底。本文件记录阶段 0 两端 DXGI 采集 → libyuv RGB→YUV →
> VP8 软编码的帧级与 CPU 基线。**全部为 debug 未优化构建**（`[profile.dev] codegen-units=2`，
> `opt-level=0`），数值用于验证链路连通性与量级，不代表最终 release 性能口径。

## 1. 口径声明（先读）

- 两组基线参数一致：1714×922、VP8、`quality=1.0`、目标码率 1646 kbps、`keyframe_interval=None`、20 s @ 30 fps、spf=33.33 ms。
- **画面内容不同，`fps(new)` 不可直接横比**：本机桌面持续有动态（Doubao / 后台 java 渲染），被控桌面静态为主（MC 在后台静止）。可比的是：编码耗时、YUV 转换耗时、CPU 量级。
- bench 第二版对齐主仓 `video_service.rs:933-955`：WouldBlock 时在 `repeat_encode_max=10` 内复用缓冲里的上一帧 YUV 重复编码以维持帧率；拿到新帧后 counter 清零。`fps(total) = (new + repeat) / wall`。
- 未复制主仓静态刷新、DXGI 三次恢复回退 GDI、QoS、会话发送逻辑。

## 2. 运行环境（采样时刻实测）

| 项 | 本机 SERVER001（主控 142） | 被控 CLIENT001（143） |
| --- | --- | --- |
| 系统 | Win11 Pro 26200 | Win11 Pro 26200，VMware SVGA 3D |
| CPU | 4 vCPU | 4 vCPU |
| 内存 | 8191 MB（采样时刻空闲约 2.7 GB） | 8191 MB，空闲 4945 MB |
| 显示器 | `\\.\DISPLAY1` 1714×922，primary | 会话 1 真实桌面 1714×922，primary |
| 采集后端 | DXGI（`gdi=false`） | DXGI（`gdi=false`） |
| 采样通道 | 本机直接运行 | 交互计划任务 `BenchCapture`（`/it`，Run As Administrator），在 Console 1 会话运行 |
| 背景进程 | java PID 13164 常驻（约 269 MB，未终止） | MC java PID 8620 保留；旧 python PID 7532 已清场，旧任务 `MCStreamSrv` 已禁用 |

> 注：SSH 落在会话 0（services），读到的 1024×768 是会话 0 虚拟分辨率假象；真实尺寸以交互会话内 bench 自报的 1714×922 为准。

## 3. bench 版本演进

| 版本 | 行为 | 用途 |
| --- | --- | --- |
| v1（0e-3 第一版） | 只测新帧采集 + 编码；WouldBlock 直接跳过，不复用旧帧 | 链路连通性首验 |
| v2（repeat 版） | WouldBlock 时复用上一帧 YUV 重复编码，上限 10 次；新帧/重复帧分组计时与计数 | 与主仓运行范式对齐，测量维持帧率与编码吞吐 |

## 4. 本机三轮原始数据（SERVER001）

### 4.1 r1 — v1 无 repeat（0e-3 首份基线，存档）

原始输出 `toolchain\bench-local-20s-30fps.txt`：

```
wall_s=20.033  frames_ok=474  would_block=1  encoded_ok=474
measured_fps=23.66
capture  avg=27.182 ms  min=12.078  max=106.881  (n=474)
convert  avg= 1.705 ms  min= 0.585  max= 15.755  (n=474)
encode   avg=10.996 ms  min= 5.332  max= 71.747  (n=474)
CPU: wall_s=20.663 cpu_s=5.328 cores=4 4核均分=6.4% 单核≈25.6%
```

### 4.2 r2 — v2 repeat 版首跑（旧包装脚本 CPU 句柄失效，帧指标有效，CPU 以样本末值为准）

```
wall_s=20.030  new=477  would_block=0  repeat_ok=0  fps(new)=23.81  fps(total)=23.81
capture  avg=27.003 ms  min=7.466  max=88.058
convert  avg= 1.237 ms  min=0.511  max=31.000
encode(new) avg= 9.872 ms  min=5.363  max=49.221
CPU 样本末值 cpu_s=4.828 @ t=20.44s（汇总行退出后读句柄失效为 0，已在 r3 修复）
```

### 4.3 r3 — v2 repeat 版，修复包装脚本（同公式，作为本机对照口径）

```
wall_s=20.033  new=468  would_block=1  repeat_ok=0  repeat_skipped=1
fps(new)=23.36  fps(total)=23.36
capture  avg=27.721 ms  min=7.927  max=85.560  (n=468)
convert  avg= 1.127 ms  min=0.508  max=16.960  (n=468)
encode(new) avg=10.106 ms  min=5.330  max=47.503  (n=468)
CPU: wall_s=20.299 cpu_s=4.875 cores=4 4核均分=6.0% 单核=24.0%
```

本机三轮 fps：23.66 / 23.81 / 23.36，稳定在 23–24 fps，可复现。

## 5. 被控两轮原始数据（CLIENT001）

### 5.1 r1 — v2 repeat 版首跑（旧 ps1 CPU 句柄失效；静态桌面无脏帧，帧率不可比）

```
wall_s=20.041  new=37  would_block=356  encoded_ok=37  fps(new)=1.85
capture  avg=46.938 ms  min=6.867  max=291.821  (n=37)
convert  avg= 1.217 ms  min=0.585  max=15.079  (n=37)
encode   avg=10.641 ms  min=6.115  max=28.224  (n=37)
CPU 采样失效 cpu_s=0（句柄跨令牌读取为 0，已在 r2 修复）
```

### 5.2 r2 — v2 repeat 版，修复 ps1（被控对照口径）

原始输出 `C:\dev\bench\bench-result.txt`，汇总 `bench-cpu.txt`：

```
time_start=19:16:19.105  time_end=19:16:39.806  cores=4  mem_free=4945 MB
display: 1714x922 primary=True
wall_s=20.023  new=53  would_block=342  repeat_ok=329  repeat_no_output=0  repeat_err=0  repeat_skipped=13
fps(new)=2.65  fps(total)=19.08
capture(new) avg=51.749 ms  min=4.164  max=250.905  (n=53)
convert(new) avg= 1.103 ms  min=0.076  max=14.578  (n=53)
encode(new)  avg= 8.429 ms  min=4.948  max=28.078  (n=53)
encode(repeat) avg=9.840 ms  min=5.500  max=14.485  (n=329)
CPU: wall_s=20.519 cpu_s=3.844 cores=4 4核均分=4.7% 单核=18.7%
exit_code=unknown（见第 9 节已知问题）
```

## 6. 同口径对照表（本机 r3 ↔ 被控 r2，均 v2 repeat 版）

| 指标 | 本机 SERVER001 r3 | 被控 CLIENT001 r2 | 解读 |
| --- | --- | --- | --- |
| wall_s | 20.033 | 20.023 | 一致 |
| new 帧 | 468 | 53 | 本机桌面持续动态，被控静态 |
| WouldBlock | 1 | 342 | 被控脏帧稀疏 |
| repeat_ok | 0 | 329 | 被控靠旧帧重复编码维持帧率 |
| repeat_skipped | 1 | 13 | 达 repeat_max=10 上限后跳过的轮次 |
| fps(new) | 23.36 | 2.65 | 画面内容不同，**不可横比** |
| fps(total) | 23.36 | 19.08 | 维持帧率口径 |
| capture avg / max (ms) | 27.72 / 85.56 | 51.75 / 250.91 | **VM 端采集明显更弱**，第一瓶颈 |
| convert avg (ms) | 1.127 | 1.103 | libyuv 路径两端都很轻 |
| encode(new) avg (ms) | 10.106 | 8.429 | 编码同量级 |
| encode(repeat) avg (ms) | — | 9.840 | 重复编码与新帧编码耗时相当 |
| cpu_s | 4.875 | 3.844 | — |
| CPU 4 核均分 | 6.0% | 4.7% | 同量级 |
| CPU 单核 | 24.0% | 18.7% | 同量级 |

## 7. 结论与瓶颈定位

### 7.1 链路连通性（已验证）

- 两端均在 Console 1 交互会话枚举到真实桌面 1714×922，DXGI 采集器、VP8 编码器创建成功；全部新帧与重复帧编码成功，0 采集错误、0 编码错误、0 VpxNoOutput。
- 静态链接的 libvpx 1.15.2 / libyuv 1974（vcpkg `x64-windows-static`）在被控 VM 上直接运行：唯一非系统依赖 `VCRUNTIME140.dll` 已确认就位；exe 体积 4.13 MB，可由主控编译后 scp 部署。

### 7.2 瓶颈排序

1. **采集是第一瓶颈，VM 端更弱**：本机 27.7 ms、被控 51.75 ms（max 250.9 ms）。被控为 VMware SVGA 脏帧复制开销；被控静态桌面还叠加新脏帧稀疏。
2. **YUV 转换不是瓶颈**：两端均约 1.1 ms。
3. **VP8 软编码不是瓶颈**：新帧 8.43–10.11 ms、重复帧 9.84 ms，两端同量级；1714×922、1646 kbps 量级内可控。
4. 被控 `fps(total)=19.08` 未达 30：采集新帧慢 + repeat 上限 10 + repeat 编码本身约 9.84 ms 叠加，循环被 spf 33.33 ms 与处理耗时共同限制。

### 7.3 CPU 量级（对照旧 Python 链路）

| 链路 | 进程 CPU 口径 | 备注 |
| --- | --- | --- |
| 旧 Python / aiortc | 被控端经常 70% 以上，不足 30% 留给 Minecraft | 用户实测判定不可接受，已弃用 |
| 本 Rust 采集+编码（debug） | 本机 4 核 6.0% / 单核 24.0%；被控 4 核 4.7% / 单核 18.7% | 仅采集+编码进程，不含游戏本身 |

量级改善显著；当前为 **debug 构建 + 静态/低动态画面**，不作为对外性能承诺。最终性能口径以 release 构建、MC 动态画面基线为准。

## 8. 0e-4 部署事实（供阶段 1 复用）

- SSH 免密：`Administrator@192.168.59.143`，密钥 `C:\Users\user\.ssh\id_ed25519_mc`；远程多条命令必须写成 bat/ps1 经 scp 上传执行，本地 pwsh 会拦截 `&`、`||`、重定向与内嵌双引号。
- 被控任务：`BenchCapture`（`/it` 交互，Run As Administrator，`C:\dev\bench\run-bench.cmd`）；旧 `MCStreamSrv` 已 `/change /disable`，防止到点拉起旧 python。
- 被控清场：python PID 7532 已终止（`schtasks /end` 成功，`taskkill /PID /F` 报成功后进程短暂残留，二次复核已退出）；MC java PID 8620 本轮保留未动。
- 依赖核对（dumpbin /dependents）：系统 DLL（kernel32/USER32/gdi32/dxgi/d3d11/pdh/advapi32/ntdll/oleaut32/shell32/psapi/powrprof/bcrypt）+ `VCRUNTIME140.dll` + UCRT api-ms-win-crt-*；无 msvcp140、无外部 vcpkg DLL。

## 9. 未控变量与已知问题

1. **debug 未优化**：`opt-level=0`，Rust 胶水与采集/编码偏慢；release 基线待补。
2. **静态画面口径**：被控桌面静态为主；MC 动态场景下新脏帧增多、码率与编码耗时会上升，动态基线在阶段 1 出画面后补测。
3. **未统计编码输出字节与实际码率**：阶段 1 桥接层补录。
4. **未做 DXGI ConnectionReset 三次恢复与 GDI 回退**（主仓 969–997 行）；0e 只测新帧采集 + repeat 编码。
5. **包装脚本退出码读取为 `unknown`**：`Start-Process` 返回对象在交互令牌上下文取不到 `ExitCode`；帧指标与 CPU 不受影响，阶段 1 改为 bench 进程自写退出码文件后再核。
6. 本机背景 java PID 13164 与 Doubao/WorkBuddy 渲染占用 CPU，本机 max 尖峰含其影响。

## 10. 下一步

- 0e 收尾：将 0e-2 新增裁剪口径（record/camera/webm/nokhwa 移除、主仓 Cargo.lock 复用、codec.rs AV1 空臂）回写 `docs\stage0-env-checklist.html`。
- 阶段 1 媒体面最小闭环：在采集+编码内核外另起 WS 信令 + 标准 RTCPeerConnection + `TrackLocalStaticSample` 输出 VP8 压缩负载，对接 selkies-webclient；先出画面独立验收。
- 阶段 1 出画面后补：release 基线、143 MC 动态画面基线、编码输出字节/实测码率、DXGI 恢复回退链路。
