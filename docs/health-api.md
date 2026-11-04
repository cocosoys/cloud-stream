# 虚拟机状态 API（#93）

> 一条命令问出"这台虚拟机活着吗、它在推什么、谁在看"。
>
> `curl -s http://<host>:<port>/api/v1/health | jq`

---

## 1. 为什么不是 `/api/status`

`GET /api/status` 回 `ok\n`。它回答的是"端口通不通、进程在不在"。

这台机器上有过这样的现场：进程好好地躺着、端口好好地开着、`/api/status`
200，但画面早就不动了（DXGI 复制输出失效、编码器停摆、track 写入全在报错）。
`status` 对此一无所知，因为它读的是"进程还活着"这一个事实。

所以这个端点补的是**推流健康**：判据是 `last_sample_at` —— 上一次有帧真正写进
RTP track 的时刻。新帧算，重复帧也算（静止桌面本来就只发重复帧），只有"什么都
没出去"才是卡死的证据。

**HTTP 状态码就是判活结论**，不要在 JSON 里翻字段：

| 状态码 | 含义 |
|---|---|
| `200` | 进程活着，且推流链路没卡（包括"没人连着"和"客户端暂停了"） |
| `503` | 进程活着，但该出帧不出帧（`verdict: "stalled"`） |

一个 uptime 监控器只需要看状态码；要看细节的才解析 body。

---

## 2. 判活边界

| 情形 | `verdict` | `ok` | 状态码 | 理由 |
|---|---|---|---|---|
| 没人连 | `idle` | true | 200 | 开着没人看是正常状态 |
| 连上了，还没出第一帧（<15 s） | `starting` | true | 200 | 建 PC、开采集器都在这段里 |
| 出帧正常 | `streaming` | true | 200 | — |
| 客户端按了暂停（`STOP_VIDEO`） | `paused` | true | 200 | 用户的选择，不是故障 |
| 出过帧，但 3 秒没有东西写进 track | `stalled` | **false** | **503** | 这就是"进程在、管线死" |

`flags` 数组是给人看的旁证，不参与裁决：

| flag | 触发 |
|---|---|
| `no_frame_for_over_3s` | 判 `stalled` 的那一条 |
| `no_first_frame_after_15s` | 建连 15 s 还没首帧（可能浏览器没发 `SESSION`） |
| `new_errors_since_last_check` | 采集/编码/track 写入的累计计数比**上次查询**大了 |

最后一条为什么是"上次查询以来"而不是"本窗口"：`FrameCounters` 由统计定时器每秒
清零，而定时器**只在面板的统计开关打开时才跑**。面板关着时那些计数就是自采集循环
启动以来的累计值（实测 `repeat: 534` 配 `new: 1`），拿它当窗口判据会说谎。所以
基线由探针自己记，措辞与事实一致。第一次查询没有基线，`errors_since_last_check`
是 `null`，不下结论。

---

## 3. 响应结构

