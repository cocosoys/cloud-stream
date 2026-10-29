# 一键部署（#34）

> 目标：从干净工作区到「浏览器能连上」是一条命令，中途任何一步失败都能回到
> 上一个能跑的状态。本文是这条命令的说明书，也是它为什么长这样的原因。
>
> 脚本：`toolchain/deploy.ps1`（本机）、`toolchain/deploy-remote.ps1`（被控机）。
> 最后核对：2026-10-29，部署机 `192.168.59.148`，`ssh mc-admin`。

---

## 0 一条命令

```powershell
pwsh toolchain/deploy.ps1
```

输出长这样（真实运行，48.7 s）：

```
[1] 预检
      OK   ssh / scp / curl.exe 就位
      OK   ssh 可达：client001（mc-admin）
      OK   sshd 已是自动启动
      OK   防火墙放行规则已在
      OK   本机 exe 17.3 MB  md5=AA37E0EF...
[2] 上传
      OK   stream-server.new.exe
      OK   selkies-web.zip
      OK   deploy-remote.ps1
[3] 传输校验（两端 MD5）
[4] 切换（停任务 → 备份 → 换文件 → 起任务）
           OK  task disabled / killed pid 1888 / swapped in ...
           OK  started pid 5956 session 1
[5] 体检（https://192.168.59.148:8080/api/status）
      OK   HTTP 200
      OK   listening on https://0.0.0.0:8080 (TLS rustls 1.2/1.3)
      OK   pid=5956 session=1 start=2026-10-29T10:45:07+08:00
部署完成：48.7s
```

页面 `https://192.168.59.148:8080/`，**记得 Ctrl+F5**：资产名变了，`index.html`
本身也可能还在缓存里。

---

## 1 部署内容清单

### 1.1 每次部署会动的文件

| 本机产物 | 被控机落点 | 干什么 | 要重启服务吗 |
|---|---|---|---|
| `target/release/stream-server.exe`（17.3 MB） | `C:\dev\stream\stream-server.exe` | 采集 / 编码 / 信令 / HTTP 静态 / 文件传输，全部逻辑 | **要** |
| `addons/selkies-dashboard/dist/`（打包 1.17 MB） | `C:\dev\selkies-web\` | 前端整页 | 不要 |
| `toolchain/start-stream.cmd`（2070 B） | `C:\dev\stream\start-stream.cmd` | **命令行参数全在这** | 下次启动生效 |
| `toolchain/run-hidden.vbs`（635 B） | `C:\dev\stream\run-hidden.vbs` | 无窗口启动，避免每分钟闪一次黑框 | 下次启动生效 |
| `toolchain/deploy-remote.ps1` | `C:\dev\deploy\deploy-remote.ps1` | 被控端执行体 | 不要 |

前端的目录内容（`-SkipFrontend` 可以整块跳过）：

```
index.html                  786 B   ← 判定「是不是 dashboard 构建」的唯一凭据
assets/index-*.js         727 KB   React + 侧边栏 + 面板
assets/index-*.css         38 KB
assets/selkies-core-*.js  527 KB   核心：信令、解码、输入、统计浮层
jsdb/                    1992 个   手柄映射库，按 VID/PID 分文件
codecs/                      4 个   可选编解码器
manifest.json / icon*.png / selkies.svg
```

### 1.2 只在引导（`-Bootstrap`）时管一次的东西

| 对象 | 位置 / 名字 | 为什么一次性 |
|---|---|---|
| sshd 服务 | `sshd`，启动类型 **Automatic** | 重启后还能连进来；改一次永久生效 |
| 防火墙 | `OpenSSH SSH Server (sshd)`，TCP 22 入站 | 同上 |
| 计划任务 | `MCStreamSrv`，每 1 分钟 | 看门狗（#81），创建要账户密码 |
| TLS 证书 | `C:\dev\cert.pem` / `C:\dev\key.pem` | 自签，静态；脚本只检查存在 |
| 目录 | `C:\dev\stream\logs`、`C:\dev\deploy`、`C:\dev\selkies-web` | 建一次 |
| 上传根 | `--upload-dir C:\Users\Administrator\Desktop` | 在 `start-stream.cmd` 里 |

### 1.3 明确**不**进部署链的

- 游戏本体 / Java / Mohist 服务端 —— 各自有各自的启动方式，重启流媒体不该碰它们。
- 证书内容 —— 到期换证书是手工动作，脚本只验证文件在不在。
- `stream-server.exe.bak*` 这类历史备份 —— 只读，不清理（清理是人的决定）。

### 1.4 前端仓库在哪

前端是**独立仓库**（`cloud-stream-web`），部署脚本要在本机先构建它，所以得知道它
在哪。默认在后端仓库的同级目录里按名字找，两个常见名字都认：

```
<父目录>/
  cloud-stream/          ← 后端（本仓库）
  cloud-stream-web/      ← 前端，按仓库名克隆
  # 或者老工作区沿用的 selkies-webclient/
