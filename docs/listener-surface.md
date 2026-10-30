# 监听面清单与鉴权骨架（#88）

> 这份文档回答两个问题：**这台流媒体服务器到底有哪些"外部能碰到"的入口**，
> 以及**未来要在别的项目里做鉴权时，从哪一格开始写**。
>
> 本项目的态度是只建骨架：入口全部接好、裁决函数全部就位、默认全部放行。
> 具体谁能看、谁能动，属于 webgame 之类的上层产品，不在这里实现。
>
> 代码：`crates/stream-server/src/security/`（后端）、
> `addons/selkies-web-core/lib/listener-surface.js`（前端）。
> 单测 91 通过（`cargo test -p stream-server`），前端行为自测 14 项。

---

## 1. 三个层次，不要混在一起谈

"监听"在这个项目里指三样不同的东西，混起来谈会得出错误的结论：

| 层 | 在哪 | 长什么样 | 谁能拦 |
|---|---|---|---|
| **L1 浏览器事件** | 页面 | `addEventListener('keydown', …)` | 前端拦截器 `setInterceptor` |
| **L2 传输消息** | WebSocket / DataChannel / HTTP | `kd,65`、`SETTINGS,{…}`、`POST /api/upload` | 后端 `security::check` |
| **L3 主机副作用** | 被控机 | `SendInput`、`SetClipboardData`、`on_track` | 只能在后端拦，且必须在副作用**之前** |

L1 和 L2 不是一一对应：一次 `keydown` 可能发三条上行（修饰键 + 键码 + 心跳），
也可能一条都不发（被本地 IME 吃掉）。**鉴权要做在 L2/L3，L1 只负责"尽早告诉
用户不行"**，否则一个改过的客户端可以直接绕过前端。

这条判断决定了骨架的形状：真正有约束力的是后端那五处插桩，前端那套只为了
体验（不让按键看起来"按下去了但没反应"）。

---

## 2. 总表

面的名字（`surface=`）是**契约**：日志按它 grep，策略按它 switch，前后端用同一
套拼写。`security/mod.rs:125` 的 `label()` 是唯一权威。

| 面 | 触发 | 前端入口 | 后端入口 | 当前校验 |
|---|---|---|---|---|
| `startup` | 进程启动 | — | `main.rs:96` | 无（拦了会 `bail`） |
| `http.status` | `GET /api/status` | — | `http/server.rs:221` | 无 |
| `http.upload` | `POST /api/upload` | `file-upload.js:125` | 同上 | 无 |
| `http.files` | `GET /api/files/…` | — | 同上 | 无 |
| `http.static` | 页面/bundle/jsdb | — | 同上 | 无 |
| `http.signaling` | WS 升级路径 | — | 同上 | 无 |
| `http.health` | `GET /api/v1/health` | — | `http/health.rs:handle_health` | 无（#93，见 `health-api.md`） |
| `http.control` | `POST /api/v1/control/*` | — | `http/health.rs:handle_control` | 无（#93，白名单：`capture/restart`、`session/kick`） |
| `signaling` | 真正建立会话 | `signaling.js:481` | `http/server.rs:321` | 无 |
| `control.*` | 设置/码率/统计开关 | `selkies-wr-core.js` | `websession.rs:846` | 无 |
| `input.keyboard` | `kd`/`ku`/`kh`/`kr` | `input.js:5894` | `websession.rs:811` / `:846` | 无 |
| `input.pointer` | `m`/`m2`/`p`/`_pointer_echo` | 同上 | 同上 | 无 |
| `input.touch` | `sf` | 同上 | 同上 | 无 |
| `input.gamepad` | `js` | `gamepad.js:125` | 同上 | 无 |
| `input.text` | 输入法提交 | `input.js:5894` | 同上（走 `kd`） | 无 |
| `clipboard.read` | `REQUEST_CLIPBOARD` | `clipboard-sync.js:1718` | `websession.rs:846` | 无 |
| `clipboard.write` | `cw*` / `cb*` | `clipboard-sync.js:1719` | 同上 | 无 |
| `media.video-out` | 采集→编码→发送 | — | `websession.rs:846`（`START_VIDEO`） | 无 |
| `media.audio-out` | 桌面声音 | `START_AUDIO` | 同上 | 无 |
| `media.mic-in` | 麦克风上行 | `webrtc.js:434` | `webrtc/mod.rs:292` | 无 |
| `media.webcam-in` | 摄像头注入 | `webcam-capture.js:723` | 未单独插桩（见 §8） | 无 |
| `file.upload` | 上传一个文件 | `file-upload.js:125` | `http/server.rs:221` | 无 |
| `lifecycle` | 可见性/前后台 | 多处 | — | UI 用 |
| `viewport` | resize/全屏/指针锁 | 多处 | — | UI 用 |