```jsonc
{
  "ok": true,
  "verdict": "streaming",
  "flags": [],
  "now": 1761780000,

  "server": {
    "version": "0.1.0",
    "pid": 8508,
    "uptime_s": 3600,
    "host": "WIN-VM01",          // COMPUTERNAME，用来确认"回答我的是哪台"
    "policy": "allow-all",
    "input_policy": "shared",    // shared（默认）| exclusive，见 §3 的 role 说明
    "tls": false,
    "listen": "0.0.0.0:8081"
  },

  "host": {                       // 与右上角面板同一份口径
    "available": true,
    "cpu_percent": 12.5,          // 整机，占全部逻辑核心
    "process_cpu_percent": 88.1,  // 本进程，占**单核**（>100 正常）
    "cpu_cores": 4,
    "mem_used": 4294967296,
    "mem_total": 8589934592,
    "mem_percent": 50.0,
    "sample_age_ms": 1200         // 这批数字有多旧（见 §5）
  },

  "stream": {
    "active": true,               // 视频门开着（START_VIDEO 之后）
    "throttled": false,           // QoS 是否把码率压到了基线下
    "width": 1920, "height": 1080, "fps": 30,
    "encoder": "libvpx", "codec": "vp8",
    "rate_control": "cbr",        // cbr | crf
    "crf": 25,                    // 仅 crf 模式有意义
    "target_kbps": 2500,
    "warm": true,
    "last_sample_age_ms": 33,     // 判活的核心数字
    "timing": { "encode_ms": 4.2, "pipeline_ms": 9.1, "send_ms": 1.2 },
    "frames_total": {             // 累计值，见下
      "new": 30, "repeat": 0, "key": 1,
      "capture_err": 0, "encode_err": 0, "write_err": 0
    },
    "errors_since_last_check": null,  // 首次查询没有基线
    "checked_ago_ms": null,
    "pli_requests": 3,            // 浏览器请求关键帧的次数，涨=下行丢包
    "audio": { "enabled": true, "bitrate_bps": 96000 }
  },

  "processes": {
    "game_running": true,
    "game": [ { "pid": 4212, "name": "javaw.exe", "cpu_percent": 55.5, "mem_bytes": 2147483648 } ],
    "top":  [ /* 按 CPU 降序前 5 名，同样的四项 */ ]
  },

  "admission": {                  // 还收不收新会话（见 §3.2）
    "max": 8,                     // --max-sessions
    "live": 2                     // 已占用的名额（含正在握手、还没发 HELLO 的）
  },

  "clients": {
    "count": 2,
    "items": [{
      "id": 1,                    // 进程内单调递增的会话号
      "ip": "192.168.1.5",
      "device_id": "9f2c…",       // localStorage 持久 UUID（见 §4）
      "tab_id": "1b7e…",          // sessionStorage，关标签即失效
      "role": "controller",       // 能不能动手：shared 下恒为 controller（见下）
      "client_type": "controller", // 页面在 HELLO 里**自称**的角色，仅供对照
      "display": 0,               // 挂在哪块屏上；未 attach 为 null
      "user_agent": "Mozilla/5.0 …",
      "connected_s": 42,
      "reconnects": 3,            // 这个浏览器此前连过几次（首连为 0）
      "bytes_total": 21474836,    // 本会话自己那条下行累计字节
      "frames_total": 1260,       // 本会话自己那条下行累计帧数
      "avg_mbps": 4.09,           // bytes_total × 8 ÷ connected_s，按会话全程平均
      "denied_inputs": 0          // 被角色门禁丢掉的输入条数（见下）
    }, {
      "id": 2,
      "ip": "192.168.1.5",
      "device_id": "9f2c…",       // 同一个浏览器，第二个标签/第二个显示器页面
      "tab_id": "4c9a…",
      "role": "controller",       // 默认 shared：第二个页签同样能操作
      "client_type": "controller", // 页面自称；和 role 不一致才是值得看的信号
      "display": 0,
      "user_agent": "Mozilla/5.0 …",
      "connected_s": 7,
      "reconnects": 0,
      "bytes_total": 3670016,
      "frames_total": 210,
      "avg_mbps": 4.19,
      "denied_inputs": 0          // 只有 --input-policy exclusive 才会增长
    }]
  },

  "pipelines": {                  // 按显示设备共享的采集+编码程序（见 §3.1）
    "count": 1,
    "items": [{
      "display": 0,               // DisplayKey，0 = 主屏
      "holders": 2,               // 挂在同一个程序上的会话数（= refs）
      "downlinks": 2,             // 扇出层当前接入的 track 数
      "active": true              // 视频门开着（与 stream.active 同源，取自该程序的 handle）
    }]
  }
}
```

`clients.count` 可以大于 1：多个页面可以同时看同一块屏。互斥只发生在**同一个
`tab_id`** 上 —— 刷新页面留下的僵尸 socket 会被新连接踢掉（close code 4001），
另一个标签页是合法公民。

`role` 决定这个会话能不能动手。它是**服务端的裁决**，不是页面的自称：

