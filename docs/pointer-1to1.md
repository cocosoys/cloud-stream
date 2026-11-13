# #124 相对指针：从「能动」到 1:1

> 触发物是 `temp/几猫国产3A大作_V1.html` —— 一份单文件 FPS，只认原生 pointer lock 的
> 相对位移。拿它当参照物跑下来，暴露的不是"参数没调好"，而是链路少了两段：
> **没有把被控机指针钉到 1:1**，**没有在指针顶到屏幕边缘时把它拉回来**。
>
> 本文记录五处改动（A–E）、各自的边界，以及"怎么证明它生效了"。
> 上游分析见 `roadmap-future.md §1.2(b)`、`mouse-sensitivity-default.md §8`、
> `ghost-cursor.md §6`。

---

## 0. 契约对照

参考客户端对指针的要求是硬的（数字都是从它的代码里取的，不是估的）：

| # | 约束 | 对服务端的含义 |
| --- | --- | --- |
| 1 | 只读 `movementX/Y`，`f_ = 0.0021` | sens=1.0 时 **2993 counts 转一圈**，即 1 count ≈ 0.120° |
| 2 | 单条位移 > 400 px 直接丢 | 服务端**不能**攒位移批量注入，也不能放大单条增量 |
| 3 | 依赖宿主 pointer lock | 不锁定时仍用 movementX，但受光标边界限制 |
| 4 | 锁定失败退 drag 模式 | 必须有可见光标，且此时不能吃相对位移 |
| 5 | 自己画准星 | 服务端不应再往帧里合成一个 OS 光标 |
| 6 | 菜单四屏全是 DOM 按钮 | 必须能解锁，解锁后指针位置要合理 |

对照 cloud-stream 的原状：#74 已经做到"相对位移原样直通"，但只解决了**缩放**，
没解决**物理结果**——注入出去的位移在到达游戏之前被 Windows 的加速曲线和屏幕
边界各改了一次。

---

## A. 自动钉 1:1（`ma,1` 不再需要前端发）

**做法**：`WinInputSink` 第一次收到 `m2`（或 `p,1`）时自己调
`hostui::pin_pointer()`：关掉"提高指针精确度"、把速度档钉到 10。三条释放路径
都是幂等的——`p,0`、会话 `reset()`、以及"30 秒没有 `m2`"。

**为什么必须**：#74 实测注入侧 `dm/m2 = 9.55 counts/msg`，正落在 Windows 的
`Threshold1=6 / Threshold2=10` 加倍档里（这个比值是"每条消息的 counts"，
也就是 OS 判档的输入；它随手指快慢变，**不能**反过来当 1:1 的判据）。
加速是**非线性**的，玩家无法用灵敏度抵消它：慢移还在 1 倍档、快甩已到 2 倍档。
唯一解就是把曲线关掉。

**边界**：

- `ma,0` 之后本次会话**不再自动钉**（`DIRECT_OFF`）。否则客户端明确说了"不要"，
  下一条 `m2` 就把它推翻，用户看不到任何解释。
- 所有权而不是引用计数：`pin_pointer` 全局只有一个还原点，所以"第一个开口的人"
  负责还原。`hostui::pointer_pinned()` 已经是 `true` 时，本次 sink 不记自己有名，
  也就不会去解别人的钉（与 #101 "只有 controller 能撤销"同一套判断）。
- 钉的是**进程级**设置，多页面同时锁定时一个解锁就还原——和 `ghost-cursor.md §6`
  登记的行为一致，单实例单桌面部署下遇不到。

### 勘误（#124/B 发现，已修）：这一项原本是坏的

`pin_pointer` 依赖的 `hostui::Accel` 把三元组当成 `{MouseSpeed, t1, t2}`，而
`SPI_GETMOUSE` 的实际顺序是 **`{threshold1, threshold2, acceleration}`** —— 开关在
**末位**。三个分量都是小整数，所以读错只是读到另一个"看起来合理"的数，不会报错。

后果不是"效果差一点"，是**方向反了**：

| | 原实现 | 实际写入的含义 |
| --- | --- | --- |
| `accel_off_triple` | `[0, accel[1], accel[2]]` | **t1 被清成 0**（每个位移都进高档），加速开关仍是原值（通常为开） |
| 读回判定 | `triple[0] == 0` | 读到的是 t1，恒为 0 → **报"已关闭"** |

写入和校验错在同一个位置，于是"钉住了"是自证的：日志写 `accel_off=true`，玩家却
仍觉得灵敏度不线性，因为曲线其实被加强了。同一处错误还让 `accel_on()` 在加速**已关**
的机器上返回 `true`，那种机器上 `pin_pointer` 会判定失败并回滚 —— 也就是说，"本来
就 1:1"的机器反而钉不上。