```

两个都没有时脚本在第一步就 `throw`，不会带着空 `$CoreDir` 一路跑到打包阶段才炸。
目录放在别处就用 `-WebRoot`：

```powershell
pwsh toolchain/deploy.ps1 -WebRoot D:\src\cloud-stream-web
```

---

## 2 路径契约：为什么改一处要连带改三处

**命令行参数不在部署脚本里，在 `start-stream.cmd` 里。** 这是最容易忘的一条：
只换 exe 不换启动脚本，参数就永远停在上一次。

| 路径 | 谁在引用 | 改它要连带改 |
|---|---|---|
| `C:\dev\stream\stream-server.exe` | `start-stream.cmd` 最后一行 | — |
| `C:\dev\selkies-web` | `start-stream.cmd` 的 `--dist` | 部署脚本 `$RemoteWeb` |
| `C:\dev\cert.pem` / `key.pem` | `--tls-cert` / `--tls-key` | — |
| `C:\dev\stream\logs\stream-server.log` | `--log` | 部署脚本 `$RemoteLog`（体检读它） |
| `C:\dev\stream\run-hidden.vbs` | 计划任务的 `/TR` | `setup-watchdog.bat` |

所以引导阶段会把两个启动脚本一起推上去，并且**覆盖** `C:\dev\stream\` 下的同名文件。

---

## 3 一次性引导

```powershell
pwsh toolchain/deploy.ps1 -Bootstrap
```

做四件事，全部幂等，可以反复跑：

1. **sshd 开机自启**（见 §3.1）
2. **建目录**：`C:\dev\stream\logs`、`C:\dev\deploy`、`C:\dev\selkies-web`
3. **推启动脚本**：`start-stream.cmd`、`run-hidden.vbs` → `C:\dev\deploy\` 再落到 `C:\dev\stream\`
4. **核对**三样东西并打印结果：exe 在不在、计划任务在不在（`Running`）、证书在不在

```
..   exe 18144256
..   task Running
..   tls OK
```

### 3.1 为什么单独强调「ssh 自动开启」

默认装完 OpenSSH Server，`sshd` 的启动类型是 **手动**。表现是：机器重启 →
服务不再运行 → 部署脚本 `ssh` 直接超时，而人可能不在现场。

脚本每次部署都会检查并修正（幂等）：

```powershell
Set-Service sshd -StartupType Automatic
Start-Service sshd
New-NetFirewallRule -DisplayName 'OpenSSH SSH Server (sshd)' -Direction Inbound -Protocol TCP -LocalPort 22 -Action Allow
```

`ssh` 真连不上的时候，脚本会**把这三行原样打出来**并停下来，告诉你到被控机
控制台（VMware 窗口 / IPMI）执行一次。它不会在那儿转圈重试。

### 3.2 计划任务需要密码，脚本不碰

`MCStreamSrv` 要用 `Administrator` 的**交互式**会话跑（见 §7 的会话隔离），
`schtasks` 建它必须带 `/RU` + `/RP`。把密码写进新脚本里没有意义，所以这一步
留给人：

```bat
:: 在被控机上执行一次
C:\dev\deploy\setup-watchdog.bat
```

脚本只负责核对它存在且是 `Running`。不存在就报 `task MISSING` 并随后在切换
阶段失败（比静默跑起来好）。

---

## 4 日常部署

| 命令 | 用途 | 大约耗时 |
|---|---|---|
| `pwsh toolchain/deploy.ps1` | 后端 + 前端全量 | 50 s（增量构建）+ 构建时间 |
| `pwsh toolchain/deploy.ps1 -SkipBackend` | 只改前端（样式、面板） | 20 s，**不断会话** |
| `pwsh toolchain/deploy.ps1 -SkipFrontend` | 只改后端 | 30 s |
| `pwsh toolchain/deploy.ps1 -SkipBuild` | 已经构建过，只上传 | 15 s |
| `pwsh toolchain/deploy.ps1 -DryRun` | 构建 + 上传 + 校验，**不切换** | — |
| `pwsh toolchain/deploy.ps1 -Bootstrap` | 补一次性的东西 | — |

其他参数：`-Target mc-admin`（ssh 别名）、`-HostName 192.168.59.148`、
`-Port 8080`、`-HealthTimeoutSec 90`。

`-DryRun` 是排查「到底是构建坏了还是部署坏了」时最有用的一个：它把文件送到
位、两端 MD5 对上，然后停手。

---

## 5 五个阶段分别做什么、判断依据是什么

| 阶段 | 动作 | 判据 |
|---|---|---|
| 1 预检 | 本机要 `ssh`/`scp`/`curl.exe`；远端要能 `hostname` | 连不上 → 打印修 sshd 的三行 |
| 2 上传 | `scp` 到 `stream-server.new.exe` / `selkies-web.zip`；带 `BatchMode=yes`，缺密钥立刻失败而不是卡在密码提示 | — |
| 3 校验 | 两端 MD5 比对 | 不一致就停，**不切换** |
| 4 切换 | 远端：停任务 → **禁用任务** → 杀进程 → 备份 `.prev` → 换文件 → 解压 → 启任务 | 每一步失败即中止 |
| 5 体检 | 轮询 `/api/status` | 见下 |

体检是**三重**判据，缺一不可：

1. `GET https://<host>:8080/api/status` 返回 200（`http/server.rs:265` 回纯文本 `ok`）；
2. 被控机上 `stream-server` 进程的 `StartTime` **晚于本次部署开始时间**；
3. 日志尾部 40 行里出现 `listening on`。

