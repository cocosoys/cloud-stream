# 游戏托管：`game` 模块（S1 + S2）

日期：2026-11-17　环境：被控 `.128`（Windows X-Lite / Win11、4 vCPU、VMware SVGA 3D）

这篇记的是"谁把游戏拉起来、它放在哪、怎么停"这三件事的现状。之前这一层是
`ssh → schtasks /IT → powershell → wscript → javaw` 五层链条，全部塞在
`C:\mc\start-mc.ps1` 一个 443 行的脚本里；现在是 `stream-server` 的一个模块。

## 一、为什么搬进 stream-server

五层链条里有四层在补偿同一件事：**Windows 会给 console 子系统的子进程配一个控制台，
而那个控制台的关闭会连带杀死游戏**（`Ctrl-Close`）。最后靠 `wscript`（GUI 宿主、
没有控制台）绕过去。

`stream-server` 自己是 GUI 子系统（`build.rs` + `main.rs` 的 stdio 兜底），它 spawn
出来的 `javaw` 没有控制台祖先 —— 那条绕法连同全部理由一起消失。顺带拿回投递方式
永远拿不到的三样东西：pid、退出码、活的子进程句柄。

## 二、目录

```text
<exe 所在目录>\games\            可用 config.toml 的 game_root 改（绝对/相对均可）
  clients\1.12.2-forge\          只读客户端（versions / libraries / assets）
  saves\default\                 --gameDir：存档、options.txt、logs、mods*
  instances\mc-1.12.2.json       实例描述
  runtime\                       cmdline.txt、hs_err_%p.log
```

`*` **`mods/`、`config/`、`resourcepacks/` 在 1.12.2 里跟着 `--gameDir` 走，不跟着
classpath 走。** 第一次把 `gameDir` 挪到 `saves\default` 后，游戏只加载了 4 个 mod
（CustomNPCs 消失了）。`toolchain\seed-games.ps1` 用目录联接（Junction）把这三个目录
接回客户端的原位置，客户端保持"唯一真相"，实例这边零拷贝。

## 三、实例描述（`instances\<id>.json`）

```jsonc
{
  "schema": 1,
  "name": "Minecraft 1.12.2 Forge",
  "client_dir": "C:\\Users\\Admin\\Desktop\\.minecraft",  // 绝对或相对 games 根
  "saves_dir": "saves\\default",
  "java": { "path": "C:\\mc\\jre8\\bin\\javaw.exe", "heap_mb": 2048, "jvm_args": [] },
  "game": { "version": "1.12.2-Forge_14.23.5.2864",
            "main_class": "net.minecraft.launchwrapper.Launch",
            "tweak_class": "net.minecraftforge.fml.common.launcher.FMLTweaker",
            "player": "CloudPlayer", "game_args": [] },
  "display": { "model": "windowed", "width": 0, "height": 0, "x": 0, "y": 0 },
  "lifecycle": { "stop_timeout_sec": 15, "restart_max": 0, "restart_backoff_sec": 5 }
}
```

* `display.width/height = 0` 表示**跟随串流分辨率**（`config.width/height`）。采集和
  游戏用同一对数，编码前不会多一次缩放。
* `display.model`：`exclusive`（写 `options.txt: fullscreen:true`）或 `windowed`。
  没有 `borderless` —— 实测 MC 1.12.2 按 F11 切全屏后 DXGI 桌面复制仍能出画面，
  再加一层改窗口样式的 hack 只会引入"画面正常但鼠标偏移"这类新故障。
* **未知字段原样保留**（`unknown` + serde flatten）。这是"预留冗余"的落点：新版本
  加字段、旧版本读同一份文件，写回时不会把不认识的那一行吃掉。

## 四、配置项（`stream-config.toml`）

```toml
game = true                 # 关着时 /api/v1/control/game/* 一律 404
game_instance = "mc-1.12.2" # instances\<id>.json
game_root = "games"         # 相对 exe 目录；空 = 默认
game_autostart = false      # 服务起来时顺手拉起游戏
```

`--game` / `--game-instance` / `--game-root` / `--game-autostart` 是命令行等价物；
`--print-game` 打印生效实例与拼出来的那一条命令行（不含敏感值，可贴到别处比对）。

## 五、接口

| 端点 | 行为 |
|---|---|
| `POST /api/v1/control/game/start` | 起；已在跑返回 409（同一份 saves 上两个 MC 会抢 `session.lock`） |
| `POST /api/v1/control/game/stop` | 停；见下节 |
| `POST /api/v1/control/game/restart` | 停（如果在跑）+ 起 |
| `GET /api/v1/health` | 多一个 `game` 段：`state / pid / uptime_secs / last_exit / crash_reports / window` |

