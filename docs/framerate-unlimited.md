# 帧率上限与「不限制」模式

控制面板里帧率滑块原来只能到 60 FPS，因为 `dispatch/output.rs` 的
`server_settings` 把能力公告写死成 `min:10, max:60`。改成 240 之后，滑块能覆盖
144 / 165 / 240 Hz 的显示器；再多就没有意义了——真正想要的是**不要任何上限**，
所以滑块最右端多出一档「不限制」。

## 1 线上契约：`framerate: 0` 就是不限制

`0` 不是一个帧率，永远不要拿它去做 `1/fps`、去和 `min/max` 比、去乘码率系数。
它在链路上的含义只有一句：**不再按定时器走拍，改为每来一帧画面就编一帧。**

| 位置 | 行为 |
|---|---|
| `server_settings` | `framerate.max = 240`，并新增 `"uncapped": true` |
| `StreamSettings::set_fps(0)` | 存 0（其它值 clamp 到 `1..=240`） |
| `StreamSettings::spf()` | 返回 `Duration::ZERO` |
| `StreamSettings::sample_spf()` | 1 ms（只给不知道实测间隔的调用方兜底） |
| `StreamSettings::budget_fps()` | 240（码率预算与 IVF 头用它，不把不限当成 0 fps） |
| `settle_beat` | `spf == 0` 时阻塞在采集邮箱上（见 §2） |
| 前端 | 存储字符串 `'0'`，`parseFloat` 能吃能吐，`requestedFramerate` 原样返回 0 |

面板只在服务端公告了 `uncapped: true` 时才给出这一档：轨道末尾接一个服务端会
悄悄 clamp 掉的位置，比没有这一档更糟。

## 2 为什么不 sleep 0 —— 采集邮箱才是节拍器

`settle_beat` 原本是「滑窗截止时间 + 最后 1.5 ms 自旋」。`spf = 0` 会让它每拍都
判定已过期、立刻返回，于是整个 beat 循环变成死循环：静态桌面时 `worker.take()`
永远是 `None`，循环以几十万次每秒空转，把一个核吃满。

所以 `spf == 0` 走另一条路：`CaptureWorker::wait_frame(8ms)`，在采集线程发布帧的
那个 condvar 上等。

- 桌面**有变化**：采集线程 `publish()` 唤醒，唤醒就是新帧到达本身，不是超时。
  拍频 = 显示器刷新率，这是这条链路能给的最低延迟。
- 桌面**静止**：8 ms 超时后走原有的空闲路径（`REPEAT_ENCODE_MAX` 限流），
  一个核的占用可以忽略。
- 停止、门控关闭、采集卡死：最迟 8 ms 被发现。

门控关闭（`!handle.is_on()`）的分支要特别注意：邮箱里还留着关门那一刻的帧，
`wait_frame` 会立刻返回，于是那里改成 `thread::sleep(8ms)`，否则照样空转。

## 3 RTP 时长：不限模式用实测间隔

`write_sample(data, duration)` 的 `duration` 是解码器用的 delta。给 0 会让每个帧带
同一个时间戳，浏览器把后面的都当重复帧；给固定 1 ms 则让媒体时钟以真实时间的
千分之一前进。上调的转发器因此按上一拍的 `sample.beat` 到这一拍的间隔来填，
并 clamp 在 `0.5 ms .. 200 ms`。

## 4 面板改动

- 帧率滑块：最右一档在轨道上画成 `∞`、标签右侧注记「不限制」，输入框留空即
  该档位（占位符就是 `∞`）。
- 所有圆点滑块统一成 `SliderField`（`components/SliderField.jsx` + `Overlay.css`
  里的 `.slider-field` 段）：名字在左，右侧是可直接填写的数值输入框与单位，
  轨道在下。稀疏轨道够不到的数值，从输入框精确给。
- 指针灵敏度的标签改用中文词条（`sections.screen.pointerGainName`）。

## 5 验收

1. 面板 → 屏幕/视频，帧率滑块拖到最右 → 标签右侧出现「不限制」、输入框变空。
2. 被控机日志出现 `client requested unlimited: cadence re-based`。
3. 静止桌面时 `pipeline ...` 行里 `loop=` 落在 100 上下的空闲拍频，`new=` 为 0；
   动鼠标时 `new=` 跟随显示刷新率。
4. 清空输入框后仍在「不限制」；填 144 立刻回到 144。
