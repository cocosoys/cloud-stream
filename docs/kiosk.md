# 输入锁定（kiosk）

> 目标一句话：**从网页进来的输入只落在游戏里，出不去；而且一定有办法解开。**

代码在 [`crates/stream-server/src/kiosk/`](../crates/stream-server/src/kiosk/)。

## 一、为什么需要它

不是"体验优化"，是两件具体的事凑在一起：

1. **串流采的是整个显示器。** 采集走 DXGI `IDXGIOutput1::DuplicateOutput`（桌面复制），
   它拿的是某个输出上的整块桌面，不是某个窗口。
2. **全屏的 MC 一失去焦点就自己最小化**，窗口坐标变成 `-32000,-32000`，GL 上下文
   在反复 `SW_RESTORE` 之后会直接崩（`No OpenGL context found in the current thread`）。

于是玩家在网页里按一下 `Alt+Tab`：游戏退到后面 → 自我最小化 → 浏览器里变成一张
**桌面的画面**，而玩家再也没法把它叫回来（他按的键全落在桌面上）。

`F11` 的真全屏是可以用的 —— 采集能出画面（2026-11-17 实测）。但"全屏"只解决了
"窗口多大"，没解决"焦点会不会被抢走"。锁定解决的是后者。

## 二、三层，各管一段

| 层 | 代码 | 拦什么 | 为什么只有它能拦 |
|---|---|---|---|
| 注入端 | `guard::KeyGuard` + `dispatch::win_input` | 浏览器上报的危险组合 | 键在**传输之后、`SendInput` 之前**就没了，系统没机会反应 |
| 钩子 | `hook` | 物理键盘、别的进程的注入 | `WH_KEYBOARD_LL` 是系统级，只有它看得见不走我们消息通道的键 |
| 策略 | `policy` | `Win` 组合、任务管理器 | 注册表层，钩子挡不住的地方（`Win+D`、`Win+R` 之类由系统直接处理） |

### 判据只有一份

两套键码（浏览器的 X11 keysym、Windows 的 `VK_*`）先翻译成同一个 `Chord`，
再用同一个 `judge` 判。两处各写一遍规则必然会走偏：一处拦住 `Alt+Tab`、
另一处漏掉 `Alt+Esc`，而这种偏差只在玩家真按了那个键时才暴露。

| 键 | 锁定时的行为 | 理由 |
|---|---|---|
| `Win`（左/右） | **吞** | 一按下开始菜单就盖住整个桌面 |
| `Win` 按住期间的任何键 | **吞** | 系统的下一次按键会被解释成 Win 组合 |
| `Alt+Tab` / `Alt+Esc` / `Alt+F4` / `Alt+Space` / `Alt+Enter` | **吞** | 全部是"离开游戏"或"关掉游戏" |
| `Ctrl+Esc` / `Ctrl+Shift+Esc` | **吞** | 开始菜单 / 任务管理器 |
| `F11` | **放行** | 它是玩家唯一想要的全屏键 |
| `Tab` / `Esc` / `Alt` / `Ctrl` / `Shift` 单独按 | **放行** | 游戏内正常按键（MC 里 Tab 是玩家列表） |

### 钩子必须放行 `LLKHF_INJECTED`

这是整个模块最容易写错的一行。我们自己的游戏按键也是 `SendInput` 注入的，在低级
钩子里同样以"注入事件"的身份出现。如果连注入事件也判一遍，那么玩家按 `Alt`（放行）
+ 按 `Tab`（`Alt+Tab` 判定成立）时，**注入端已经放行的 Tab 会被钩子再吃一次** ——
表现是"游戏里键盘时灵时不灵"，且只在按过修饰键之后出现，极难复现。

分工是硬边界：**注入的键由注入端判，物理的键由钩子判**。

### `Win` 的按下状态由钩子自己记

钩子把 `Win` 的 key-down 吞掉之后，系统层根本没收到这个键，`GetAsyncKeyState(VK_LWIN)`
靠不住。所以 `WIN_HELD` 是自己维护的，用来继续吞掉 `Win` 按住期间的后续按键。

## 三、配置

