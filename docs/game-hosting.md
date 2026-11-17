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

## 八、输入锁定（S3，已完成）

见 [kiosk.md](kiosk.md)。三层：注入端过滤、低级键盘钩子、注册表策略；四个解锁出口：
`Ctrl+Alt+Del`、`--unlock`、网页口令、进程重启时自动还原。

与游戏托管的关系只有两条：

1. **`kiosk_autolock = true` 时，`game/start` 成功之后才上锁**（不是之前）。先锁再启动，
   玩家会在"进程刚起来、窗口还没出"的几十秒里面对一个锁住的桌面。
2. **全屏（F11）能用，前提是焦点不被抢走** —— 这正是锁定提供的东西。所以 `exclusive`
   全屏在托管启动下变成可选项，不再依赖"人工按住这个窗口"。

## 九、下一步（S4）

1. **面板**：`stream-setting` 加"游戏"页（全字段可改、危险项二次确认）与"输入锁定"
   页（锁定/解锁按钮、口令输入、三层开关）。
2. `C:\mc` 下的 20 多个排障脚本归档到 `toolchain\probes\`。
3. 已知待修：`toolchain\deploy.ps1` 的 `-RemoteBase` 参数被第 139 行的硬编码覆盖，
   且它假定运行根是 `<base>\stream`，与安装器实际的 `C:\Program Files\cloud-stream`
   不一致。这一轮的部署是手工做的（临时 `toolchain\serve-file.ps1` 用 HTTP 传二进制）。
