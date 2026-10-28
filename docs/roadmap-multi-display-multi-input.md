# 预留设计：单设备多显示 与 多操作

> 状态：**未实现，本期只留切口**。
> 本文档的目的不是描述一个已完成的特性，而是把「将来要做」的事钉在现在的架构上：
> 哪些位置现在写死了「一屏 / 一个客户端 / 一个指针」，将来要改哪里、协议怎么扩、
> 哪些约束在 Windows 上根本做不到。
> 写下来的原因很实际：这些假设散落在 `capture.rs`、`settings.rs`、`win_input.rs`、
> `webrtc/track.rs` 里，每处都只有一行注释，下一次重构很容易顺手把它焊死。

---

## 1. 目标定义

两个能力经常被混为一谈，其实互相独立，实现顺序也不同。

| 能力 | 一句话 | 本期 |
| --- | --- | --- |
| **单设备多显示** | 一台被控机有多块屏/多个虚拟显示，客户端能分别看、分别点 | 只留切口 |
| **多操作** | 同一台被控设备同时被多个客户端操作（或至少同时观看 + 一个操作） | 只留切口 |

先做多显示，再做多操作：多操作依赖「这个输入是发给哪块屏的」这件事已经有答案，
否则两个客户端各发一套 `m,x,y` 会互相打架而无法解释。

---

## 2. 现状：写死在哪

这张表是本文档的核心。左边是当前位置，中间是它现在的隐含假设，
右边是引入多显示/多操作后必然爆掉的地方。

| 位置 | 当前假设 | 冲突点 |
| --- | --- | --- |
| `pipeline/capture.rs` | `Output1` 上 `is_primary()` 的那一块，单 DXGI duplication | 多屏需要按 `output_index` 枚举，并允许同时开 N 个 duplication |
| `pipeline/settings.rs` `StreamSettings` | 一组 `width/height/output/fps/bitrate`，全进程一份 | 每屏需要独立分辨率与码率预算 |
| `webrtc/track.rs` | 固定 `video-0` / `stream-0` 一条 `TrackLocalStaticSample` | 多屏需要 `video-<i>`，且 SDP 要能同时协商多条 |
| `webrtc/channels.rs` | 一对 `control` / `pointer` 通道，进程内只有一组 | 多客户端时每个 PeerConnection 各有一对，服务端要能分辨来源 |
| `dispatch/win_input.rs` `WinInputSink` | 目标坐标空间 = 唯一一屏的帧坐标；`map_axis()` 直接映射到 `GetSystemMetrics(SM_CXSCREEN)` | 多屏要按屏 origin 偏移；`SM_CXSCREEN` 只是主屏 |
| `pipeline/cursor.rs` `cursor_pos()` | 返回虚拟桌面绝对坐标，只有一个 OS 光标 | 多屏合成时每屏要用 `虚拟坐标 - 该屏 origin`；多客户端时「谁的光标」没有答案 |
| `signaling/websession.rs` | 一个进程 = 一个会话（`stage1` 边界，注释里写得很明确） | 多客户端需要 `SessionId` 与角色，才能做输入仲裁 |
| `pipeline/mod.rs` 统计行 | `m=` / `m2=` / `dm=` / `paint=` 是进程级原子 | 多客户端时这些计数会混在一起，需要按会话拆 |

**这些计数是进程级静态量**（`MOUSE_REL_EVENTS` 等），这一点在当前单会话模型下是对的，
换成多会话后必须挂到 `SessionCtx` 上，否则诊断信息会失去意义。

---

## 3. 单设备多显示

### 3.1 两条路线，选第二条

| 路线 | 做法 | 问题 |
| --- | --- | --- |
| A. 服务端拼一张大画布 | 把 N 块屏拼成一个虚拟桌面整体编码 | 带宽按总面积走，客户端无法只订一块；分辨率一变整路重编 |
| B. 一屏一路流，客户端排布 | 服务端每屏一条 video track，客户端决定怎么摆 | 需要多 track 协商 + 按需订阅 |

选 **B**。理由：客户端「只看其中一块」时带宽应该只付一块；
A 路线里客户端想把副屏放大到全屏，还得服务端重新裁一次。

