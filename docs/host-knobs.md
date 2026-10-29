# 被控端系统旋钮：指针 DPI 与界面缩放（#90 / #91）

面板上「指针灵敏度」「界面缩放」改的是**被控机自己**的设置，不是流参数。所以
它们不在 `StreamSettings` 里（pipeline 既不读也不 clamp 它们），回执也不走
`server_settings`，而是各自一条消息。

同一次改动还修了三处「面板说的和实际跑的不一致」，见 §5。

## 1. 指针灵敏度 = 系统指针速度，不是客户端增益

鼠标 DPI 是硬件属性（传感器每英寸多少计数），软件改不了。Windows 能设的是
**指针速度倍率** `SPI_SETMOUSESPEED`，共 20 档（1/32 … 7/2）。面板把它按
**400 DPI 基线**（第 10 档 = 1x = 400）报给用户，所以用户填的数就是被控机实际
在跑的数。

以前这里是客户端 `pointerGain`：只对指针锁定后的相对增量乘一个系数。问题有两层：

- 它只作用于指针锁定的那一条路径，桌面、启动器、游戏内的鼠标全是另一个手感；
- 它治的是症状。真正的偏差来自画面缩放（见 `host-display-control.md`），
  在客户端补系数只是把 1.12 的缩放藏起来。

`pointerGain` 已删除（`lib/input.js`），`_relativeToServer()` 现在原样透传。

指令：`md,<dpi>`。回执：

```json
{"type":"mouse_dpi","requested":777,"applied":800,"speed":15,"exact":false}
```

`applied` 是回读的实际档位，`exact:false` 表示 Windows 只有这些档。面板把滑块的
存储值改成 `applied` —— 存用户填的 777 会在下一次 `server_settings` 同步时
把 800 顶回 777。

## 2. 界面缩放

`dd,<dpi>`（96 = 100%）。背后是 `SPI_SETLOGICALDPIOVERRIDE`，SDK 标着
"Do not use"，而且 `uiParam` 有两种流传的读法（绝对索引 / 相对推荐值）。所以
`apply_scale` 先按绝对索引写、回读，没落地再试相对形式，回执带 `strategy`
（`absolute` / `relative` / `already`）。

**注意历史 bug**：前端原来发的是 `s,<dpi>`，而 `s` 在服务端的忽略组里 ——
每一次缩放选择都死在网线上，且没有任何日志。现在发 `dd`。

## 3. 为什么必须在服务进程里调用

`SystemParametersInfo` 是**按会话**生效的。服务跑在交互会话（session 1），
SSH 登录的进程在 session 0，那里 `SPI_GETLOGICALDPIOVERRIDE` 直接失败。所以
这两个调用只能在被控端进程内做（`spawn_blocking`，不阻塞 select 循环），
不能在部署脚本里"顺手设一下"。

## 4. 面板里两个控件的数据来源

`server_settings` 新增：

```json
"scaling_dpi": {"value":"96","allowed":["96","120",...],"locked":false},
"mouse_dpi":   {"value":400,"allowed":[13,25,...,1400],"base":400}
```

- `scaling_dpi.value` 是**回读**的，不是记住的：新浏览器打开时显示的是桌面
  真实所处的缩放。
- `mouse_dpi.allowed` 是 20 档速度换算成 DPI。面板把 100 以下的四档滤掉
  （13/25/50/100 —— 25 DPI 的指针没人用），链路起点仍是服务端给的列表，
  所以滑块不会出现"拖到某个档什么也没发生"。
- `scaling_dpi` 曾整块缺失，而面板的 `<select>` 选项就是
  `scaling_dpi.allowed`，于是下拉框是个空的、但可点的控件。

## 5. #91：面板说的事必须和实际跑的一样

三处「面板在说自己的默认值」：

| 位置 | 以前 | 原因 | 现在 |
|---|---|---|---|
| 视频设置 · 编码器 | H.264 | `server_settings` 没有 `encoder`，面板用自己的本地默认 | `allowed:["vp8enc"]`、`locked`，行仍在但只读，显示 VP8 |
| 视频设置 · 码率控制模式 | CBR/CRF 可选 | 同上，后端根本没有 CRF 路径 | `allowed:["cbr"]`，整行隐藏 |
| 音频设置 · 音频码率 | 灰的，拖不动 | 面板按 `allowed.length <= 1` 判禁用，而**这个键根本不存在**，长度算 0 | 宣告 `allowed:[32000..128000]` 与回读的 `value` |

编码器那一格的矛盾最直观：统计信息的「编码」块读 `stream_info`（`libvpx` /
`VP8`，由后端硬写，因为 pipeline 只构造 VP8），而视频设置读
`server_settings.encoder`（不存在 → 本地默认 `h264enc`）。两个答案对一个
问题，都不是服务端给的。

音频码率的链路本来就是通的：面板 → core → `ab,<bps>` → `audio::set_bitrate_bps`
→ Opus `set_bitrate`（不重启编码器）。**只是滑块被灰掉了，用户点不到**。
现在 `SETTINGS` 批处理也接受 `audio_bitrate`（`picked` 走这条路，不能只认 `ab,`）。

`allowed` 只到 128000：Opus 上限 510k，但 `set_bitrate_bps` 把上限压在 128k，
再高的档位是"点了会静默变成 128k"的谎话。

## 6. Turbo 在这台服务端上是什么

`video_streaming_mode`（Turbo）的原始语义是关掉 VNC 式增量刷新，改成整帧连续
编码。**本服务端没有增量刷新这条路径** —— pipeline 每一拍要么抓新帧整帧编码，
要么重复编码上一帧的 YUV —— 所以这个开关不改变画面，也不改变码率。

它真正决定的是面板对「静态区域优化」（`use_paint_over_quality`）的建议：
Turbo 开启时建议关闭。而 paint-over 在服务端同样没有实现。

面板因此在开关下面写明了这件事，而不是只留一句上游的
"encodes all frames like a traditional video encoder"（在这台机器上等于什么都没说）。

要让 Turbo 真的有效果，需要服务端实现按区域重绘（增量编码），那是一个独立的
大改；或者把它改接 `--repeat-every`（静态画面重复帧的节流），代价是语义变了：
Turbo 会变成"静态画面也保持满帧发送"。

## 7. 右上角统计面板改为跟随「在画面上显示」开关

面板（`selkies-web-core/lib/stats-hud.js`）原来有三个入口：自己的 localStorage
默认值（默认还就是开）、F3 切换、F4 切麦克风。于是「统计信息 → 在画面上显示」
这个开关说关，面板照样挂在右上角 —— 两个开关管同一件事，其中一个说话没人听。

现在只有一个来源：

- **开关**（`Sidebar.jsx`，`stats_strip`）是唯一裁判，状态变化和挂载时都
  post `{type:"statsHud", open}`，core 里唯一监听它的是面板自己。
- 面板不再写自己的 `cloudstream_stats_hud` 存储键。留一份"我认为应该是开"
  的记忆，只会和开关在下一次刷新时打架，输的是最后一个写的那个。
- **F3 / F4 撤掉**：`statsHud` 之外没有第二条进场路径，F3 也就能重新作为
  普通按键发到被控端。麦克风仍有面板上的按钮（指针锁定时面板是唯一能点到
  它们的地方，所以按钮留着）。
- 面板**不再自己 post `statsOpen`**。订阅由前端的 `holdStats` 计数器持有
  （条带和统计区块各算一份）；面板再发一次，关面板就会把统计区块还在看的
  数字一起掐掉。