"当前校验"全是"无"——这正是本项目的现状（局域网内裸奔，2.5 未做）。骨架的价值
不是现在挡住谁，而是**以后只需要写一个策略文件**。

---

## 3. 逐类详解

### 3.1 启动与生命周期

**后端启动序列**（`main.rs`），每一步都是一个可裁决点：

| 步 | 行 | 做什么 | 失败会怎样 |
|---|---|---|---|
| 1 | 65 | `timeBeginPeriod(1)` | 采集节拍被 15.6 ms 粒度拖到 ~57 Hz |
| 2 | 67 | `Config::parse` | 未知参数只 warn，不退出 |
| 3 | 70 | `--no-hdr` 关闭 HDR 探测 | VM 上少一条每帧 warn |
| 4 | 74 | 文件日志 | 起不来 |
| 5 | **80–107** | **安装策略 + 裁决 `startup`** | 策略拒绝 → `bail` |
| 6 | 404 | `serve()`：TLS 装配 → bind → accept | 端口占用则退出 |

第 5 步的位置是刻意的：**必须早于 `serve`**。装晚了几十毫秒，最早那几个请求
就会走默认分支——而"刚启动、还没来得及装策略"恰恰是最该拒绝的时刻。

**前端启动序列**（`selkies-core.js`）：

- `185`：`window.selkiesSurface` 在**模块求值时**挂上，早于初始化。理由同上：
  首屏那几十毫秒的按键不能漏。
- `196`：`selkiesCoreInitialize()` 由 dashboard 调用（`main.jsx:70`），
  然后 `199` 切模式、`209` 装统计浮层（包了 try/catch —— 可选浮层不许拖垮主界面）。
- `main.jsx:80`：React 树在这之后才挂载，所以 core 里任何未捕获异常都会表现成
  "左侧工具栏不见了"。

**生命周期事件**（`visibilitychange`、`pagehide`、`freeze`、`blur`）走 `lifecycle`
面。它们本身不改主机状态，但在"人走了要不要锁会话"这类策略里是第一手信号。

### 3.2 HTTP

五个端点，全部经过 `http/server.rs:221` 一次裁决。这一处是**整个 HTTP 面的唯一
闸门**：`handle_inner` 是每个 TCP 连接的唯一入口，请求头解析完之后、任何响应
写出之前。

```
GET  /api/status              → http.status
POST /api/upload              → http.upload
GET  /api/files/<rel>         → http.files
GET  /api/<app>/signaling/    → http.signaling（再做一次 signaling 裁决）
GET  其它                     → http.static
```

拒绝时回 `403 Forbidden`，纯文本。**不回原因**：`Deny(&'static str)` 里的字符串
只进日志，客户端拿不到——否则等于把策略模型送给了对方。

`Route::Files` 同时承载"列目录"和"下载"，两者都是读，所以统一映射到
`Endpoint::Files`；真正的写入口只有 `Endpoint::Upload`。这条判断写在
`http/server.rs:46` 的 `surface_of` 里，是**唯一**需要跟着 `Route` 变化的地方。

### 3.3 信令

`http.signaling` 只回答"这个 URL 存不存在"；`signaling` 回答"你能不能拿到一路
画面"。两者分开是因为它们的失败模式不同：前者是 404，后者是 403。
`http/server.rs:321` 这一道就是未来的"观众 / 操作员"分级落点。

WS 信令的消息面（`signaling/line.rs`）：

