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
  runtime\                       cmdline.txt、hs_err_%p.log、natives\<版本>\、jre\<任意名>\
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
  "client_dir": "clients\\1.12.2-forge",  // 绝对或相对 games 根
  "saves_dir": "saves\\default",
  "java": { "path": "", "major": 0, "heap_mb": 2048,
            "jvm_args": [], "verify_classes": true, "extract_natives": true },
  "game": { "version": "1.12.2-Forge_14.23.5.2864",
            "player": "CloudPlayer", "game_args": [] },
  "display": { "model": "windowed", "width": 0, "height": 0, "x": 0, "y": 0 },
  "lifecycle": { "stop_timeout_sec": 15, "restart_max": 0, "restart_backoff_sec": 5 }
}
```

* `game.main_class` / `game.tweak_class` **不写是常态**：留空时分别从版本 json 的
  `mainClass` 与参数模板里的 `--tweakClass` 取。只在"确实要偏离 json"时才写（自定义
  包装启动器）。见 §十。
* `java.path` **留空是常态**：留空时按版本 json 的 `javaVersion.majorVersion` 自动挑
  JRE（见 §十二）。写了就永远优先，哪怕版本对不上 —— 那只警告、不拦，因为定制运行时
  是有意的。
* `java.major`（默认 `0` = 跟随 json）：钉死主版本。1.12 及以前的 json **没有**
  `javaVersion` 段，而一台机器上同时装了 8 和 17 时，"1.12.2 该用哪个"需要有人说了算。
* `java.verify_classes`（默认 `true`）：启动前扫一遍 classpath 的 jar 中央目录，确认
  主类与 tweaker 真的在里面。关掉它回到"java 自己报错"的老路。
* `java.extract_natives`（默认 `true`）：启动前按版本 json 的 `natives` 段把本地库解到
  `runtime\natives\<版本>`。关掉只影响"用哪一份 dll"（退回客户端目录下的
  `natives-tmp`），用于排障对比。见 §十一。
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
`--print-game` 打印生效实例与拼出来的那一条命令行（不含敏感值，可贴到别处比对）；
`--print-java` 打印这台机器上所有能找到的 JRE、各自的主版本、以及最终会选哪一个
（实例文件读坏了也照样输出）。

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

**但这三个值描述不了"是谁的意思"。** `stop` 在这台机器上恒走 `killed-window-gone`
+ `code=1`，一次真崩溃也是 `exited` + 非 0，两者从 JSON 上看不出区别。所以
`last_exit` 还带一个 `requested`：

| `requested` | `code` | 含义 | 面板文案 |
|---|---|---|---|
| `true` | 任意 | 有人调了 `POST control/game/stop` | 已按请求停止 |
| `false` | `0` | 玩家在游戏里选"保存并退出" | 客户端正常退出 |
| `false` | 非 0 / `null` | 崩了 | 异常退出（code N），查看崩溃报告 |

`requested` 缺失（0.4.0 及更早）时前端退回旧文案，不猜。

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

* 停止前自动"游戏内存档"—— 目前做不到，见 §6.1。

### 第四轮（2026-11-19，0.4.0 → 0.4.1）

这一轮的目的是把"代码走通"的三项变成"机器上走通"，顺带查清了 `last_exit` 的
不确定性问题。

| 场景 | 结果 |
|---|---|
| `game/stop` 连续 4 次（windowed ×3、exclusive ×1） | 全部 `reason=killed-window-gone` + `code=1` —— **确定性**，不是偶发；这就是加 `requested` 的理由 |
| `display.model=exclusive` | 窗口 `1920×1080 @0,0`；windowed 是 `1936×1100 @0,0`，多出的 16×20 就是边框 |
| `game_autostart=true` + 重启计划任务 | 7 s 后自动 start，200 s 出窗口，`kiosk.armed=true reason=autolock` |
| `restart_max=2`，强杀 javaw 两次 | 两次都自愈：`restarts` 1→2，pid 4400→5280→5424 |
| 第 3 次强杀 | 不再重启，`state=stopped`，`restarts` 停在 2 |
| 主动 `game/stop`（配额未用尽） | `requested=true`；25 s 后仍 `stopped`、`restarts=0` —— 自愈没有误触发 |
| 冷启动耗时 | 130–200 s（5 个 mod + CustomNPCs 的 Markov 字典）。面板的"启动中…"要能撑住这么久 |

仍未验证（都需要人在串流页面或 VMware 控制台里做）：

* **物理键盘**在锁定态是否真按不动（`Alt+Tab`/`Win`）。脚本发的键都是 `INJECTED`，
  按设计会被放行，自动化测不出来 —— 见 `kiosk.md` 的同一项。
* **F11 运行时切全屏**。这一项卡在一个更基础的事实上：SSH 会话在 Session 0，
  而游戏窗口在 Session 1，`FindWindow`/`GetWindowRect` 在 Session 0 里全是空 ——
  测不了，不是没测。`exclusive` 的启动即全屏已经把"托管下能全屏"这件事证明了，
  F11 只剩"锁定后焦点不被抢"这一半。
* 停止前自动存档（同上，做不到）。


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
4. ~~客户端从桌面迁到 `games\clients\`~~ → 已执行（`.128`，2026-11-18），见 §十三。
   源目录 `Desktop\.minecraft` 按"复制不删源"的策略保留着，观察一段时间再决定。

## 十、参数来源与启动前校验（E + A）

### 起因：改一个参数就起不来

老的 `plan()` 把 1.12.2 的那串参数一条条 `push` 进 `Vec<String>`，同时实例文件里也写着
`main_class` 与 `tweak_class`。而版本 json 里这两样**本来就有**：

```
1.12.2-Forge_14.23.5.2864.json
  mainClass          = net.minecraft.launchwrapper.Launch
  minecraftArguments = --username ${auth_player_name} … --versionType ${version_type}
                       --tweakClass net.minecraftforge.fml.common.launcher.FMLTweaker
