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
  "java": { "path": "C:\\mc\\jre8\\bin\\javaw.exe", "heap_mb": 2048,
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

* `javaVersion` 段（1.13+ 的 json 会写"这个版本要求 JRE 17"）目前不看，`java.path`
  仍由实例指定。要自动挑 JRE 时再说 —— 那件事故意留给人。
* 客户端目录仍在桌面（`Desktop\.minecraft`），迁到 `games\clients\` 会动 1.5 GB 用户数据，
  等确认。