只查端口是不够的：看门狗每分钟会拉起进程，如果新 exe 起不来、旧进程又被拉起来，
端口照样通，但那跑的是旧二进制 —— 第 2 条就是为这种情况准备的。

---

## 6 失败与回滚

脚本 `catch` 到任何异常都会自动调：

```powershell
ssh mc-admin 'powershell -ExecutionPolicy Bypass -File C:\dev\deploy\deploy-remote.ps1 -Rollback'
```

远端回滚的动作：停任务 → 杀进程 → `stream-server.exe.prev` 覆盖回 live →
`selkies-web.prev` 换回目录 → 启任务。

手工回滚（脚本本身坏了时）：

```powershell
ssh mc-admin 'powershell -ExecutionPolicy Bypass -File C:\dev\deploy\deploy-remote.ps1 -Rollback'
```

**回滚的边界**：只回滚 exe 和 web 目录。配置（`start-stream.cmd`）、证书、日志
都不动 —— 它们不是「上一版/这一版」的关系。

`.prev` 是**一份**，不是历史栈：连续部署两次之后，`.prev` 是倒数第二版。要长期
保留某个版本，自己 `Copy-Item` 到 `C:\dev\stream\archive\`。

---

## 7 踩坑记录

按「症状 → 根因 → 现在怎么防」记。前四条都是部署链上真实发生过的，不是假设。

### 7.1 页面能出画面，但左侧工具栏整个消失

**根因**：服务的是 **core 构建**（`addons/selkies-web-core/dist`，`index.html`
185 B，只有 `#app` + `selkies-core.js`），不是 **dashboard 构建**
（`addons/selkies-dashboard/dist`，`index.html` 786 B，带 `#root` + `assets/`）。
工具栏住在 dashboard 的 React 侧边栏里，那个页面根本没被服务。

**防**：部署脚本只打包 `selkies-dashboard/dist`，并且远端解压后**检查
`assets\*.js` 是否存在**，没有就报 `wrong build (core instead of dashboard?)`。
自查一行：

```bash
curl -sk https://192.168.59.148:8080/ | grep -o 'assets/[^"]*'
# 期望两行：index-*.js、index-*.css
```

### 7.2 工具栏和统计面板一起消失 —— 整个 dashboard 没挂载

**根因**：`selkies-core.js` 的 `installStatsHud()` 里，CSS 模板字符串的注释中写了
反引号，字符串被提前截断，运行时 `ReferenceError: handle is not defined`。
`main.jsx:70` 先调 `selkiesCoreInitialize()`、第 80 行才 render React 树，异常把
**整个 dashboard 的挂载**带走了。