```

同一件事存两处，任一处抄错一个字母，`javaw` 只会回一句
`Could not find or load main class`，与"哪个字段写错了"毫无关联。这就是"改一个参数
就启动失败"的具体形态。

### 参考 PCL2（`ModLaunch.vb`）

| 机制 | PCL2 的做法 | 我们原来的做法 |
|---|---|---|
| 参数来源 | 旧版读 `minecraftArguments`，1.13+ 读 `arguments.game` / `arguments.jvm` | 全硬编码 |
| 占位符 | 二十余个 `${…}`，`${classpath}`、`${natives_directory}`、`${resolution_width}`… | 无 |
| 继承 | `InheritName` 向上逐级追加参数与库 | 只看单层 json |
| 规则 | `McJsonRuleCheck` 统一过滤（含 `features`） | 只认 `os.name` / `os.arch` |
| 去重 | 拼接后统一去重：游戏参数**新值覆盖旧值**，JVM 与 `--tweakClass` 保留旧值 | 无 |
| natives | 遍历 `IsNatives` 的库，只解 `.dll`，被占用则中止 | 不解压，靠预先铺好的目录 |

最值得抄的是**去重**：PCL 敢把 `--height/--width` 直接追加到模板后面，正是因为后出现的
重复键会覆盖前面的 —— "改窗口尺寸"这件事永远不需要动模板。

### 这一轮做了什么（E + A）

**A. 消除双写。** `main_class` / `tweak_class` 变成可选覆盖：

- 留空 → 主类取 json 的 `mainClass`；
- 留空 → tweaker 从 json 的参数模板里取 `--tweakClass` 的值（旧式 `minecraftArguments`
  按引号感知的规则切分后取，新式 `arguments.game` 数组也认）；
- 写了 → 以实例为准，来源标 `instance`。

**E. 让"改坏了"在启动前就能看见。**

`--print-game` 现在分三段输出，每个 token 标来源（`builtin` / `json` / `instance`）：

```
# check      : args: 9 个 jvm / 24 个 game token，无重复键、无占位符残留
# check      : classes: 2 个类都在 classpath 里（扫了 55 个 jar）

jvm (9 token)
  builtin  -Xmx2048M
  …
  json     C:\Users\Admin\Desktop\.minecraft\libraries\…
main
  json     net.minecraft.launchwrapper.Launch
game (24 token)
  builtin  --username
  instance CloudPlayer
  …
  builtin  --tweakClass
  json     net.minecraftforge.fml.common.launcher.FMLTweaker

