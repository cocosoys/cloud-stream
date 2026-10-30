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

  "clients": {
    "count": 1,
    "items": [{
      "id": 1,                    // 进程内单调递增的会话号
      "ip": "192.168.1.5",
      "device_id": "9f2c…",       // localStorage 持久 UUID（见 §4）
      "tab_id": "1b7e…",          // sessionStorage，关标签即失效
      "role": "controller",
      "user_agent": "Mozilla/5.0 …",
      "connected_s": 42
    }]
  }
}
```

`count` 永远是 0 或 1：这个服务是一路单会话（新连接踢掉旧的）。

`frames_total` 是**累计值**，不是速率也不是"本秒"：自本次采集循环启动以来的总数。
要速率就对两次响应做差（和 Prometheus 抓 counter 的用法一样）—— 这也是为什么键名
带 `_total` 后缀。`encode_ms` / `pipeline_ms` / `send_ms` 是统计窗口内的均值，窗口
由统计定时器决定；面板关着统计时它们覆盖的时间会更长。

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

# 踢掉当前会话（客户端会自己重连）
curl -X POST http://host:8081/api/v1/control/session/kick
```

| 动作 | 正常 | 没有活会话 | 说明 |
|---|---|---|---|
| `capture/restart` | `202` | `409` | 置位 `PipelineHandle::rebuild`，采集循环下一拍走 `InnerOutcome::Rebuild`（与改分辨率同一条路） |
| `session/kick` | `200` | `409` | 走信令那条关闭码 4002 的通道，和"被新连接顶掉"同一条路 |

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
| 判活时间戳 | `crates/stream-server/src/pipeline/stats.rs`（`last_sample_at`） |
| 重建请求 | `crates/stream-server/src/pipeline/mod.rs`（`request_rebuild`） |
| 设备码 | `addons/selkies-web-core/lib/util.js`（`deviceId`）+ `lib/signaling.js`（hello） |

单测 10 项：idle / streaming / stalled / paused / starting 五种结论、进程名匹配、
采样缓存、错误计数做差、无会话时的 JSON 形状。