修法：`Accel` 增加具名索引 `ACCEL_T1 / ACCEL_T2 / ACCEL_SWITCH`，全部改用索引访问，
`accel_off_triple` 改成"复制后把 `ACCEL_SWITCH` 置 0"。回归测试
`turning_acceleration_off_keeps_the_thresholds` 原来断言的正是错误行为
（`[1,6,10] → [0,6,10]`），现在断言 `[6,10,1] → [6,10,0]`。

本机实测数据（这次是两边都能读才发现的）：

```
HKCU\Control Panel\Mouse   MouseSpeed=1  Threshold1=6  Threshold2=10
SPI_GETMOUSE               6  10  1        ← 同一份数据，顺序不同
```

`--diagnose` 第 10 节现在会把这两个并排打出来，字段名对齐后再比 —— 直接拿数组比会
得到一个"永久不一致"的假警报。

---

## B. 边缘重定位（`--pointer-recenter`，默认关）

**做法**：注入 `m2` 之前读一次 `GetCursorPos`；若指针在**被串流的那块屏**的任意
边缘 2 px 内（或已经跑到别的屏上），先 `SetCursorPos` 到该屏中心，再注入位移。

**为什么必须**：`m2` 是往真光标上累加的。光标贴到桌面边界之后，后续相对移动
产生的位移为 0，宿主浏览器报的 `movementX` 也就是 0 —— 视角停住。一次 180° 是
~1500 counts，1920 宽的屏**两枪就顶到边**。浏览器里看不到这件事，因为 Chrome
自己的 pointer lock 会把光标夹在窗口中央；远程串流时那层夹子不在。

**为什么默认关**：它动的是**被控机的真光标**。开着的时候本机用户和远端抢同一个
指针，而且在没被 `ClipCursor` 夹住的桌面上那次 warp 是可见的跳动。默认行为保持
插桩前的样子，谁需要谁开。

**边界**：

- 用 `SetCursorPos` 而不是 `SendInput` 绝对定位：后者会合成一个移动**事件**，
  宿主应用把它当成一次真实的甩动 —— 在游戏里就是一次转头。
- 判定是纯函数 `recenter_target()`，"边缘"按 `x + w - 1` 算（最后一个存在的像素），
  差一位的症状是"指针离边还有 1 px 就停了，并且没有任何日志能解释"。
- 退化目标（采集还没开、宽高为 0）直接返回 `None`，绝不返回 `(0, 0)`。

---

## C. 绝对与相对互斥

**做法**：`p,1` 之后如果收到过 `m2`，`m` 不再注入，只记下最后位置；`p,0` 时把那个
位置注入一次。

**为什么必须**：锁定态下页面会同时发两条 —— `m` 来自它那个已被夹到窗口里的光标
位置，`m2` 来自 `movementX/Y`。把 `m` 也应用下去，等于每帧把指针拽到一个和"玩家
正在看的视角"无关的坐标上，累积的偏移全被抹掉，手感是"鼠标往回抖"。

**边界**：只有 `locked && relative` 同时成立才压制。锁定但从没发过位移的页面
（纯绝对操作）走原路，否则它会**完全动不了**。解锁时补一次绝对定位，解决"解锁
瞬间指针跳到角落"。

---

## D. 锁定态不合成 OS 光标

**做法**：`PipelineHandle::cursor_inject_on()` = 前端请求值 **且** 不是
"锁定 + 相对"状态。

**为什么**：锁定态下游戏自己画准星，页面画自己的幽灵指针；服务端再合成一个
OS 光标就是屏幕上**第二个**指针，而且它停在"上一次绝对位置"上，和准星分离。
顺带省掉每帧 3–4 ms 的 BGRA→I420 + 编码（60 fps 预算的 20–24 %，见
`ghost-cursor.md`）。

**边界**：判定用的是 `pointer_is_relative()` 而不是裸 `p,1`。drag 模式（锁定失败）
必须合成光标，否则玩家看不见自己能点哪 —— 那种情况下 `relative` 不会为真
（页面根本没锁上），前端原来的 `SET_NATIVE_CURSOR_RENDERING` 继续说了算。

---

## E. 诊断行补四个数

`pipeline` 的 5 s 统计行原来有 `m=` `m2=` `dm=` `pm=`，现在多四个：