command
<一整行，可直接复制比对>
```

三条校验，任一不过就拒绝启动而不是等 `javaw` 秒退：

1. **占位符残留** —— 参数里出现 `${…}` 说明有人把模板串写进了 `game_args`。
2. **重复的 `--flag`** —— MC 一般只认第一个，重复是"改了却没生效"的经典来源。
3. **主类 / tweaker 真的在 classpath 里** —— 读 jar 的中央目录（不解压），
   扫不到就报错并点出"扫了几个 jar、哪些 jar 读不开"。可用
   `java.verify_classes: false` 关掉。

### 实测（`.128`，2026-11-17 16:20）

| 场景 | 结果 |
|---|---|
| 实例写着 `main_class` / `tweak_class` | `--print-game` 标 `instance`，起停正常（1.4 s） |
| 删掉这两个字段后 | 来源变 `json`，`# main` 与 `command` 行**逐字不变** |
| 真实启动（去双写） | 202，pid 3696，11 s 后窗口 `LWJGL 1936×1100 @0,0` |
| 把主类改成 `…LaunchTypo` | 启动前报错：`classpath 里找不到 …LaunchTypo（扫了 55 个 jar；另有 5 个 jar 读不开（scala-continuations-library…、… 等 5 个））` |
| 单测 | 480 通过（新增 7：切分 / 取值 / 重复键 / 中央目录 / 主类与 tweaker 解析） |

**顺手查出一个真实隐患**：那 5 个"读不开"的 jar 是 `scala-continuations-*`、
`scala-parser-combinators-*`、`scala-xml-*` 等，**每个只有 689 字节** —— 内容是下载失败
留下的错误页，不是 zip。1.12.2 跑 Forge 时并不加载它们，所以游戏一直能起；但它们确实是
坏文件，将来若某个 mod 用到 Scala 才会以"类找不到"的形式暴露。来源是整合包自带的
`libraries`，本服务不做资源下载，所以这里只记录、不自动修。

### 之后（B / C / D 已完成）

见 §十一 —— 参数整体模板化、继承链合并、natives 自动解压。


## 十一、参数模板 / 继承链 / natives（B + C + D）

上一轮（§十）只把主类与 tweaker 从 json 里取回来，参数还是硬编码。这一轮把三件事做完：
**参数整体由版本 json 的模板生成**（B）、**读继承链并合并库**（C）、**按 json 解 natives**（D）。

### B：参数来自模板，去重收口

新增 `game/template.rs`，做四件事：

| 职责 | 做法 |
|---|---|
| 占位符 | `${auth_player_name}`、`${game_directory}`、`${natives_directory}`、`${classpath}`、`${resolution_width}`… 二十余个；**不认识的直接报错**，绝不把 `${…}` 原样传给 java |
| `rules` | `os.name`/`os.arch`/`features`；**最后一条命中的规则**说了算，一条没命中就排除。也接受"单个对象而不是数组"的写法（1.12.2 的 `lwjgl-platform:2.9.2` 就是这样少了一对方括号） |
| 两种模板 | 旧式 `minecraftArguments` 按**引号感知**切分；新式 `arguments.game` 数组，元素可以是字符串或 `{rules, value}` |
| 去重 | 同一个 `--flag` 只留**最后一次**出现的那一对（PCL2 的语义）；`${versionType}` 展开成空串时连键一起删 |

拼接顺序固定为：**模板 → `--width/--height` → 实例的 `tweak_class` → 实例的 `game_args`**，
然后统一去重。所以"改窗口尺寸"永远不用动模板 —— 模板里有 `--width` 也没关系，后面那对会顶掉它。

`--print-game` 因此多了一行"覆盖掉了哪些键"：

```
# check : args: 9 个 jvm / 24 个 game token，模板=json，覆盖掉的键=--versiontype，残留占位符=0
```

`--versiontype` 出现在这里不是意外：1.12.2 的模板自己写了两次（`--versionType ${version_type}`
与 `--versionType Forge`），去重让后者赢 —— 与旧实现硬编码 `release` 时得到的结果不同，
但**这才是 json 的本意**。

### C：继承链合并

`load_version_chain()` 沿 `inheritsFrom` 向上读（上限 16 层，超过就是成环），然后父在前、
子覆盖父地合并：

* `libraries`：子层覆盖同名构件时**原地替换**（身份是 `group:artifact[:classifier]`，**不看版本**），
  同一层里的重复**保留**；
* `arguments.game` / `arguments.jvm`：数组拼接，父在前；
* 其余字段（`mainClass`、`assetIndex`、`type`、`logging`）：子覆盖父；
* `inheritsFrom` 合并后丢弃 —— 留着只会让下游以为还有一层。