`window` 这一栏不是装饰：这台机器的全屏有一个已知脾气 —— 一失去焦点就自己最小化到
`-32000,-32000`，进程活着、日志正常、屏幕上什么都没有。没有这一栏，运维看到
`state=running` 只会怀疑串流坏了。

## 六、实测结论（三条，都会影响后续设计）

### 1. MC 1.12.2 没有可用的外部优雅退出路径

给 LWJGL 窗口发 `WM_CLOSE`：**窗口消失、javaw 不退出**（进程停在 71 MB，无画面、
无日志）。先 `SetForegroundWindow` 再发，结果一样。

| 轮次 | 动作 | 结果 |
|---|---|---|
| 1 | WM_CLOSE 给非 LWJGL 窗口（探针类名读错时） | 顺带把主窗口关了，进程仍活 |
| 2 | WM_CLOSE 给 LWJGL（未激活） | window alive=False, process alive=True，等 15 s 未退 |
| 3 | SetForegroundWindow + WM_CLOSE | 同上，5 s 内未退 |

所以 `stop` 的判据从"等够时长"改成"还有没有意义等"：窗口一消失再给 1 秒收尾，
然后结束。停止耗时从 16 s 降到 **1.4 s**。

`last_exit.reason` 三种值：`exited`（自己退的）、`killed-window-gone`（窗口没了进程
还挂着 —— 就是上面这条）、`killed`（宽限期到了窗口还在）。

**代价必须写进运维手册：停止 = 强杀。** 玩家在游戏里没退出世界就点停止，等于
`taskkill /F`，可能留下半写的区块文件（本机那个 `Unexpected end of ZLIB input
stream` 崩溃就是这么来的）。正确顺序是：游戏内先"保存并退出"，再从页面停止。

### 2. DXGI 桌面复制采的是整个显示器

游戏窗口之外不能有任何可见窗口。`stream-setting` 工具箱在游戏启动时应缩到托盘，
否则会出现在串流画面里（第 0 档的 kiosk 清理）。

### 3. 全屏与否不影响采集

`fullscreen:true`（独占全屏）在这台机器上仍可被 DXGI 采到（2026-11-17 用户实测：
按 F11 后浏览器里画面正常）。这条推翻了"必须做无边框窗口"的假设。

## 七、已验证 / 未验证

已验证（`.128`，三轮）：

* 服务起来时载入实例，日志 `game: hosting instance "mc-1.12.2"`；
* `health.game` 从 `stopped` 正确反映 `running / pid / uptime / window(1936x1100 含边框)`；
* 5 个 mod 全部加载（junction 生效）；
* 三轮 start→stop：1.4 s / 1.5 s / 1.4 s，`reason=killed-window-gone`；
* 450 个单元测试通过（新增 12 个）。

未验证：

* `game_autostart = true` 的冷启动（现在配的是 false，手工走接口更可控）；
* `exclusive` 全屏模式下由托管启动的稳定性（依赖 S3 的输入锁定，否则失焦即最小化）；
* 崩溃自动重启（`restart_max` 目前是 0，代码走通但没造崩溃验证）；
* 停止前自动"游戏内存档"—— 目前做不到，见 §6.1。

### 前端面板上线后的复测（2026-11-17 15:20，从 `.129` 远程调用）

| 调用 | 结果 |
|---|---|
| `POST control/game/start` | 202，`pid 3892`；10 s 后 `window = LWJGL 1936×1100 @0,0`，`kiosk.armed=true reason=autolock` |
| 重复 `game/start` | 409，`error` 是中文说明，响应体**同时带完整的 `game` 段** —— 面板据此采纳服务端状态，不会出现"旧标签 + 红字" |
| `POST control/game/restart` | 202，1.44 s，新 `pid 3496` |
| `POST control/game/stop` | 200，`last_exit.reason=exited`；3 s 内 `kiosk.armed=false`（autolock 自动解锁） |
| `GET /api/v1/health` | 匿名可读（`game` / `kiosk` 两段都在） |

**结论：控制面从 LAN 内别的机器调得通**，不是只能本机 loopback —— 这是网页面板能成立的
前提。当初按"只允许本机"设计的假设在这里被实测推翻，若后续要收紧，得先解决"面板从哪
调"的问题，不能直接加 IP 限制。

## 八、输入锁定（S3，已完成）