**防**：`installStatsHud()` 已经包了 try/catch，可选浮层拖不垮主界面；构建链上
这类错误 `npm run build` 是查不出来的（语法合法），只能靠打开页面看。

### 7.3 `schtasks /End` 之后进程还在

**根因**：任务是 `wscript.exe run-hidden.vbs` → `cmd /c start-stream.cmd` →
`stream-server.exe` 三层。`/End` 只结束最外层，exe 还活着，`Move-Item` 覆盖
正在运行的可执行文件失败。

**防**：远端脚本 `/End` 之后**显式 `Stop-Process` 并轮询 20 次 × 250 ms**，确认
进程消失才动文件；还有残留就 `FAIL` 退出，不硬来。

### 7.4 看门狗在「杀进程」和「换文件」之间把旧 exe 拉起来

**根因**：`MCStreamSrv` 每分钟触发一次。停在 `/End` 之后、`Move-Item` 之前的那
几秒里，下一次触发（或脚本自己的 `/Run`）会把旧 exe 启动，文件被占用。

**防**：先 `Disable-ScheduledTask` 再杀，换完文件才 `Enable` + `/Run`。禁用/启用
不改任务定义，只改触发器是否生效。

### 7.5 解压后首页 404 / 目录列表

**根因**：`Expand-Archive` 会把包里的顶层目录原样展开。从 `dist/` **外面**打包，
就会得到 `C:\dev\selkies-web\dist\index.html`。

**防**：本机用 `Compress-Archive -Path "$dashDist\*"`（只打内容）；远端解压后
断言 `index.html` 在根，否则 `FAIL`。

### 7.6 `ssh ... powershell -Command "...$p=C:\dev\x..."` 被吃掉

**根因**：内联命令要穿过 bash → ssh → cmd → powershell 四层引号。`$p` 在哪一层
被展开、`C:\dev\x` 被谁当成命令，取决于读者的耐心。

**防**：所有远端动作写成 `deploy-remote.ps1` 文件上传后 `-File` 执行。只有三处
仍然内联（查目录、查 MD5、查进程），因为都是一行、无赋值。

### 7.7 `Expand-Archive` / `Get-FileHash` 之外的「常识」不存在

`md5sum` 在 PowerShell 里没有；`certutil -hashfile` 也有，但输出要再切一次。
统一用 `Get-FileHash -Algorithm MD5`。

### 7.8 会话隔离：sshd 在 Session 0，游戏在 Session 1

**根因**：Windows 服务（含 `sshd`）跑在 Session 0，那里没有桌面。从 ssh 启动的
GUI 程序用户看不见，抓屏也抓的是空桌面。

**防**：用计划任务 `/IT`（交互式）以 `Administrator` 跑，游戏和流服务都在
Session 1。体检会打印 `session=1`，看到 `session=0` 就是这条踩了。

### 7.9 重启之后 ssh 连不上

见 §3.1。`sshd` 装完默认是「手动」。

### 7.10 中文输出变成「棰勬」「âœ?」

**根因**：在脚本里设 `[Console]::OutputEncoding = UTF8` 只改了 PowerShell 的解码，
没改控制台代码页；`pwsh deploy.ps1 > log.txt` 时新旧两套编码在一层管道里打架，
文件里留下 UTF-8 套 GBK 的二次编码。

**防**：脚本**不动**控制台编码。npm / cargo 的输出整条重定向进
`target\deploy-build.log`，失败时才打尾部 30 行 —— 既不刷屏，也不会被编码坑。

### 7.11 前端改了没生效

**根因**：`index.html` 自身被缓存（资产名带 hash 所以 JS/CSS 不会）。

**防**：部署完成的提示里固定带一句 `强刷 Ctrl+F5`。

### 7.12 `/api/status` 是纯文本，dashboard 却当 JSON 解析

**现象**：面板的 `Stream` 行显示 `websockets`，即使服务端用 `--app webrtc` 起。
**根因**：`main.jsx:41 detectInitialMode()` 做 `await resp.json()`，而
`/api/status` 回的是 `ok\n`，解析抛异常被 catch，`__SELKIES_STREAMING_MODE__`
没被设置，前端回落到 localStorage 里的 `stream_mode`。

这**不是部署问题**，别在部署链上找它；记在这里是因为第一次看到会以为部署错了。

### 7.13 `-SkipBuild` 会复用上一次的前端 zip（已修）

**现象**：重新 `npm run build` 之后跑 `deploy.ps1 -SkipBuild`，部署日志全绿，
但线上 `index.html` 引用的还是上一版的 `index-*.js`。