```toml
kiosk = true               # 总开关；关着时所有 kiosk/* 端点答 404
kiosk_autolock = false     # 游戏 start 成功后自动上锁
kiosk_policy = true        # 注册表策略（NoWinKeys / DisableTaskMgr）
kiosk_hook = true          # 低级键盘钩子
kiosk_password = ""        # 网页解锁口令的哈希；空 = 不提供网页解锁
```

命令行同名开关：`--kiosk` / `--no-kiosk`、`--kiosk-autolock`、`--no-kiosk-hook` 等。

`kiosk_policy` 与 `kiosk_hook` 默认是 **true**，`kiosk` 默认 **false**：打开总开关
之后立刻就是完整的 2 档锁定，不需要再记两个开关。

生成口令哈希：

```
stream-server.exe --kiosk-hash 我的口令
# 或不让它进 shell 历史：
echo 我的口令 | stream-server.exe --kiosk-hash
```

输出形如 `sha256i:10000:<盐>:<哈希>`（10 000 轮 SHA-256 + 每台机器不同的盐）。
存哈希不存明文：运维拷一份 `stream-config.toml` 去排障、贴进聊天窗口，里面的字串
不该等于口令。配置里填了明显不是哈希的值时，加载会 warn 并忽略 —— 否则网页解锁
会永远失败，而失败的样子是"口令不对"，没人会想到是格式问题。

## 四、接口

| 动作 | 说明 |
|---|---|
| `GET /api/v1/health` → `kiosk` | `armed / since_unix / reason / enabled / policy / hook / hook_installed / password_set / policy_pending` |
| `POST /api/v1/control/kiosk/lock` | 上锁。不校验口令（能点这个按钮的人已经在页面里，页面本身要令牌） |
| `POST /api/v1/control/kiosk/unlock` | 解锁。body `{"password":"..."}`。口令错 → 403 |
| `POST /api/v1/control/kiosk/status` | 同上，给只会 POST 的客户端 |

`password_set: false` 时 `kiosk/unlock` 答 403 并说明替代方案（`--unlock` 或
`Ctrl+Alt+Del`）—— 面板据此把那个按钮画成禁用，而不是让人一遍遍试口令。

### 与游戏生命周期的联动

| 事件 | 行为 |
|---|---|
| `game/start` 成功 | `kiosk_autolock = true` 时上锁（`reason: "autolock"`）。**在游戏起来之后**，不是之前 |
| `game/stop` 成功 | 只解 `reason == "autolock"` 的锁；手工锁（`reason: "web"`）不动 |

这条不对称是有意的：`autolock` 锁上的桌面，游戏一停就该还回去（否则运维每次都要记得
多点一下解锁）；而**手工锁是有人明确要求的**，不该因为一次"停止游戏"就失效 —— 否则
那个按钮就成了绕过锁定的后门。

## 五、解锁的四个出口

锁住的机器最怕"锁死了"，所以出口有四个且互相独立：

1. **`Ctrl+Alt+Del`** —— SAS，任何用户态手段都拦不住，操作系统自己处理；
2. **`--unlock`** —— 命令行，ssh 进去就能跑；
3. **网页口令** —— `POST control/kiosk/unlock`；
4. **进程重启** —— 启动时 `recover()` 把上一轮残留的策略还原。

第 4 条是兜底里的兜底，也是**还原记录必须落盘**的唯一理由：

```
读原值 → 写 games\runtime\kiosk-policy.json → 改注册表
```

顺序是死的。如果先改注册表再写记录，被 `taskkill /F` 的那一瞬间原值就永久丢了 ——
那台机器会带着"任务管理器被禁用"活到下一次有人手工改回来。记录写失败就一条都不改。
`engage()` 一开始还会先 `restore()` 一次：上一次的残留必须先还回去，否则这次读到的
"原值"是我们上次写进去的 `1`。

### `--unlock` 用的是文件，不是命名事件

第一版用 `Local\cloud-stream-kiosk-unlock` 命名事件，在 `.128` 上**直接失败**：