| 字段 | 含义 | 怎么读 |
| --- | --- | --- |
| `pin=` | 1 = 被控机指针当前在 1:1 钉上 | `m2` 在涨而 `pin=0`：钉被拒了（日志有原因）或客户端在 `ma,0` 之后 |
| `ms=` | 因为"锁定 + 相对"被扣下的绝对位移条数 | `m=0` 且 `ms` 在涨 = 正常；两者都涨 = 页面在混发 |
| `clip=` | 边缘重定位次数（窗口内） | 0 说明没到边缘或没开；**host 侧 pointer lock 生效时光标被 `ClipCursor` 关在窗口里，永远到不了边缘 —— 这种会话 `clip=0` 是正确结果，不是故障** |
| `dmax=` | 窗口内单条 `|dx|+|dy|` 的最大值，取的是**客户端到达值**（`sdx` 之前） | 逼近 400 说明客户端的 400 px 上限随时会吞掉合法甩枪 |

`dmax` 是唯一能在服务端看到"客户端把那一下扔了"的证据 —— 它扔的时候服务端
什么都收不到，只剩"玩家说甩不动"。它必须取到达值：取注入值就只能证明自己发了多少。

`dm/m2` **不是** 1:1 的判据。`dm` 累加的是注入意图（`|sdx|+|sdy|`），`m2` 是消息数，
所以这个比值就是"每条消息的平均 counts"，正常落在手速量级（10~20）。加速发生在
`SendInput` 之后、由 OS 施加到光标上，服务端没有任何计数器能看到那个乘数 ——
要证明钉住了，只能看 `pin=1` + 日志里的 `accel_off=true`，以及事后 `--diagnose`
第 10 节的 `runtime`。

---

## 开关与默认值

| 项 | 默认 | 入口 |
| --- | --- | --- |
| 自动 1:1 钉 | **开** | 无参数；`ma,0` 可关，`ma,1` 可显式接管 |
| 锁定态压制 `m` | **开** | 无参数（`p` 消息驱动） |
| 锁定态不合成光标 | **开** | 无参数（`p` + `m2` 驱动） |
| 边缘重定位 | **关** | `--pointer-recenter` / `pointer_recenter = true` |

---

## 怎么当场确认（装完机就走这三步）

`--diagnose` 第 10 节就是为了回答"钉没钉上"而加的 —— 前两步不需要连客户端：

```text
stream-server.exe --diagnose
```

本机实跑的一段（关键几行）：

```text
==== 10. pointer 1:1 baseline ====
speed      step 10 = 400 DPI（基准 400 DPI 的 1x）
accel      ON，thresholds 6 / 10（顺序：t1, t2, switch）
verdict    NOT 1:1 —— 加速曲线开着

-- 持久化值（HKCU\Control Panel\Mouse，进程内读，不经过 reg.exe）--
profile    MouseSpeed=1  Threshold1=6  Threshold2=10
runtime    Threshold1=6  Threshold2=10  switch=1
           两者是同一份数据 —— SPI 把开关放在末位、阈值放在前面 …
-- 配置开关 --
pointer_recenter  off —— 边缘重定位没开
-- 最近一次钉住的结果（stream-server.log，最后 6 条）--
没有记录 —— 没有任何会话在相对模式下跑过，或日志已经轮转
```

怎么读：

| 看到 | 含义 |
| --- | --- |
| `verdict NOT 1:1` | **空闲时是正常的**。钉只在游戏模式会话期间存在，锁一解开就还原 |
| `verdict UNKNOWN` | 这个会话读不到设置（会话 0 / 没有桌面）。这种环境里 pin 必然失败，先查第 5 节的 `LogonType` |
| 日志 `auto-pinned … strategy=pinned` | 钉成功，而且真的写了值 |
| 日志 `strategy=already` | 本来就 1:1（加速已经关着），没有写 |
| 日志 `could not pin` | 真故障：没有交互桌面，或鼠标驱动在覆盖（`runtime` 与 `profile` 会显示不一致） |
| 日志 `host pointer not restored` | 还原失败 —— 这台桌面会停在"加速关 / 档位 10"，要人工改回 |
| `runtime` 与 `profile` 不一致 | 有程序在改会话值，或上一次 pin 没还原（进程被杀） |

三步：

1. **装完机先跑一次**。`verdict` 是 `NOT 1:1` 属正常；`UNKNOWN` 要修。
2. **开一次游戏模式会话**，转几圈，再跑一次。日志应出现 `auto-pinned`；开了
   `--pointer-recenter` 时统计行还会有 `clip=`。
3. **结束后再跑一次**。日志应出现 `host pointer released`，且 `runtime` 与 `profile`
   回到一致 —— 这一条才是"还原真的生效"的证据，比手感可靠。

第 3 步为什么重要：pin 是**改系统设置**。进程在钉住期间被杀（任务管理器结束、蓝屏），
还原就不会发生，这台桌面会一直停在"加速关、档位 10"。第 10 节把 `runtime` 和
`profile` 并排放，正是为了让这种事在一份文件里露出来，而不是等下一个玩家说"鼠标怪怪的"。