| role | 怎么来的 | 能做什么 |
|---|---|---|
| `controller` | 输入策略是 `shared`（默认），或本会话持有该程序的**输入租约** | 全部：注入输入、改设置、unpin 指针、重置音频开关、开关视频门 |
| `viewer` | 只在 `exclusive` 策略下出现：挂上了程序但不持有租约 | `StatsOn/StatsOff`、`PongReply`、`CompressedNegotiate`、`ClipboardRequest`、`Ignore/Unknown`，以及 **`StartVideo`** |
| `pending` | 还没 attach（只在握手中，`display` 为 `null`） | 什么都做不了 |

**先看 `server.input_policy` 再看 `role`。** 默认 `shared`，意味着每个已经 attach 的
会话都能操作，所以两个页签**都是** `controller`——这是对的，不是上一轮那个
"每个页面自称 controller"的假象（那个字段现在叫 `client_type`）。要恢复单主模式
（只有一个页签能动手）用 `--input-policy exclusive`，此时上表的 `viewer` 才会出现。

为什么默认改成共享：上游 Selkies 本来就没有独占输入；而这台机器上的真实用法是
"同一个人开两个页签"，把第二个挡在门外时，运维看到的现象是**两个页签都按不动**
（因为租约在程序重建后会是空的），`denied_inputs` 每分钟几百——看起来像输入坏了，
实际是门禁在按设计工作。Windows 的输入隔离粒度是**会话**，不是窗口，所以独占租约
换不来任何隔离，只是把单人多页签变成不可用。

`client_type` 是页面在 HELLO 里自报的值，**每个不是严格 viewer 的页面都会说自己是
`controller`**。它只是对照用的记录：一个页面的行为和它的自称不一致时，这个字段能
说明它是"以为自己是 controller"还是"根本没请求过控制"。

租约挂在**程序**上（`SharedPipeline::controller_id`），不是每个会话一份 bool。这样
接管（`session/take/<id>`）对下一条输入消息立即生效，不需要重连，也不会出现"两个人
同时以为自己是 controller"的窗口。持租约的会话退出时，租约自动交给同一 `display`
上**最早**的幸存者；交给"没人"时，下一个 attach 的会话会**认领这块无主租约**
（`try_claim_controller`）—— 没有这一步，一个比创建者活得久的程序会永久无主，
所有页签都是 viewer、重连也修不好（租约按程序记，不按 socket 记）。

`denied_inputs` 是被角色门禁丢掉的输入条数。它只在 `exclusive` 下才会增长；共享
模式下永远是 0。没有这个数字，"第二个页签的指针不动"有两个无法区分的原因：
**消息被拒绝了**，还是**消息根本没到**。非零就说明输入到了、被 `role` 挡了；
一直是 0 而人还在动鼠标，那是传输或前端的问题。

viewer 发来的输入类命令被丢弃而不是断连 —— 两个人在同一块屏上抢指针，比"你按了
没反应"难排查得多。

`StartVideo` 是唯一一个"非对称放行"的写命令：视频门（`video_on`）长在共享程序上，
门关着时循环连采集都不做，所以黑掉的是**所有**会话，不只是发命令的那个。而浏览器
在页签切到后台时会发 `STOP_VIDEO`（`selkies-ws-core.js:6330`），于是 controller 一切
页签、另一个正在看的页面就黑了；它唯一的自救是 3 秒无帧后重发 `START_VIDEO`
（`:4612-4630`，三次后强制重连）。这条若被拒，一次瞬断就变成自我维持的刷新循环。

所以：开门幂等且只花 controller 本来就要花的编码代价 → 放行；
关门会毁掉别人的画面 → 只归 controller。代价是"controller 手动暂停"在有人正看着时
不持久，这是刻意选的——另一个方向的故障（viewer 永远恢复不了）在浏览器侧没有出路。