> SSH 进来的进程在 **session 0**，而计划任务拉起的服务在 **session 1**（console）。
> `Local\` 命名空间按会话隔离，两边看不见彼此。

也就是说，恰好在这条"最需要它"的通道上它是坏的。改成落一个
`games\runtime\kiosk-unlock.req` 文件、服务每 400 ms 看一眼：同一个用户、同一个
安装目录，跨会话跨桌面都成立。代价是 400 ms 的延迟，换来的是"它一定看得见"。

```
UNLOCK signaled=true restored=2
```

`signaled` = 请求文件落下去了（服务会在 400 ms 内 disarm），`restored` = 这次调用
自己还原了几条策略（服务不在时它独自完成还原；服务在时两边都是幂等的）。

## 六、`.128` 实测（2026-11-17）

| 项 | 结果 |
|---|---|
| 服务启动 | 钩子装上（`hook_installed=true`），策略与口令就绪 |
| `POST kiosk/lock` | `armed=true`；`NoWinKeys=1`、`DisableTaskMgr=1` 写入；`kiosk-policy.json` 生成（`existed:false` × 2） |
| 错口令 | 403 |
| 对口令 | 200，`armed=false`，两个注册表值被删，记录文件删除 |
| `--unlock`（第一版，命名事件） | `signaled=false restored=2` → **暴露 session 隔离问题** |
| `--unlock`（文件信号） | `signaled=true restored=2`，服务 3 秒内 `armed=false` |
| 强杀残留 | `taskkill /F` 后 `DisableTaskMgr` 仍是 1、记录文件仍在 → 重启服务 → 日志 `启动时还原了上一轮残留的 2 条策略`，注册表已空 |
| `kiosk_autolock = true` + `game/start` | 游戏 `running` 后 `armed=true reason=autolock` |
| `game/stop`（autolock 锁着） | 1.7 s（`killed-window-gone`），随后**自动解锁**，`armed=false policy_pending=false` |
| 手工锁 + 停游戏 | 不解锁（`reason=web` 保持） |
| 单测 | 473 通过（新增 23：判据 12、策略 4、口令 7） |

未验证：

* **物理键盘** 在锁定时是否真的按不动 —— 需要在 VMware 控制台里手按 `Alt+Tab`。
  自动化做不到：脚本发的键都是 `INJECTED`，按设计会被放行。
* 锁定状态下**游戏内的注入**是否正常（判据上是对的：注入事件一律放行，但这仍是一条
  需要人眼确认的路径）。
* 锁定状态下**按物理键**的拦截效果（同上，需要人在 VMware 控制台里试）。

## 七、运维剧本

**给一台机器开锁定**

```toml
kiosk = true
kiosk_autolock = true
kiosk_password = "<--kiosk-hash 的输出>"
```

重启服务 → 面板上点"开始游戏" → 游戏进程起来后自动上锁；点"停止游戏"自动解锁。

**没配口令也能用**

`autolock` 与 `kiosk_password` 是两件独立的事。没配口令时网页上解不开，但 `--unlock`
与"停游戏"两条路都还在 —— 也就是说，一个只有 `--kiosk --kiosk-autolock` 的裸配置
不会把那台机器锁死。真要锁死，得同时把 `Ctrl+Alt+Del` 之外的东西都拆掉。

**现场进不去了**

1. `Ctrl+Alt+Del`（一定有效）；
2. 或 `ssh 用户@机器 "C:\Program Files\cloud-stream\stream-server.exe" --unlock`；
3. 或重启服务（`schtasks /End` + `/Run`）—— `recover()` 会还原策略。

**排障：键盘没反应，怀疑是锁定**

依次关掉两层，用排除法定位：

```toml
kiosk_hook = false    # 先关钩子
kiosk_policy = false  # 再关策略
```

如果关掉钩子后恢复正常，问题在 `hook::callback`（大概率是 `LLKHF_INJECTED` 判断）；
如果关掉策略才恢复，看注册表那两个值有没有被别的东西改回去。

**为什么不能把 `stream-server` 注册成服务**

钩子与窗口都依赖"交互式会话"。注册成 Windows 服务 = session 0 = 没有可见桌面、
没有键盘输入队列，钩子装了也收不到回调。必须保持"交互式计划任务"形态（见
[docs/operations-run.md](operations-run.md) 第 3 节）。