B 路线的代价是要解决**按需订阅**：客户端没看的屏不应该在编码。
第一版可以简单点——只编码「被订阅的屏」，订阅关系由客户端在
`SETTINGS` 里声明（见 3.3）。

### 3.2 采集

```rust
// 现在
let output = adapter_output(index=0).is_primary()       // capture.rs 注释已标 Stage1 boundary

// 目标
pub struct DisplayTarget {
    pub index: u32,          // DXGI output index
    pub origin: (i32, i32),  // 该屏在虚拟桌面里的左上角，用于输入与光标映射
    pub size: (u32, u32),    // 原生像素
    pub primary: bool,
}
fn enumerate_displays() -> Vec<DisplayTarget>
```

`origin` 必须从 `DXGI_OUTPUT_DESC.DesktopCoordinates` 取，**不能**用
`GetSystemMetrics(SM_XVIRTUALSCREEN)` 加偏移去推——多屏时排列可能是负坐标
（副屏放在主屏左侧或上方），推出来的值会错。

每块屏一个独立的 capture 线程，共用现有的解耦模型
（`#65` 已经把采集放进独立线程，正好是复用的边界）。

### 3.3 协议扩展（向后兼容）

原则：**旧客户端不改任何一个字节也必须还能用**，所以不修改 `m` 的字段，而是加会话级绑定。

```
// 新增，控制通道
_d,<display_index>          设定本会话的输入目标屏（默认 0）
// 新增，服务端下行
display_list,<n>,<i>:<w>x<h>@<x>,<y>:<primary>,...   显示拓扑上报
```

`m,x,y` 的 `x/y` 语义不变——**仍然是「当前绑定屏的帧像素」**。
这样旧客户端默认绑定屏 0，行为与今天完全一致，`win_input` 的
`frame_to_screen()` 只需要多一步：

```rust
// 现在：frame 坐标 -> 主屏像素
map_axis(x, fw, sw)                 // sw = SM_CXSCREEN

// 目标：frame 坐标 -> 绑定屏像素 -> 虚拟桌面像素
let local = map_axis(x, fw, display.size.0);
let screen = (local + display.origin.0, local_y + display.origin.1);
// SendInput 走 MOUSEEVENTF_VIRTUALDESK + 虚拟桌面尺寸归一化
```

注意 `absolute_norm()` 现在是按 `SM_CXSCREEN/SM_CYSCREEN` 归一到 0..65535，
多屏下必须换成 `SM_XVIRTUALSCREEN/SM_CXVIRTUALSCREEN` 并加
`MOUSEEVENTF_VIRTUALDESK`，否则副屏根本点不到。这是一个**静默失败点**：
不报错，光标就是不出主屏。

### 3.4 光标合成

现有 `cursor_draw_pos()` 已经做了 `(csx - ox) * dst_w / src_w`——`virtual_origin()`
这一步正好是为多屏准备的。多屏时每路 pipeline 传自己的 `origin` 即可，
计算本身不用改。

真正的麻烦在 `CursorData` **不含位置**（`cursor.rs` 只给形状与热点）。
多屏下同一形状要画在 N 个位置，所以每路 pipeline 必须各自调 `cursor_pos()`，
不能共享一个缓存的坐标。

### 3.5 带宽预算

当前实测：4 vCPU / 8 GB 虚拟机上单路 1718×926@60fps VP8 2500 kbps 占整机 45–48%。
两条路 60fps 会直接打满。设计时必须给出：

- 每屏独立 fps（副屏默认 15，主屏保留 60）
- 每屏独立 `repeat_every`（静止副屏不需要 60 次/秒的空帧重编）
- 「只编码被订阅的屏」——没订阅就只采不编

---

## 4. 多操作

### 4.1 先说 Windows 做不到的事

**Windows 只有一个系统指针。** `SendInput` 注入的相对/绝对移动改的是
同一个 OS 光标，不存在「A 客户端的鼠标在左边、B 客户端的鼠标在右边」这回事。

所以「多操作」在这个架构里只能是以下三种之一：