客户端 jar 沿链找**第一个存在的** `versions\<v>\<v>.jar`：`inheritsFrom` 式的 Forge 子层没有
自己的 jar，原版那个在父层目录里。

`arguments.jvm` 只作为**补充**：`-cp`、`-Djava.library.path`、`-Xmx/-Xms`、`-XX:ErrorFile`
这些由我们自己算，json 里的同名项**连值一起丢**（`-cp` 的值是一个独立 token，只删键不删值的话
它会变成主类前面多出来的位置参数，报错是 `Could not find or load main class <classpath 首项>`）。

`# chain : test-chain → 1.12.2-base` 会出现在 `--print-game` 里 —— "官方 installer 装的 Forge"
与"合并式 json"在参数上看不出区别，排查少库时这是第一个要确认的事实。

### D：natives 自动解压

`game/natives.rs` 按 `natives.windows` 找 classifier（`${arch}` 填 64），从
`downloads.classifiers[cls].path`（缺则按 maven 布局推）拿到 jar，解到 **`runtime\natives\<版本>`**，
`extract.exclude`（缺省 `META-INF/`）里的条目跳过，同尺寸的文件跳过不重写。

`-Djava.library.path` 是**两个目录**：`runtime\natives\<版本>` 在前，旧的那个
（`natives-tmp` 或版本目录）在后。多一个目录不多一分风险，少一套 dll 就是
`UnsatisfiedLinkError: no lwjgl in java.library.path`。

解压失败不致命：记一条 `# check : natives: …`，照旧用旧目录启动。

### 实测（`.128`，2026-11-17 17:20）

| 场景 | 结果 |
|---|---|
| 生产实例 `--print-game` | `模板=json`，24 个 game token 全部标 `json`，`--tweakClass` 来自模板，`--versionType` 去重后为 `Forge` |
| natives | `3 个 jar，解出 11 个文件`；第二次跑 `0 解出 / 11 已是最新`（幂等） |
| 合成继承链（`test-chain → 1.12.2-base`，子层用新式 `arguments.game` + `arguments.jvm`） | `# chain` 打出来；`-Dtest.jvm=1` 保留、`-cp`/`-Djava.library.path`/`-Xmx` 被丢（3 个 token）；子层的 `--demo` 出现；`--width/--height` 被实例的 1280×720 覆盖 |
| classpath | 末尾是 `versions\1.12.2-base\1.12.2-base.jar` —— 父层 jar 找到了，这是继承链真正生效的证据 |
| 真实启动（生产实例） | 202，pid 1264，窗口 `LWJGL 1936×1100 @0,0` |
| **把 `natives-tmp` 改名后启动** | 仍然起得来、窗口正常 —— 说明 `runtime\natives` 那一份**够用**，不再依赖预先铺好的目录 |
| 单测 | 495 通过（新增 15：占位符 / rules / 两种模板 / 去重 / 空值 / JVM 键归属 / 继承合并 / zip 中央目录） |

### 合并过程中踩到的两个真问题

1. **同一层里的重复库不能去重。** 第一版按构件身份对整份列表去重，`lwjgl-platform` 的
   2.9.4 被 2.9.2 顶掉，而 dll 只在 2.9.4 的 natives jar 里 —— 解出来只有 jinput 与
   text2speech 的 7 个文件。表现会是 `UnsatisfiedLinkError: no lwjgl`，与"合并库"看不出关系。
   修法：只拿**父层**（本层开始之前的那些条目）比对。
2. **`rules` 少一对方括号。** `lwjgl-platform:2.9.2` 写的是 `"rules": {"action":"allow","os":{"name":"osx"}}`。
   把它当成"没有规则"会得到完全相反的结论（装到所有平台）。修法：单个对象也当一条规则。
   修完之后 classpath 少了 osx-only 的 `java-objc-bridge` 等条目，"扫了几个 jar"从 55 降到 34。

### 还剩