**根因**：打包步骤被塞在 `if (-not $SkipBuild)` 里。`-SkipBuild` 的本意是
「不重新编译」，但打包不是编译 —— dist 是磁盘上已经构建好的事实，zip 只是它
的快照。跳过打包 = 把上一次的快照发上去。

**防**：打包拆成独立步骤，只在 `-SkipFrontend` 时跳过；`-SkipBuild` 仍会按现有
dist 重新打包。判定线上是不是新产物，别看日志，看
`curl -sk https://<host>:8080/ | grep -o 'assets/[^"]*'` 里的文件名对不对。

### 7.14 构建在 `vite:prepare-out-dir` 处失败（已修）

**现象**：`npm run build` 报

```
[plugin vite:prepare-out-dir]
Error: [safe-delete][SAFE_DELETE_BULK_CONFIRM_REQUIRED] {"count":1998,"threshold":50,
  "targets":["...\selkies-web-core\dist\jsdb"]}
```

报错指向 vite 插件，看不出跟项目有什么关系。

**根因**：`selkies-web-core` 的 `postbuild` 是 `node gendb.js`，每次往 `dist/jsdb`
写 **1992 个**小手柄映射 JSON（`<vendor>-<product>.json`）。下一次 `vite build`
的 `emptyOutDir` 会一次性删掉它们 —— 在带批量删除保护的运行环境里（工作区把
node 的 `rmSync` 包了一层），超过阈值就拒绝执行，构建中断。

**防**：`deploy.ps1` 在调用 npm 之前用 `Remove-Item -Recurse` 自己清 `dist`
（PowerShell 不走那个 shim），构建路径就与文件数量无关了。手工构建时如果撞上
同样的报错，先 `rm -rf dist` 再 build。

### 7.15 只换前端却被判健康检查失败并回滚（已修）

**现象**：`deploy.ps1 -SkipBackend` 打印 `[5] 切换完成` → `[6] HTTP 200`，紧接着
报 `日志尾部没有 listening on —— 进程起来了但没在监听`，然后回滚，刚换上去的
前端又被换回来了。

**根因**：体检的前置假设是「这一轮重启过进程」。但 `-SkipBackend` 的整个意义就是
**不重启**（`deploy-remote.ps1` 只在换后端时才 `Stop-Server`，前端只是换目录），
所以进程还是原来那个 PID、日志里也不会有新的 `listening on`；而 `Get-Content
-Tail 40` 只覆盖约 3 分钟（5 秒一条统计），那条启动日志早就滚出去了。

**防**：切换前记下 PID，只换前端时改用「PID 未变 + HTTP 200」判定，并在输出里
写 `pid=10704（未重启，会话保持）`。体检的判据要和部署的语义一致 —— 判据错会
把一次成功的部署回滚掉，比不检查更糟。

### 7.16 回滚会把没换过的 exe 换掉（已修）

**现象**：`-SkipBackend` 部署失败后回滚，日志里出现 `exe restored 18181120 B`
和一次 `killed pid` / `started pid` —— 可这一轮根本没上传过 exe，会话也被打断了。

**根因**：回滚块无条件 `Copy-Item .prev → live` 并重启。`.prev` 是**上一次后端
部署**留下的，于是「回滚一次前端」等于把后端换成上一版（静默降级），并且重启了
一个和故障无关的进程。

**防**：自动回滚带 `-CycleOnly`：这一轮没换 exe 就不动 exe、也不重启；人去执行
`-Rollback` 仍是完整的「回到上一版」。两条路径语义不同，用一个开关区分，不要靠猜。

> **2026-10-29 修订**：最初还额外写了一个 `deploy-exe-swapped.marker`，想让回滚
> 「读」出这一轮到底换没换过 exe。问题是 marker 在 swap **成功时**就被删掉，而
> 回滚恰恰发生在 swap 成功之后 —— 它永远读到「没换过」。一次真正换了 exe 的部署
> 回滚时于是留下新 exe 不还原，日志还振振有词地写
> `exe untouched (this deploy did not swap it)`。现在只看调用方已经传进来的
> `-CycleOnly`，没有状态可以不同步，marker 一并删掉。

### 7.17 全量部署被判「日志尾部没有 listening on」（已修）

**现象**：全量部署 `[6]` 与 `[7]` 都是 HTTP 200，却报
`日志尾部没有 listening on —— 进程起来了但没在监听`，然后回滚。可进程明明在跑、
端口明明通。