| 方向 | 形态 | 说明 |
|---|---|---|
| C→S | `{"sdp":{"type":"offer","sdp":"…"}}` | 浏览器应答 |
| S→C | `<peer_id> {"sdp":…}` | 服务端 offer/answer |
| 双向 | `{"ice":{…}}` | 候选交换 |
| S→C | `{"type":"ping"}` | RTT 探测（#78） |
| C→S | `pong,<epoch>` | 回执，**不裁决**（拒绝它只会让 RTT 失真） |

### 3.4 控制通道（`control` DataChannel）

这是最密的一片：非指针指令全在这里。分类入口 `dispatch/commands.rs:78`，
裁决入口 `websession.rs:846`（一条 `if`，覆盖下面整张表）。

| 上行 | 语义 | 面 |
|---|---|---|
| `START_VIDEO` / `STOP_VIDEO` | 开/关采集 | `media.video-out` |
| `START_AUDIO` / `STOP_AUDIO` | 开/关声音下行 | `media.audio-out` |
| `_stats,1` / `_stats,0` | 统计推送开关 | `control.stats` |
| `_gz,1` | gzip 协商（服务端回 `_gz,0`） | `control.gzip` |
| `_arg_fps,N` | 帧率 | `control.fps` |
| `r,WxH` | 分辨率 | `control.resolution` |
| `SETTINGS,{json}` | 批量设置 | `control.settings` |
| `vb,N` | 视频码率 kbps | `control.bitrate` |
| `ab,N` | 音频码率 bps | `media.audio-out` |
| `SET_NATIVE_CURSOR_RENDERING,0\|1` | 指针渲染交接 | `control.cursor-rendering` |
| `REQUEST_CLIPBOARD` | 读主机剪贴板 | `clipboard.read` |
| `cw/cws/cwd/cwe` | 文本剪贴板上行 | `clipboard.write` |
| `cb/cbs/cbd/cbe` | 二进制剪贴板上行 | `clipboard.write` |
| `kd/ku/kh/kr` … | 输入 | `input.*` |
| `pong,<epoch>` | RTT 回执 | **豁免** |
| `s`/`_crf`/`_rc`/`_ebc`/`cmd`/`_stats_video`/`_f`/`_l` | B 组噪音 | **豁免** |
| 解析不出来的 | — | **豁免** |

豁免的三类写在 `websession.rs:637` 的 `surface_of` 里，返回值是 `Option`：`None`
表示"不构成访问"。**除这三类外每一条都必须映射**，`every_action_maps_to_a_surface_or_is_exempt`
这条测试会在新增 `Action` 变体时失败——漏映射等于那条指令永远静默绕过策略。

### 3.5 输入

前端所有输入监听收口在 `input.js:5894` 的 `addListener`：60 多处注册全部经过它，
所以一个包装就覆盖了键盘、指针、触屏、手柄、滚轮、文本输入。面由事件名查表
（`listener-surface.js:31` 附近的 `DOM_SURFACE`），未登记的事件名会在第一次
触发时 `console.warn`，不会被静默放行。

| 浏览器事件 | 上行命令 | 面 |
|---|---|---|
| `keydown` / `keyup` | `kd,<keysym>` / `ku,<keysym>` | `input.keyboard` |
| 按住期间心跳 | `kh,…` | `input.keyboard` |
| 松键（无 keyup） | `kr` | `input.keyboard` |
| `pointerdown/up` | `p,<button>,<state>` | `input.pointer` |
| `pointermove` | `m,x,y`（绝对）/ `m2,dx,dy`（锁定） | `input.pointer` |
| `pointerrawupdate` | 同 `m`，更高频 | `input.pointer` |
| `touchstart/move/end` | `sf,…` | `input.touch` |
| `wheel` | 转成 `m`/`m2` 的增量 | `input.pointer` |
| 手柄（Gamepad API） | `js,…` | `input.gamepad` |
| 输入法提交 | 逐码点 `kd`/`ku` | `input.text` |

后端在 `websession.rs:662` 的 `device_of` 按首 token 归类。**归类错了策略就会
误伤**：`sfe`（焦点事件）和 `vp`（视口尺寸）既不按键也不移动指针，所以归
`Other` 而不是 `Keyboard`/`Pointer`——否则"只允许键鼠"的策略会把视口同步一起
放行，而那正是想单独关掉的东西。

两条传输路径要分清：