---

## 验证状态

**已做（单测，440 passed / 1 ignored）**：

- `recenter_target`：中间 / 四条边 / 远边差一位 / 负原点（副屏在左）/ 退化目标。
- `should_suppress_abs`：四种组合的真值表。
- `--pointer-recenter`：默认关、出现即开、与其它参数顺序无关。
- `pointer_recenter` 的 TOML 回环（`--print-config` 贴回去逐字段相等）。
- `accel_off_triple`：清开关不动阈值（`[6,10,1] → [6,10,0]`）、已关时原样、开关索引为 2。
- `baseline`：三态判定 + "只读到一半仍算不是 1:1" + 两种排列的换算。
- `--diagnose` 第 10 节：倍率文本、日志尾部筛选、`pointer_recenter` 从配置读。

**实机**：

- `CLOUD_STREAM_LIVE_SPI=1 cargo test -p stream-server --lib -- live_pin` 通过：
  pin → 读回 `speed=10 / switch=0` → release → 设置完整复原（本机
  `MouseSpeed=1 / t1=6 / t2=10 / MouseSensitivity=10` 前后一致）。
- `--diagnose` 第 10 节在本机跑通，`profile` 与 `runtime` 判为同一份数据。

### 2026-11-13 实测（VMware 主机 / 软件编码 / 1920×1080 → 1920×1080 恒等映射）

一次真实的 pointer-lock 会话，约 25 s，四行日志齐全：

```text
pointer lock -> on (relative motion will suppress absolute `m`)
host pointer auto-pinned to 1:1 for relative input (speed=10, accel_off=true, strategy=pinned)
pointer lock -> off (relative motion will no longer suppress absolute `m`)
host pointer released (pointer lock released)
```

对应五个 5 s 统计段：`m2=434→497→502→534→289`（约 100 msg/s）、
`dm=8192→7813→8286→6802→2696`、`dmax=105→52→52→67→27`、`pin=1` 全程、`clip=0`。

| 项 | 判定 | 依据 |
| --- | --- | --- |
| A 自动钉 1:1 | **通过** | `strategy=pinned` + `accel_off=true`，`pin=1` 五个段，`p,0` 后 `released` |
| E `dmax` | **通过** | 峰值 105，距 400 上限还有 4 倍余量 —— 这一轮客户端没有合并发送 |
| C 压制绝对 `m` | **未覆盖** | 客户端锁定后自己就不发 `m` 了（`m` 归 0、`ms` 恒 0），压制路径没被走过 |
| D 锁定态不合成光标 | **无独立证据** | 页面自己先发了 `SET_NATIVE_CURSOR_RENDERING,0`（日志 `off (frontend native)`），D 只是同意它 |
| B 边缘重定位 | **未触发** | 配置关；且 `dm` 每段稳定在 7000~8300 说明光标一直在有效移动，没被 OS 丢弃 |

B 这一行值得单独记：**host 侧 pointer lock 生效时，光标被 `ClipCursor` 关在窗口内，
永远到不了屏幕边缘，所以 B 永远不会触发**。本次 `dm` 持续为正就是证据 ——
如果撞边，OS 会丢弃位移，`m2` 继续涨而 `dm` 归零。B 只在 host 侧没锁上
（前端 `requestPointerLock` 失败、窗口失焦）而客户端仍在发 `m2` 时才是兜底。

**仍未覆盖**：

1. B 真的触发一次：需要一个 host 侧锁不上的环境（或临时把窗口切到非全屏），
   看 `clip=` > 0，并确认贴边时视角不跳（`SetCursorPos` 不该合成移动事件）。
2. C：需要一个锁定态仍发 `m` 的客户端；`ms=` 应该开始涨而 `m` 不动。
3. D：需要前端不发 `SET_NATIVE_CURSOR_RENDERING,0` 的会话（当前没有 CLI 能强制
   `cursor_inject`，只能改前端）。
4. 退出后的系统设置复原：日志的 `released` 只证明调用了 `unpin_pointer`，
   真实复原要查注册表（`--diagnose` 第 10 节的 `profile` 与 `runtime` 一致）。

---

## 前端仍需做的（不在本仓库）

- 锁定失败（`pointerlockerror`）时回退到绝对模式**并显示光标**；
- backlog 高时**丢弃旧位移**而不是排队重放（`roadmap-future.md §1.2(a)`）；
- Esc 解锁流程里补一次 `ma,0`（服务端在 `p,0` 也会自动还原，但显式更清楚）。