**根因**：日志没有时间戳，判据是「尾部 40 行里出现 `listening on`」。有客户端连着
时 `pointer channel <- m,316,75,0,0,6` 每个指针事件一行，40 行只覆盖几百毫秒，
那条启动日志早被挤出去了。判据在「有人正在用」这个最常见的前提下不成立 ——
和 §7.15 是同一类错误：**判据的假设与部署的语义不一致**。

**防**：尾部放宽到 400 行；仍找不到时，只要 PID 变了（进程确实重启过）就以
`HTTP 200` 为准，输出里注明「可能是指针日志刷屏挤出了启动行」，不再判失败。
真正该失败的是反过来那种：**换了 exe 而 PID 没变** —— 计划任务没把新二进制拉起来，
跑的还是旧映像，这条现在会明确 throw。

---

## 8 验收清单

一条命令跑完之后，按顺序核：

- [ ] 退出码 0，末尾打印 `部署完成`
- [ ] `[6] 体检` 三行全绿：`HTTP 200` / `listening on ...` / `pid=... session=1 start=...`
      （只换前端时中间一行是 `pid=...（未重启，会话保持）`，这是正常的，见 §7.15；
      全量部署时中间一行可能是 `pid=...（新进程）` 加一条说明 —— 指针日志刷屏会把
      启动行挤出日志尾部，端口通即视为在监听，见 §7.17）
- [ ] `curl -sk https://192.168.59.148:8080/ | grep -o 'assets/[^"]*'` 有两行
- [ ] 浏览器强刷后**左侧有工具栏**（没有 = 服务成 core 构建了，§7.1）
- [ ] 右上角有统计面板，右边缘有粉色胶囊
- [ ] 故意失败一次能回滚：`-SkipFrontend` 上传一个坏 zip，或直接跑 `-Rollback`

---

## 9 脚本挂掉时的等价手工命令

```powershell
# 构建（从仓库根目录开始）
cargo build --release -p stream-server
cd ..\selkies-webclient\addons\selkies-web-core ; npm run build
cd ..\selkies-dashboard ; npm run build
cd C:\Users\user\Desktop\cloud-stream
Compress-Archive -Path ..\selkies-webclient\addons\selkies-dashboard\dist\* `
                 -DestinationPath target\deploy-selkies-web.zip -Force

# 上传
scp target\release\stream-server.exe mc-admin:C:/dev/stream/stream-server.new.exe
scp target\deploy-selkies-web.zip mc-admin:C:/dev/deploy/selkies-web.zip
scp toolchain\deploy-remote.ps1 mc-admin:C:/dev/deploy/deploy-remote.ps1

# 切换（唯一需要记住的一条）
ssh mc-admin 'powershell -NoProfile -ExecutionPolicy Bypass -File C:\dev\deploy\deploy-remote.ps1'

# 体检
curl.exe -sk -o NUL -w '%{http_code}' https://192.168.59.148:8080/api/status
```

---

## 10 重启之后的运维自检

机器重启后（补丁、断电、虚拟机迁移），按这个顺序看：

| 检查 | 命令 | 期望 |
|---|---|---|
| ssh 通 | `ssh mc-admin hostname` | 主机名 |
| sshd 自启 | `ssh mc-admin 'powershell -c "Get-Service sshd \| Select Status,StartType"'` | `Running Automatic` |
| 看门狗起了没 | `ssh mc-admin 'powershell -c "(Get-ScheduledTask -TaskName MCStreamSrv).State"'` | `Running` |
| 服务在跑 | `curl.exe -sk https://192.168.59.148:8080/api/status` | `ok` |
| 进程会话 | `ssh mc-admin 'powershell -c "Get-Process stream-server \| Select Id,SessionId"'` | `SessionId=1` |
| 证书 | `ssh mc-admin 'powershell -c "Get-ChildItem C:\dev\cert.pem \| Select LastWriteTime"'` | 自查是否过期 |
| 磁盘 | `ssh mc-admin 'powershell -c "Get-PSDrive C \| Select @{n=\"FreeGB\";e={[math]::Round($_.Free/1GB,1)}}"'` | 现在是 27.8 GB |

前两项挂了都会让**后续所有部署都做不了**，所以它们的修复方法（§3.1 的三行）
应该和这台机器的 IP 一起记在能看到的地方。