| 模型 | 行为 | 可行性 |
| --- | --- | --- |
| **独占租约** | 同一时刻只有一个客户端能注入，其余只读；可申请/抢占 | 可行，第一版就做这个 |
| **时分共享** | 都持有租约，输入交错注入（谁动谁赢），各客户端画自己的指示光标 | 可行但体验差，只在特殊场景开 |
| **真并行多指针** | 多个独立指针同时存在 | **不可行**，需要多个 Windows 会话（RDP/多用户），完全换掉采集与注入模型 |

文档里必须写这一条，否则「多操作」很容易被理解成第三种，做完发现做不到。

### 4.2 租约模型

```rust
pub struct InputLease {
    holder: SessionId,
    since: Instant,
    mode: LeaseMode,     // Exclusive | Shared
}

pub enum SessionRole { Controller, Viewer }
```

`WinInputSink` 目前是所有会话共享的注入目标，多会话时要前置一层判权：

```
pointer/control 消息 -> SessionCtx.route(line) -> [租约检查] -> WinInputSink.handle()
```

没有租约的会话（`Viewer`）仍然照常收视频与看光标，只是注入被丢弃，
并回一行 `input_denied,<reason>` 让前端把指针变灰而不是让人以为坏了。

### 4.3 每客户端的光标反馈

OS 光标只有一个，但每个客户端需要看到「我自己的指针在哪」。
现有前端已经有 `cursorDiv`（页面自绘光标），多会话时：

- `Controller`：看到的应该是服务端合成的 OS 光标（现状，`SET_NATIVE_CURSOR_RENDERING,1`）
- `Viewer` / 非持租约者：看到自绘指示光标，位置由本地 `movementX` 积分，**不注入**

这要求会话在 `SET_NATIVE_CURSOR_RENDERING` 之外，多一个「我是否持权」的下行通知。

---

## 5. 本期就能落的小切口

不改任何行为，只把将来必须改的地方变成「一个参数」而不是「一行硬编码」。
每条都能独立提交，都能回滚。

- [ ] `StreamSettings` 增加 `display: u32`（默认 0），`width()/height()` 语义不变
- [ ] `capture.rs` 把 `is_primary()` 换成 `output_at(settings.display)`，行为等价
- [ ] `stream_info` 下行加 `display` 字段（老前端忽略未知字段即可）
- [ ] `WinInputSink` 增加 `target_origin: (i32, i32)`，默认 `(0, 0)`；
      `frame_to_screen` 末尾加偏移，默认值为 0 时行为不变
- [ ] `websession` 引入 `SessionId` 与 `SessionRole::Controller`，单会话下恒定，
      但注入路径先经过 `SessionCtx`（空实现），为租约留位置
- [ ] 统计行的 `m= m2= dm= pm=` 从进程级 `AtomicU64` 改为 `SessionCtx` 上的字段

最后一条看着像无用功，但它是多会话诊断的前提：**现在这些原子量一旦多会话就会
把两个客户端的输入混成一个数字**，届时「指针不动」会重新变成不可归因的问题。

---

## 6. 验收标准（将来用）

多显示：
1. 两块屏（主屏 + 副屏，副屏放在主屏**左侧**使 origin 为负）都能单独出图
2. 客户端绑定屏 1 后点击，落点误差 ≤ 2 px，且光标能到达副屏最右/最下像素
3. 客户端只看屏 0 时，屏 1 的编码器不出帧（`enc_ok` 不增长）

多操作：
1. A 持租约时 B 的输入被丢弃并收到 `input_denied`，B 的页面光标仍可自由移动（本地）
2. A 断开后租约自动回收，B 可申请成功
3. 两客户端同时连接时，5 秒统计行里 `m=`/`m2=` 按会话分别可读

---

## 7. 明确不在本期范围

- 多 Windows 用户会话 / 真并行多指针（需要换掉 DXGI 采集与 SendInput 注入模型）
- 音频多路（单设备多显示通常伴随每屏音频，采集侧完全没有这个能力）
- 客户端侧多屏「无缝跨屏投递」——即鼠标从 A 屏滑出直接进 B 屏。
  这需要前端做拼接与边界穿越，服务端只需保证 `display_list` 里有 origin 与尺寸，
  属于客户端工作，但**协议里必须有 origin**，否则将来做不了。