> 已知遗留：扇出层还没有"按 sink 暂停"。viewer 切到后台时它的 `STOP_VIDEO` 被丢弃，
> 服务端仍往那条 track 写帧；上游 Selkies 的做法是 `sharedVideoPaused` 只停这一路
> （控制、光标、音频照旧）。要做的是给 `VideoFanout` 的每个 sink 挂一个 `want` 标志，
> 写帧时跳过它、并把全局门改成"有人想要就算开"。

`frames_total` 是**累计值**，不是速率也不是"本秒"：自本次采集循环启动以来的总数。
要速率就对两次响应做差（和 Prometheus 抓 counter 的用法一样）—— 这也是为什么键名
带 `_total` 后缀。`encode_ms` / `pipeline_ms` / `send_ms` 是统计窗口内的均值，窗口
由统计定时器决定；面板关着统计时它们覆盖的时间会更长。

### 3.1 `pipelines` 为什么单独一段

`stream.*` 描述的是**画面本身**，`clients` 描述的是**人**。中间那层——"谁在采、谁在编、
这一帧发给了几条 track"——以前被隐式当成"和会话一一对应"，所以没有名字。现在它有了。

注意 `stream` 段取的是**主屏那条程序**（`pipelines.first()`）：分辨率、编码
时间、帧计数都是它的事实。有多条管线时，`stream` 只代表主屏，其余屏的帧计数不在
这里——这是刻意的，避免把两条管线的计数加成一个没有物理意义的数字。

- `holders` 是引用计数（=`SharedPipeline::refs()`）。最后一个会话离开时程序停掉，
  这条管线就从中消失；`holders` 掉到 0 的瞬间程序已经不再出帧，所以不要把它当成
  "还剩几个人在等"。
- `downlinks` 是 `VideoFanout` 里当前挂着的 sink 数。正常情况下 `downlinks` 与
  `holders` 相等；不相等说明有会话正在建立中（`acquire` 了但还没 `attach`），
  或刚断开还没 `detach` 完。两者长期不等就是泄漏。
- **一个进程里可以有多条管线**，但只有一条 `display: 0`（主屏）。将来接上第二块屏
  就会多一条 `display: 1` 的段；在那之前如果出现 `display != 0`，说明前端请求了
  一个还不存在的显示设备——它会如实失败，而不是把主屏的画面错发过去。

多个会话共享一条管线的硬理由不是省 CPU，而是 **DXGI 桌面复制对同一 output 是独占的**：
第二个 `DuplicateOutput` 会拿到 `DXGI_ERROR_NOT_CURRENTLY_AVAILABLE`，然后被恢复链
降级到 GDI——也就是说第二个页面看到的会是"另一个采集器、另一个编码器、另一份画面"，
而且慢。共享管线是唯一能同时满足"多页面"和"硬编一路"的做法。

### 3.2 `bytes_total` / `avg_mbps` 与 `admission`

**为什么按会话计数，而不是把 `stream.video_mbps` 除以人数。** 共享编码下
`stream.*` 的字节是**程序**的总量，只有扇出层还知道每一帧被写给了哪几条 track。
把总量摊到人头在两种情况立刻算错：一条 track 还没绑定（`write_sample` 返回 `Err`，
那一份根本没出去），以及两个会话的帧率不同（一个刚连上、一个连了半小时）。
所以计数长在 `VideoFanout` 里，每个 sink 一份，`bytes_total` 读的就是它。

- 一个会话在 `Connected` 之前没有下行，字段报 `0` 而不是 `null` —— 但 `avg_mbps`
  也是 `0`，两者一起读表示"还没开始传"，不要单独看。

- `reconnects` 按 `device_id` 计数（首次连接为 0）。它是"这个人"和"一个在重连
  循环里的页面"之间的区别：`connected_s` 一直很小而 `reconnects` 一直涨，就是重连
  风暴的形状——本项目历史上每一次"画面反复闪断"最终都是这么定位的。

**`admission` 是准入状态，不是健康状态。** `live` 可能在 `clients.count` 之上：
名额在 WebSocket 升级**之前**就占住了，而会话要等 `HELLO` 才出现在 `clients` 里。
名额用完时新的升级请求得到：