* ~~`javaVersion` 段不看，JRE 靠实例手写 `java.path`~~ → 已做，见 §十二。
* ~~客户端目录仍在桌面（`Desktop\.minecraft`），迁到 `games\clients\` 会动 1.5 GB 用户
  数据~~ → 已执行（实际 237 MB），见 §十三。

## 十二、JRE 自动挑选（`javaVersion`）

### 为什么以前不敢做

版本 json 从 1.13 起写 `javaVersion.majorVersion`（1.17 写 16、1.20 写 17），官方启动器
据此下载运行时。我们一直只看 `java.path` —— 一个手写的绝对路径。于是同一台机器上
"1.12.2 要 8、1.20 要 17"这件事只能靠人记住并改配置，症状是两类，都很贵：

* 用 17 起 1.12.2 Forge：`Unsupported class file major version 61`，或者 LaunchWrapper
  的反射报错，字面上与"你选错了 java"毫无关系；
* 用 8 起 1.20：同样的错，方向相反。

### 怎么挑（`game/java.rs`）

候选按来源排序，**同版本里排在前面的赢**：

| 来源 | 位置 | 为什么在这个位置 |
|---|---|---|
| `runtime` | `games\runtime\jre\**`（递归三层，找 `bin\javaw.exe`） | 跟着部署走，换一台机器不该换结论 |
| `JAVA_HOME` | 环境变量 | 人已经明确表达过偏好 |
| `installed` | `%ProgramFiles%\{Java, Eclipse Adoptium, Microsoft, Zulu, BellSoft, Amazon Corretto, AdoptOpenJDK, Semeru}` 下的一级子目录 | 系统上正常装的那些 |
| `client` | 客户端目录自带的 `runtime\`（官方启动器下载的那份） | 客户端换版本时跟着变 |
| `legacy` | `C:\mc\jre8`、`C:\dev\jdk8\jdk8u504-b01` | 这台机器上的历史遗留，任何其它来源都能顶掉 |

版本号**先读 `<jre>\release`**（`JAVA_VERSION="17.0.9"`），读不到才起一次
`java -version`（带 `CREATE_NO_WINDOW`，否则玩家屏幕上会闪一个黑框）。探测要扫十几个
目录，只有前者能接受。

三条策略，每条都对应一种真会发生的部署：

1. **有精确匹配就用它**；
2. **没有精确匹配时向上取最小的那个** —— 1.17 的 json 写 16、机器上只有 17，是最正常
   的漂移（`--release 16` 的字节码在 17 上跑得起来）；
3. **全部低于要求时拒绝启动**。反过来是**必然**失败，没有"试一试"的余地 —— 与其让
   `javaw` 打一行 `UnsupportedClassVersionError` 秒退，不如说清"要求 17，扫到的是 8"。

两个例外：

* 实例写了 `java.path` → **永远照用**，版本不符只记一条警告（定制运行时是有意的）；
* json 没有 `javaVersion`（1.12 及以前）→ **优先 Java 8**，没有 8 才取最低可用版本，
  并留一条"json 没写 javaVersion"的提示。

### 怎么查（`--print-java`）

JRE 的问题全发生在"服务起得来、游戏起不来"那一层，那时人通常只能隔着串流页面看一个
黑屏。这个开关在任何一台机器上离线可跑，且实例文件读坏了也照样输出：

```text
# jre dir    : C:\Program Files\cloud-stream\games\runtime\jre
# required   : Java 17
# found      : 2 个 JRE
  runtime   Java 17 C:\...\games\runtime\jre\jdk-17\bin\javaw.exe   <- C:\...\games\runtime\jre
  JAVA_HOME Java 8  C:\dev\jdk8\bin\javaw.exe                      <- JAVA_HOME
# chosen     : C:\...\runtime\jre\jdk-17\bin\javaw.exe（Java 17，来自 runtime）
```

`--print-game` 里则多了两行：`# java major : 8` 与一条排在最前的
`# check : java: …（Java 8，来自 JAVA_HOME）` —— "要求 17、用的是 8"这类问题，命令行
本身是看不出来的。

### 实测（本机，无 `.128`）

| 场景 | 结果 |
|---|---|
| 1.20.1 的 json（`javaVersion: 17`）+ 只有 Java 8 | 拒绝启动，错误里带"要求 Java 17 / 扫到 Java 8 / UnsupportedClassVersionError" |
| 往 `runtime\jre\jdk-17` 放一个带 `release` 的 JRE | 立刻选中它（`runtime Java 17`），`--print-game` 通过 java 这一关 |
| 1.12.2 的 json（没有 `javaVersion`） | 两台都有时选 **Java 8**，不选 17 |
| 同一实例写 `java.major: 17` | 改选 17 —— 覆盖生效 |
| 同一实例写 `java.path: C:\dev\jdk8\bin\javaw.exe`，json 要求 17 | 照用 8，`# warning` 说明"版本 json 要求 Java 17，按实例配置照用" |
| 单测 | 506 通过（新增 11：`release` 解析 / `java -version` 解析 / 精确匹配 / 来源优先级 / 向上漂移 / 拒绝更旧 / 无要求优先 8 / 版本未知 / 路径回溯） |