- `control` 通道：非指输入（按键、点击、手柄）。
- `pointer` 通道：无序、不重传，只走 `m`/`m2` 的合并运动（#72）。

两条都有独立的 `on_message`，也各自裁决（`websession.rs:811` 与 `:846`）。
**只插一条是不够的**：指针通道上的消息永远不会经过控制通道的分支。

### 3.6 剪贴板

两个方向，权限模型里从来不是一回事：

- `clipboard.read`：`REQUEST_CLIPBOARD`，把主机剪贴板读回浏览器。
- `clipboard.write`：`cw*`（文本）/ `cb*`（二进制），把浏览器剪贴板写进主机。

前端在 `clipboard-sync.js:1713–1719` 用 `attach()` 注册，两个方向显式分开。
旁边那对 `keydown`/`keyup`（`:1713`、`:1714`）只是本地节流——粘贴键按住期间
把上行推迟一拍，没有副作用——所以显式归 `lifecycle`：键盘被拒时这点本地顺序
控制不该跟着停。

**别按事件名归类这一对**：它们是 `keydown`，查表会落进 `input.keyboard`，
于是"拒绝键盘"会顺带把剪贴板的节流逻辑弄坏，症状是粘贴偶发丢内容。

文本与图像的取舍在 `dispatch/clipboard.rs`：文本优先（Excel 复制区域时两者都有）。

### 3.7 媒体

四个方向，只有两个能"进来"：

| 面 | 方向 | 入口 | 备注 |
|---|---|---|---|
| `media.video-out` | 出 | `START_VIDEO` → 采集循环 | 屏幕画面 |
| `media.audio-out` | 出 | `START_AUDIO` → Opus 编码 | 桌面声音，64 kbps |
| `media.mic-in` | **入** | `webrtc/mod.rs:292` | 浏览器麦克风 |
| `media.webcam-in` | **入** | `webcam-capture.js:723` | 摄像头画面（WebCodecs 编码上行） |

麦克风那道裁决放在 `mic_started.swap(true)` **之前**。放在之后的话，被拒绝时
标记已经置上，下一条 track 会被当成"第二条"忽略，日志写"second audio track
ignored"——真相反而是"第一条就被策略挡了"，排查时会被这行日志带偏。

`on_track` 只在**第一个 RTP 包**到达时触发（不是协商时），所以要拦住麦克风，
必须在这里，不能指望前端按钮。

### 3.8 文件

`file.upload` 出现两次，是刻意的：

- 前端 `file-upload.js:125`：拒绝时立刻回一条 `status:'error'` 给面板，用户看到
  "Upload denied by policy"而不是一个卡住的进度条。
- 后端 `http/server.rs:221`：唯一有约束力的那一道。

路径校验（穿越、盘符、保留字符）在 `http/file_transfer.rs`，与鉴权是两回事：
前者防"写到不该写的地方"，后者防"不该写的人"。

### 3.9 诊断

`control.stats`（统计推送开关）与 `/api/status` 都不改主机状态，属观测类。
`EventKind::is_observational()`（`security/mod.rs:172`）把"什么算观测"钉在一个
地方，未来的只读角色直接复用它，不必每个策略各写一遍。

---

## 4. 后端 API

```rust
// 装策略（进程内一次，通常在 main）
security::install(Arc::new(MyPolicy))?;

// 裁决（插桩点唯一该调的）
match security::check(EventKind::Input(Device::Keyboard), &principal) {
    v if v.allowed() => { /* 放行 */ }
    v => log::debug!("denied: {:?}", v.reason()),
}

// 便捷
if !security::allow(kind, &principal) { return; }
```

实现一个策略（`security/mod.rs:283`）：

```rust
use stream_server::security::{Event, EventKind, MediaDir, Policy, Principal, Verdict};

struct LanOnly { allowed_net: ipnet::IpNet }

impl Policy for LanOnly {
    fn name(&self) -> &'static str { "lan-only" }

    fn check(&self, ev: &Event<'_>) -> Verdict {
        // 无地址（DataChannel）时按会话已授权处理 —— 它是在 WS 升级时进来的。
        let Some(peer) = ev.principal.peer else { return Verdict::Allow };
        if !self.allowed_net.contains(&peer) {
            return Verdict::Deny("peer outside lan");
        }
        // 观众只能看，不能动。
        if !ev.kind.is_observational() && ev.principal.credential == Credential::None {
            return Verdict::ReadOnly;
        }
        Verdict::Allow
    }
}
```