```http
HTTP/1.1 503 Service Unavailable
Retry-After: 5
{"status":"error","message":"session limit reached (8); retry after a holder disconnects"}
```

为什么不直接断开：101 之后的拒绝只能靠关闭码表达，前端读到的是"连上了又断"，会
当成网络抖动进入重连循环——那恰好是限流想避免的负载。返回 `503 + Retry-After`
让浏览器知道"有人占着"，而不是"服务端坏了"。

`--max-sessions 0` 会被夹到 1，而不是把运维自己锁在门外。

---

## 4. 设备码从哪来

`clients.items[].device_id` 读的是前端 `HELLO` 里的 `client_device_id`，
由 `lib/util.js:deviceId()` 生成并保存在 **localStorage**（键
`<origin+path>_device_id`）。

- 和 `client_tab_id` 的区别：后者在 sessionStorage，关掉标签就没了；前者跨标签、
  跨刷新、跨重启浏览器。用途是"这台机器又来了"而不是"这个标签还在"。
- 它不是硬件指纹：清掉站点数据就换一个，也不携带任何机器信息。
- 私密模式禁掉存储时它会退回 `pageTabId()`，所以字段可能等于 `tab_id`，也可能
  是空串（老页面没有这个字段）——两种都别当成错误。
- 作用域跟这个模块其他键一致（`getStorageAppName()`，origin+pathname），所以同一
  origin 上的另一个部署不会继承这份身份。

---

## 5. 采样成本与新鲜度

主机 CPU / 内存 / 进程表**不在每个请求里重算**，而是缓存在进程内，最长 5 秒
（`health.rs:SAMPLE_EVERY`）。原因很实际：一次全进程扫描在 Windows 上要几十
毫秒，如果一个监控每秒 curl 一次，观测者自己就成了负载。

所以：

- `last_sample_age_ms`、`verdict`、`clients` **永远是实时的**（读内存标志位）。
- `host.*` 与 `processes.*` 可能是 5 秒内的旧数字，`host.sample_age_ms` 会说清楚。
- 进程表用 `ProcessRefreshKind::new().with_cpu()`，故意不刷新 disk usage 和
  user —— 查每个进程的用户 SID 在 Windows 上是一串 LSA 调用，判活探针不需要。

`game_running` 是**名字启发式**：`javaw.exe` / `java.exe` / `minecraft.exe` /
`minecraft.windows.exe`，大小写不敏感。整合包改过进程名就认不出来，所以 `top`
列表放在旁边 —— 那是原始事实，不带判定。

---

## 6. 控制端点（只读面之外的那一小半）

```bash
# 重开采集器与编码器（分辨率/显示模式变了、或怀疑采集卡住时）
curl -X POST http://host:8081/api/v1/control/capture/restart

# 踢掉全部会话（客户端会自己重连）
curl -X POST http://host:8081/api/v1/control/session/kick

# 只踢掉某个会话（id 取自 GET /api/v1/health 的 clients.items[].id）
curl -X POST http://host:8081/api/v1/control/session/kick/2

# 把输入租约交给某个会话（页面之间的控制权移交，见 §5 的 role 说明）
curl -X POST http://host:8081/api/v1/control/session/take/2
```

| 动作 | 正常 | 没有活会话 | 说明 |
|---|---|---|---|
| `capture/restart` | `202` | `409` | 置位 `PipelineHandle::rebuild`，采集循环下一拍走 `InnerOutcome::Rebuild`（与改分辨率同一条路）。多管线时**每一条**都置位 |
| `session/kick` | `200` | `409` | 走信令那条关闭码 4002 的通道，和"被新连接顶掉"同一条路。踢掉的是**全部**会话 |
| `session/kick/<id>` | `200` | `409` | 只踢指定会话。`id` 不存在回 `409`（"踢了 0 个"不算成功——否则运维会以为那个卡住的页面已经被清掉了）。`id` 不是数字回 `400`，**不会**退化成"踢全部" |
| `session/take/<id>` | `200` | `404` / `409` | 把输入租约移交给该会话。`id` 不存在回 `404`；会话还没 attach（或那块屏没有活程序）回 `409`；`id` 不是数字回 `400`。响应里的 `previous` 是原来的持有者 |

