# 游戏模式幽灵光标（#96）

> 一句话：**指针不再由被控端画进画面，而由浏览器自己画**。代价是被控机的指针
> 必须被临时钉成 1:1，退出游戏模式时原样还回去。

## 1. 为什么

两种模式的指针反馈路径不同，这是"全屏模式跟手、游戏模式迟钝"的根因：

| | 全屏（绝对定位） | 游戏模式（指针锁定） |
|---|---|---|
| 输入 | `m,x,y` 绝对坐标 | `m2,dx,dy` 相对增量 |
| 指针谁画 | 浏览器（`cursorDiv` 或 CSS 光标），跟手 0 延迟 | 被控端**合成进视频帧** |
| 端到端延迟 | ≈ 0（本地） | 一帧 + 一次编码 + 网络 + 解码缓冲，50–80 ms |
| 每次移动的成本 | 无 | 整帧 BGRA→I420 + 编码（实测 3–4 ms / 帧） |

实测（#95 日志，5 秒统计行）：`paint=213/5s`（≈43 Hz）而 `m2=425/5s`（≈85 Hz），
`pipe_avg=18.1 ms` 超过 60 fps 的 16.7 ms 预算。也就是说手给了 85 次位移，屏幕
上的指针只更新 43 次 —— 这就是"阶梯感"。

幽灵光标把唯一可见的指针搬回客户端，于是延迟回到本地、整帧重绘不再由指针引起。

## 2. 三件事必须同时成立

只做第一件会漂：画面上的指针和真实指针会越走越远，而且点击永远落在真实位置。

1. **客户端画**（`input.js`）：锁定期间 `cursorDiv` 不再隐藏，位置由**已发出去的**
   线上增量累加（`_ghostMove`）。用线上值而不是事件值，是因为合并、量化、取整都
   发生在那个值上；累加别的会让每一帧的舍入误差都变成漂移。
2. **服务端不合成**：锁定期间发 `SET_NATIVE_CURSOR_RENDERING,0`（与 `p,1` 一起），
   帧里不再有被控端画的光标 —— 否则屏上会有两个光标。
3. **主机 1:1**：发 `ma,1`，服务端调 `dispatch/hostui.rs::pin_pointer`：
   - 关闭「提高指针精确度」（`SPI_SETMOUSE`，三元组 `{MouseSpeed,th1,th2}` 的
     `MouseSpeed` 归 0，**两个阈值原样保留**）；
   - 速度档归 `UNIT_SPEED = 10`（`SPI_SETMOUSESPEED`，倍率正好 1）。

   两个都要：加速度让位移成为手速的非线性函数，速度档把位移乘上倍率（档 15 = 2×）。
   任何一个没关，幽灵与真实指针就会按那个系数分叉。

## 3. 协议

| 行 | 方向 | 含义 |
|---|---|---|
| `ma,1` | 浏览器 → 服务端 | 钉住主机指针（关加速 + 档位 10）。幂等：重复发不覆盖要还原的值 |
| `ma,0` | 浏览器 → 服务端 | 还原进游戏模式前的档位与加速曲线。没钉过时是 no-op |
| `pointer_direct` | 服务端 → 浏览器 | 回执：`{pinned, speed, accel, strategy}`，失败时 `{pinned:null, error}` |

面板不看回执，日志看。三处调 `ma,0` 兜底：解锁（`_pointerLock`）、页面离开
（`_exitPointerLock`，`detach()` 路径）、**会话结束**（`websession::run` 的清理段）。
第三处是必需的：标签页被关掉时前两处都不一定跑得到，而钉住的是**主机的系统设置**。

## 4. 淡出

游戏自己画准心，一个永不消失的箭头会盖在画面上并声称一个指针可能已经不在的位置。

- 连续无**有效位移**（线上增量为 0 不算）达到 `ghost_cursor.idle_secs` 秒 → 淡出
  （`opacity` 180 ms 过渡），下一次有效位移立刻回来。
- 配置：`--ghost-cursor-idle <秒>`，默认 **5**，启动参数**可以不写**；范围 1–600，
  越界夹取（0 会被读成"永不淡出"，正是要避免的行为）。
- 下发：`server_settings.data.settings.ghost_cursor = {value, idle_secs, locked:false}`。
  服务端不跑计时器，只广播这个数；计时在浏览器（`Input.ghostCursorIdleMs`）。

## 5. 面板

"屏幕设置"里新增「游戏模式指针」开关，默认**开**（`ghost_cursor` 条件设置，核心
持久化到 localStorage，面板不存）。

开启时「指针灵敏度」滑块**禁用**并注明原因：钉住期间档位被临时固定在 400 DPI，
此刻调它只会在退出游戏模式时被还原覆盖，等于骗人。想看真实档位就关掉幽灵光标。

## 6. 已知边界

- **漂移不能为零，只能趋近零**。幽灵按常量增益累加，主机（即使关了加速）仍受
  `SPI_SETMOUSESPEED` 与游戏自己的灵敏度影响；解锁那一刻用绝对坐标重新对齐。
- **幽灵被夹在画面内**：主机指针到屏幕边缘就停了，但增量还在发；不夹会让它走出
  窗口再也回不来。
- **主机隐藏光标时不跟着隐藏**：全屏视频、接管 raw input 的游戏把光标设成空，
  幽灵保留最后一张图形 —— 否则"本地画"就白做了。
- **多会话**：钉住是进程级、幂等的；两个页面同时锁定时，一个解锁就会还原。
  单实例单桌面的部署下不会遇到。
- **2.5 鉴权没做**：`ma` 落到 `control.pointer-direct` 面，将来可以单独拒绝。

## 7. 验证

```powershell
# 单元：分类、夹取、形状
cargo test -p stream-server ghost
cargo test -p stream-server settings_describe

# 实机（会真的改这台机器的指针，断言在还原之后，失败也不会把它留在 1:1）
$env:CLOUD_STREAM_LIVE_SPI = '1'; cargo test -p stream-server hostui
```

线上核验：部署后被控端日志应有

```
game-mode ghost pointer: fades after 5s without an effective move
client sets pointer direct -> true
host pointer pinned to 1:1 (speed=10, accel_off=true, strategy=pinned)
...
host pointer released back to the session's own settings
```

`strategy=already` 表示主机本来就是 1:1，这次只记录了要还原的值。