三条约定，写在 trait 的文档里但值得重复：

1. **纯函数**。它跑在采集循环和 DataChannel 回调里，每次输入都过一遍；
   在这里读数据库或等网络，键鼠就会卡。需要外部状态时，`install` 之前查好塞进
   结构体。
2. **失败关闭**。策略内部出错应当返回 `Deny`，不是 `Allow`。
3. **不泄漏**。`Deny` 的原因是给日志看的，不要回给客户端。

`--policy` 当前只接受 `allow-all`（默认）与 `deny-all`。拼错时退回 `allow-all`
并告警——默认本来就是全放行，为一个拼写错误停服不划算，但那声 warn 必须留着。

## 5. 前端 API

```js
// 页面里，任何时机
window.selkiesSurface.setInterceptor((surface, detail) => {
    if (surface === 'media.mic-in') return false;        // 不给开麦
    if (surface === 'clipboard.read') return false;      // 不给读回剪贴板
    return true;
}, 'webgame-policy');

window.selkiesSurface.stats();
// [{ surface: 'input.pointer', allowed: 8123, denied: 0 }, …]
```

`detail` 是 `{ type, event }`，`event` 在非 DOM 触发（麦克风、摄像头、上传）时
没有。**拦截器必须同步**：它跑在 `pointerrawupdate` 这种每帧几十次的路径上。

没有装拦截器时 `admit()` 只做一次 `if (!interceptor) return true` 就返回，
不计数、不建 Map，热路径上没有可感知的额外开销。

## 6. 怎么验"真的接上了"

`deny-all` 不是安全模式，是**覆盖率探针**：

```powershell
C:\dev\stream\stream-server.exe --policy deny-all
```

预期现象：

- **进程照常启动**（`startup` 被 `DenyAll` 显式放行。第一版没放行，实测启动
  即 `bail`，探针自己先死了）；
- 页面打不开（`http.static` 被拒）；
- 能连上 WS 也拿不到画面（`signaling` 被拒）；
- 日志每来一个请求就一行
  `authz deny surface=http.static peer=192.168.59.1 … policy=deny-all reason=deny-all policy`。

反过来看：**某个功能在 `deny-all` 下照常可用，那个入口就是漏了插桩。**
这就是这道探针的全部意义。

后端的这三个面在单测里已经锁死：`deny_all_covers_the_whole_surface`、
`every_action_maps_to_a_surface_or_is_exempt`、`input_lines_classify_by_device`。

## 7. 已知空白（有意留的）

| 空白 | 为什么 | 补的时候注意 |
|---|---|---|
| DataChannel 层 `Principal` 是空的 | peer 地址只在 TCP 建连时存在，DC 回调里拿不到；会话号还没引入 | 正确做法是在 WS 升级时把 token 解析成角色，随 `MediaSession` 带进来，**插桩点不用动** |
| `media.webcam-in` 后端没有独立插桩 | 摄像头画面走的是另一条上行路径（#77 的 webcam 轨），当前只在前端拦 | 加的时候注意它可能在 `on_track` 里与麦克风同一个回调 |
| 没有 token / cookie 解析 | 2.5 鉴权本身还没做 | `Credential` 枚举已经留好 `Bearer`/`Master` |
| 前端拦截器不防绕过 | 改过的客户端可以跳过整层 | 真正的约束必须在后端；前端只为体验 |
| `http.status` 不检测频率 | 无鉴权时它只是 `ok\n`，扫不出东西 | 做鉴权时顺手加限流，别单独做一个 |

## 8. 一句话总结

入口一共 22 个面，后端五处插桩把它们全覆盖了（HTTP 一处、信令一处、控制通道
一处、指针通道一处、麦克风一处），每一处的裁决都发生在副作用**之前**。
现在所有策略都是"全放行"，所以这一版不改变任何行为；等到 webgame 需要区分
"谁能看、谁能动"时，写的应该只是一个 `impl Policy`。