为什么需要 `session/take/<id>`：一个人开两个页签看同一块屏时，租约只属于**先**挂上去
的那个。切到另一个页签后，鼠标和键盘都没反应——在接管出现之前，唯一的出路是关掉持有
租约的那个页签。而"最后动鼠标的人赢"不能作为自动策略：两个不同的人看同一块屏时，
它会把控制权从正在打字的人手里抢走。所以移交是**显式**的，由知道自己在做什么的人发起。

为什么要能只踢一个：`session/kick` 是"重启信令"的暴力版本——它会把 controller 一起
清掉，于是重新连上来的第一个页面才有资格当 controller。而"我只想让那个卡住的页面
重连、不要动别人的画面"是更常见的诉求，尤其当同一个人开了两个页面看同一块屏时。

未知动作回 `404` 并列出白名单，而不是静默 `200` —— 一个拼错的
`capture/restat` 悄悄成功，运维会以为重启过了。

---

## 7. 鉴权

**没有**。这是明确决定，不是遗漏。

这份报告确实比 `status` 暴露得多（谁连着、从哪个 IP、什么设备码、进程表），
但本项目的监听面本来就是局域网内不设防，在这里单独加一把和别处不一致的锁只会
制造"以为安全了"的错觉。

正确的做法已经铺好：`Route::Health` → `Endpoint::Health`、`Route::Control` →
`Endpoint::Control`，两者在 `security::check` 里是**两个不同的面**（`http.health`
归在 `is_observational`，`http.control` 不归）。将来装上真策略时，"能探活"和
"能看连接清单"、"能看"和"能踢人"可以分别授权，不需要改这个文件。

细节见 `listener-surface.md`。

---

## 8. 代码位置

| 东西 | 位置 |
|---|---|
| 路由分类 | `crates/stream-server/src/http/mod.rs`（`Route::Health` / `Route::Control`） |
| 端点实现 | `crates/stream-server/src/http/health.rs` |
| 会话身份发布 | `crates/stream-server/src/signaling/websession.rs`（`SessionInfo` / `SessionRegistry`） |
| 准入名额 | `crates/stream-server/src/signaling/websession.rs`（`Admission` / `AdmissionPermit`），在 `http/server.rs` 的 signaling 分支取用 |
| 共享管线与引用计数 | `crates/stream-server/src/pipeline/shared.rs`（`SharedPipeline` / `PipelineRegistry`） |
| 输入租约（controller 判定） | `SharedPipeline::controller_id` / `set_controller`；门禁在 `signaling/websession.rs` 的 `ControlGate` |
| 租约交接与移交 | `signaling/websession.rs`（`SessionRegistry::successor`）+ `http/health.rs`（`session/take/<id>`） |
| 一对多扇出与按 sink 计数 | `crates/stream-server/src/pipeline/fanout.rs`（`VideoFanout` / `SinkId` / `SinkCounters`） |
| 判活时间戳 | `crates/stream-server/src/pipeline/stats.rs`（`last_sample_at`） |
| 重建请求 | `crates/stream-server/src/pipeline/mod.rs`（`request_rebuild`） |
| 设备码 | `addons/selkies-web-core/lib/util.js`（`deviceId`）+ `lib/signaling.js`（hello） |

单测（`cargo test -p stream-server`）：

- 结论判定：idle / streaming / stalled / paused / starting 五种、进程名匹配、采样缓存、错误计数做差
- 多会话形状：`clients.count` 与 `pipelines` 段、无会话时的 JSON
- 按会话计量：`bytes_total` / `frames_total` / `avg_mbps` / `reconnects` 的接线，以及"没有下行时报 0 而不是 null"
- 扇出计数：只统计真正被接受的帧、detach 之后仍可读
- 准入：名额恰好 `max`、并发 16 线程只有 4 个进来、`0` 夹到 `1`
