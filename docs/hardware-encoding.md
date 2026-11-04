# 硬件编码：只预留能力，不预留谎言（#95）

目标很明确：**今天不实现硬编，但要让「换一块显卡」成为一次硬件变更，而不是一次
代码变更**。所以这一轮做的是能力探测 + 后端决议 + 可观测，而编码器本身仍然是
libvpx。

代码：`crates/stream-server/src/platform/hwenc.rs`（探测与决议）、
`--encoder`（`config.rs`）、启动三条日志（`main.rs`）。

## 1. 门槛（按需求写，不按市场话术写）

| 厂商 | 门槛 | 编码器 | 说明 |
| --- | --- | --- | --- |
| NVIDIA | GTX 10 系及以上 | NVENC | Pascal 起 H.264/HEVC 都够 1080p60，是三者最强 |
| AMD | RX 400 及以上 | AMF | Polaris 起 VCE 3.4 才谈得上可用 |
| Intel | HD/UHD/Iris/Arc 核显 | QSV | 1080p60 画质一般，排在最后 |

优先级固定为 **NVIDIA → AMD → Intel**，同一厂商取第一个满足门槛的适配器。理由：
核显往往和独显同时存在，而核显是三者里最弱的一档，不该因为枚举顺序靠前就抢走。

## 2. 探测什么，不探测什么

**只枚举显示适配器（`EnumDisplayDevicesW`），不去开 `nvEncodeAPI64.dll`。**

开机时试着打开编码器 API 是典型的「探测会撒谎」：在独显被省电关掉的笔记本上它
成功；在驱动正常但还没有交互会话的机器上它失败。而适配器列表是 OS 已经掌握、
静态、无副作用的数据。真正的「能不能编」应该在构造编码器时回答，不在启动时猜。

判定分三步：

1. **厂商**取自 PCI 厂商号（`VEN_10DE` / `VEN_1002` / `VEN_8086`），不看名字——
   驱动显示名可能被本地化、也可能为空。
2. **代际**取自适配器名字（`RTX` / `GTX 10` / `GTX 16` / `RX 4xx` / `UHD` …）。
   PCI device id 能精确到芯片，但要转成「有没有 NVENC」需要一张随每代更新而失效
   的对照表；名字里带的是市场分档，恰好就是需求写的那条线。
3. **三态**：`Yes` / `TooOld` / `Unknown`。**`Unknown` 绝不提升成 `Yes`**——
   错的 `yes` 会把会话送进一个 open 就失败的编码器，错的 `no` 只是少一次优化。

另外检查用户态运行时 DLL 是否在盘上（`nvEncodeAPI64.dll` / `amfrt64.dll` /
`libvpl.dll` …）。这是「没有硬件」和「有硬件但驱动装残了」的唯一区别，也是排障时
最想看到的一行。

## 3. 决议：`--encoder`

| 值 | 行为 |
| --- | --- |
| `auto`（默认） | 永不失败。探测到什么写进日志，最终仍用软件编码（本构建没有硬编） |
| `software` | 明确禁用硬编 |
| `nvenc` / `amf` / `qsv` | 点名要求。硬件不在、或运行时缺失，**启动即报错退出** |

点名的语义是「我要这个画质档位」，所以不能静默降级——否则错误会一直藏到第一帧
看起来不对。默认是 `auto`，所以换机器、换显卡都不会导致服务起不来。

## 4. 日志长什么样（三种机器）

```
# .148（VMware 虚拟机，交互会话）—— 实际输出，逐字
encoder probe | candidate=none; adapters: VMware SVGA 3D [other too-old]
encoder probe | adapter "VMware SVGA 3D" vendor=other support=too-old attached=true id=PCI\VEN_15AD&DEV_0405&SUBSYS_040515AD&REV_00
encoder backend=Software | no hardware encoder among 1 adapter(s); using software
encoder tuning | buf=1000ms err_res=false screen=true intra_cap=300% (--tune legacy reproduces pre-#95)

# 装了 RTX 3060，但这个二进制没编硬编
encoder probe | candidate=nvenc on "NVIDIA GeForce RTX 3060"; adapters: NVIDIA GeForce RTX 3060 [nvidia yes]
encoder backend=Software | nvenc detected on "NVIDIA GeForce RTX 3060" but hardware encoding is not
                             built into this binary (build with --features hwcodec); using software

# 会话 0（服务/计划任务在登录前跑，ssh 里跑）
encoder probe | candidate=none; adapters: none visible to this session
encoder backend=Software | no display adapter visible to this session (session 0 or headless) —
                           hardware capability unknown; using software
```

第三条是必须区分开的：**会话 0 枚举不到任何显示适配器**，把它写成「这台机器没有
显卡」就是一句假话，会让人去买一块自己已经有的卡。实测踩到过一次——ssh 里跑的
探针就是这么报的。

第二条与第一条的区别同样重要：`detected ... but not built into this binary` 说的是
「卡没问题、程序旧了」，而不是「卡不行」。这两种情况在浏览器里的表现一模一样。

## 5. 等真正接上硬编时要改的地方

只有 `Backend::Hardware` 那一支：

1. `scrap` 开 `hwcodec` feature（`vendor/scrap/src/common/hwcodec.rs` 766 行已实现：
   `HWRAM`/`VRAM`、`is_hardware()`、`force_keyframe()`，`vram` 走 D3D11 纹理直通）。
   构建需要 `VCPKG_ROOT` + ffmpeg 的 `x64-windows-static`（`vendor/scrap/build.rs:103`）。
2. `hwenc::decide` 在候选可用时返回 `Backend::Hardware(enc)`，把同一份 `EncoderCfg`
   换个变体。
3. 探测门槛不用动——那部分与编码器实现无关，已经是纯数据判断。

## 6. 验证

- `cargo test -p stream-server --bin stream-server hwenc`：7 项
  - 厂商取 PCI id 而不是名字（含本地化名字为空的情况）
  - 门槛边界：GTX 1060 = yes，GTX 960 = too-old；RX 480 = yes，R9 390 = too-old
  - `Unknown` 不被提升成 `yes`（这是设计约束，不是覆盖率）
  - VM 适配器不是候选
  - 探测到硬编但构建没有时，日志必须说「没编进去」而不是「没硬件」
  - 空适配器列表说的是「未知」，不是「没有」
  - 点名 `nvenc` 而机器上没有 → 启动报错，错误里带「no nvidia adapter present」
- 线上（.148，交互会话，8099 端口空跑一次）：上面第 1 段那四行，逐字一致。

## 7. 两个探针本身的坑

- **`EnumDisplayDevicesW` 是按「显示」枚举，不是按「芯片」**：.148 有 8 个虚拟
  显示器，第一版输出里同一个 `PCI\VEN_15AD&DEV_0405` 出现 8 次，启动日志变成 8 行
  完全一样的适配器。现在按 PCI id 去重（无 id 的按名字），只留第一条——顺带说明
  枚举顺序里 `attached=true` 的那条在最前面。
- **`--log-file` 不存在，真实旗标是 `--log`**。第一次拿错名字跑探针，参数被当成
  未知项丢掉，日志落进了**正在运行的那个服务**的 `logs\stream-server.log`——同一台
  机器上起两个实例写同一个文件，看起来「一切正常」。凡是「跑了但没输出」，先去确认
  参数名对，再去怀疑程序。