顺带修掉一个自己造成的 bug：`--print-java` 第一版直接调 `pick` 而不是 `select`，于是
实例写了 `java.path` 时**表里的选择与实际启动不一致** —— 一个查错用的输出偏偏不说真话，
是最坏的一种。

### 实测（`.128`，2026-11-18 11:10，新二进制已部署）

这台机器上的 `1.12.2-Forge_14.23.5.2864.json` **带 `javaVersion: 8`**（合并式 json
里保留了这一段），所以要求是明确的：

| 场景 | 结果 |
|---|---|
| 实例仍写着 `java.path: C:\mc\jre8\bin\javaw.exe` | `# required : Java 8` / `# chosen : C:\mc\jre8\bin\javaw.exe（Java 8，实例 java.path）` |
| 删掉 `java.path` 后重启服务 | `# found : 1 个 JRE（legacy Java 8 C:\mc\jre8）` → `# chosen : …（Java 8，来自 legacy）` —— **自动挑选接住了原来手写的那个路径** |
| 同一实例 `--print-game` | 多出 `# java major : 8` 与首条 `# check : java: …`；classpath 34 个 jar、主类找到 |
| 起游戏 | 202，pid 4460，31 s 后窗口 `LWJGL 1936×1100 @0,0`，`kiosk.armed=true reason=autolock` |

## 十三、客户端迁移（`toolchain\migrate-client.ps1`）

把 `C:\Users\Admin\Desktop\.minecraft`（~1.5 GB）搬进 `games\clients\1.12.2-forge`。
**这是本项目唯一一处碰部署树之外的用户数据**，所以整个脚本的默认行为是
**复制 + 校验 + 保留源目录**，删原件要同时给 `-PurgeSource` 和 `-Yes`。

八步，每步失败都停在原地：

1. 游戏在跑就拒绝（`javaw` 的 CommandLine 里含旧客户端路径）—— 它把大 jar 映射着，
   robocopy 会报一堆像是磁盘故障的错；
2. 空间检查：复制不是移动，目标卷要放得下第二份（多留 15%）；
3. `robocopy /E /COPY:DAT /R:2 /W:2 /XJ`。`/XJ` 是关键：源里可能自己带 junction
   （旧的 `saves\mods` 链接），跟着走会重复甚至成环；
4. 校验：文件数 + 总字节数与源逐项相等，不等就把**每个一级子目录**的差异打出来然后
   中止，什么都不切换；
5. 改写 `instances\<id>.json` 的 `client_dir`：**只替换捕获到的那一段**（不用
   `ConvertFrom-Json` 往返 —— 那会重排键、改转义、格式化数字），先备份成
   `<id>.json.bak-<时间戳>`；
6. 检查新客户端下 `versions\<version>\<version>.json` 在不在（classpath 是从它推出来的，
   缺了就是"classpath 是空的"）；
7. 修 `saves\<dir>\{mods,config,resourcepacks}` 三个 junction —— `seed-games.ps1` 造的，
   搬完就悬空，症状是**游戏一个 mod 都不加载**。删链接用
   `[IO.Directory]::Delete()`：`Remove-Item -Recurse` 会穿过重解析点删掉真文件；
8. 有 `stream-server.exe` 就顺手跑一次 `--print-game`，等于让人当场看到"还能不能起"。

幂等：第二次运行会发现 `client_dir` 已经是目标值、junction 已指对，**不写任何文件**。
先在假目录上验证过一遍（复制 → 校验 → 改 json → 换 junction → 再跑无写入）。

### 实测（`.128`，2026-11-18 10:56）

```text
source : C:\Users\Admin\Desktop\.minecraft  files=1453 bytes=237,279,151
free   : 54,848,045,056 bytes on C
robocopy exit = 1 (0-7 = ok, 8+ = failure)
verify : files 1453 -> 1453; bytes 237,279,151 -> 237,279,151
client_dir: 'C:\\Users\\Admin\\Desktop\\.minecraft' -> 'clients\1.12.2-forge'
instance rewritten: …\instances\mc-1.12.2.json  (backup mc-1.12.2.json.bak-20261118-105657)
version json at new client: True
junction removed/made: mods / config / resourcepacks
```