见 [kiosk.md](kiosk.md)。三层：注入端过滤、低级键盘钩子、注册表策略；四个解锁出口：
`Ctrl+Alt+Del`、`--unlock`、网页口令、进程重启时自动还原。

与游戏托管的关系只有两条：

1. **`kiosk_autolock = true` 时，`game/start` 成功之后才上锁**（不是之前）。先锁再启动，
   玩家会在"进程刚起来、窗口还没出"的几十秒里面对一个锁住的桌面。
2. **全屏（F11）能用，前提是焦点不被抢走** —— 这正是锁定提供的东西。所以 `exclusive`
   全屏在托管启动下变成可选项，不再依赖"人工按住这个窗口"。

## 九、S4：网页面板（已完成两块）

原计划把面板做进 `stream-setting`（桌面端工具箱）。实际做在了 web 前端
`cloud-stream-web` 的 `addons/selkies-dashboard` 里，理由是：要锁的、要起停的东西都在
被控机上，而唯一稳定可达被控机的界面就是串流页面本身 —— 桌面端工具箱还得先有人坐在
那台机器前，那正好是 kiosk 之后做不到的事。

| 页面 | 文件 | 内容 |
|---|---|---|
| Game | `src/components/GamePanel.jsx` | 起停/重启、5 s 轮询 `health.game`、运行时长与窗口尺寸、崩溃报告入口、停止即强杀的原话提示 |
| Input lock | `src/components/KioskPanel.jsx` | 锁定/口令解锁、`hook_installed` 与 `policy_pending` 警告、`403` 双语义分流 |

两者共用 `src/lib/control-api.js`（唯一出口，见 `kiosk.md` §4「网页侧」）。

### 前端部署（这台机器上唯一走通的路）

`C:\Program Files\cloud-stream\selkies-web` 是静态根，替换目录即可，不用重启服务
（每次请求重读磁盘）。走的顺序：

```powershell
# 1. 本机打包（产物目录是 addons/selkies-dashboard/dist）
Compress-Archive -Path <dist>\* -DestinationPath temp\selkies-web.zip -Force
# 2. 本机起投递，被控机自己拉（scp 在这套策略下被拦）
toolchain\serve-file.ps1 -Port 18080 -File temp\selkies-web.zip
# 3. 被控机上解压到同盘临时目录，再交换
ssh mc-150 'curl -sS -o C:\Users\Admin\sw.zip http://<本机>:18080/selkies-web.zip'
ssh mc-150 'powershell -NoProfile -Command Expand-Archive -Path C:\Users\Admin\sw.zip -DestinationPath C:\PROGRA~1\cloud-stream\selkies-web-new -Force'
ssh mc-150 'cd /d "C:\Program Files\cloud-stream" && ren selkies-web.bak selkies-web.bak.prev & ren selkies-web selkies-web.bak && ren selkies-web-new selkies-web'
```

先解压到 `-new` 再交换，是为了让"新目录不完整"的失败形态变成一个空目录，而不是一个
半截的线上站点。上一个版本留在 `selkies-web.bak` 作回滚点。

三个踩过的坑：

1. **`Program Files` 的写入**：SSH 会话 `net session` 成功不代表能写安装目录；本次能写，
   但第一次失联前 `echo > probe.txt` 静默失败过。部署前先 `echo` 一个探测文件验一下，
   别在 `ren` 上才发现。
2. **引号地狱**：远端默认 shell 是 cmd，`powershell -Command "..."` 里再套带空格的路径，
   三层转义必错；`-EncodedCommand` 被本地安全策略拦。最后走通的是 8.3 短路径
   （`C:\PROGRA~1\cloud-stream\...`）+ 单层双引号。
3. **`tar -xf` 在这台机器上解不了 zip**（报"系统找不到指定的路径"），`Expand-Archive`
   才是可用的那条。

### 还剩下的

1. `stream-setting` 桌面端工具箱——现在不是必需路径了，做不做看有没有"不串流也能运维"
   的场景。
2. `C:\mc` 下 20 多个排障脚本归档到 `toolchain\probes\`。
3. `toolchain\deploy.ps1` 的 `-RemoteBase` 被第 139 行硬编码覆盖，且假定运行根是
   `<base>\stream`，与安装器实际的 `C:\Program Files\cloud-stream` 不一致。这一轮的部署
   是手工做的，这个脚本该修，否则下次还会绕开它。
4. 客户端从桌面迁到 `games\clients\`（1.5 GB，动 `Desktop\.minecraft`，等用户确认）。