两件事要记下来：

* **实际是 237 MB，不是 1.5 GB。** 那个数字来自更早的估算（可能算上了别的目录）。
  核对方法只有一条：让脚本自己 `Measure-Object` 一遍，别信文档里的旧数字。
* **三个 junction 都指过旧路径**，三个都被换掉了。如果漏了这一步，症状是"游戏起得来
  但一个 mod 都不加载"，而且日志里不会有任何一句抱怨。

迁移后立刻起游戏：pid 5296，窗口 `LWJGL 1936×1100 @0,0`；`saves\default\mods` 下能列出
`CustomNPCs_1.12.2` 与 `nashorn.jar`（走的是新 junction）。源目录仍留在桌面，等真正的
"跑一阵子没问题"之后再删。

## 十四、崩溃自愈与退出归因（0.4.1）

### 一维不够

`last_exit.reason` 回答的是"进程怎么没的"，不是"谁让它没的"。本机 1.12.2 的表现
是恒定的：`WM_CLOSE` 之后窗口销毁、javaw 不退出、1 秒后被 `TerminateProcess`——
于是**主动停止**永远是 `killed-window-gone` + `code=1`，与一次真崩溃从 JSON 上
看不出区别。第四轮连测 4 次，4 次一样，所以这不是偶发而是形态。

加了 `requested: bool` 之后，两个问题各有答案：

| 场景 | `requested` | `code` |
|---|---|---|
| `POST control/game/stop` | `true` | 1（强杀） |
| 玩家在游戏里"保存并退出" | `false` | 0 |
| 崩溃 / 被外部杀掉 | `false` | 非 0 或 `null` |

前端据此分三档措辞（`GamePanel.jsx` 的 `exitTone`）：已按请求停止 / 正常退出 /
异常退出（提示看崩溃报告）。字段缺失（≤0.4.0）时退回旧文案，不猜。

### 判据：什么该救

`should_restart()` 是一条纯函数，四条与关系：

```
max > 0 && !requested && code != Some(0) && restarts < max
```

* **`max = 0` 不救**：默认值，等于这个功能不存在。
* **主动停止不救**：人点了停止，重启它等于跟人对着干。
* **`code = 0` 不救**：那是玩家自己退出游戏，救回来等于把人拽回主菜单，
  还顺手盖掉他刚写的存档。
* **到上限不救**：坏客户端 / 坏驱动会让每次启动都崩在同一处，无限重启只会
  占满 CPU 并把 `logs` 刷爆。

监督循环每 2 秒看一次 `status()`，用 `last_exit.at_unix` 去重（否则同一次退出
会被反复重启）。重启成功后补一次 `kiosk::autolock` —— 停游戏会自动解锁，
这条路不补就会留下"游戏在跑、桌面没锁"。

### 实测（`.128`，2026-11-19）

`restart_max=2`，`restart_backoff_sec=5`：

| 动作 | 结果 |
|---|---|
| 强杀 javaw（pid 4400） | 20 s 内新 pid 5280，`restarts=1` |
| 再强杀（5280） | 新 pid 5424，`restarts=2` |
| 第三次强杀 | `state=stopped`，`restarts` 停在 2，日志一条"已自愈 2 次（上限 2）" |
| 手动 start 后 stop | `requested=true`，25 s 后仍 stopped、`restarts=0` |

单测 512（新增 6：非主动非零救 / 主动不救 / 零码不救 / 上限 / max=0 / code 缺失）。

### 踩到的：回滚配置会换掉访问口令

`configfile::ensure_auth_token` 是既有行为：**配置里 `auth_token` 为空时，服务启动
会生成一个随机口令写回配置文件**（它是 `/api/` 的 Bearer，也是 MC 上报的
`X-Cloud-Key`）。所以：

* 用备份覆盖 `stream-config.toml`（本文档 §九 那种"回滚"），重启后**会换一个新口令**，
  已经带旧口令打开的页面全部 401；
* 反过来，只要不动那个文件，重启多少次口令都不变。

这一轮就是被它绊了一次：用 `.bak-s4` 恢复配置 → 令牌丢掉 → 重启生成新的 →
我的 `curl` 全部 401。之后的验证都带着 `?token=<口令>`。**回滚配置的人要知道
"回滚也会回滚口令"**；不想换就把 `auth_token` 那一行先抄下来。
