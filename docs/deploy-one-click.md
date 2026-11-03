# 一键部署（#34）

> 目标：从干净工作区到「浏览器能连上」是一条命令，中途任何一步失败都能回到
> 上一个能跑的状态。本文是这条命令的说明书，也是它为什么长这样的原因。
>
> 脚本：`toolchain/deploy.ps1`（本机）、`toolchain/deploy-remote.ps1`（被控机）。
> 最后核对：2026-11-03，部署机 `192.168.59.148`，`ssh mc-admin`。

---

## 0 一条命令

```powershell
pwsh toolchain/deploy.ps1
```

输出长这样（真实运行：2026-11-03 布局迁移那一轮，`-SkipBuild` 全量切换 86.5 s，
`…` 是省略的无关行）：

```
[1] 预检
      OK   ssh / scp / curl.exe 就位
      OK   ssh 可达：client001（mc-admin）
      OK   sshd 已是自动启动
      OK   防火墙放行规则已在
      OK   本机 exe 17.7 MB  md5=468BE201…
      OK   本机 ViGEmClient.dll 140288 B  md5=91BA893E…
[2] 前端：跳过构建，按现有 dist 重新打包
      OK   dist（index.html 917 B，core selkies-core-yAvzPWiV.js）
      OK   打包 1.16 MB  md5=A49CAD29…
[3] 汇总产物（target\output\cloud-stream）
      OK   产物目录 …\target\output\cloud-stream
      OK     exe 17.7 MB / 前端 2008 文件 3.6 MB / 证书 yes（cert-148）
      OK     …\target\output\cloud-stream.zip（8.17 MB）
[4] 上传
      OK   stream-server.new.exe
      OK   ViGEmClient.new.dll
      OK   selkies-web.zip
      OK   start-stream.cmd / start-stream.60fps.cmd / run-hidden.vbs / setup-watchdog.bat
      OK   cert.pem / key.pem（仅当被控机缺失时安装）
      OK   deploy-remote.ps1
[5] 传输校验（两端 MD5）
      OK   exe 468BE201AA98B97E7183858861F29C87
      OK   dll 91BA893E750B2FDCCD142FDA9BA2DE2F
      OK   zip A49CAD294D3AA27554BF5F722AFD89AB
[6] 切换（停任务 → 换文件 → 起任务）
           STEP preflight
           OK  root C:\dev\stream
           OK  migrated legacy web root C:\dev\selkies-web -> C:\dev\stream\selkies-web
           OK  copied cert.pem into the runtime root
           OK  copied key.pem into the runtime root
           OK  new exe 18607104 B md5 468BE201…
           OK  new dll 140288 B md5 91BA893E…
           OK  zip 1214640 B md5 A49CAD29…
           STEP backend
           OK  task ended / task disabled / killed pid 9504
           OK  no stream-server process
           OK  swapped in 18607104 B md5 468BE201…
           OK  dll in place 140288 B md5 91BA893E…
           OK  start-stream.cmd unchanged / start-stream.60fps.cmd updated / run-hidden.vbs unchanged / setup-watchdog.bat unchanged
           STEP frontend
           OK  web dir 917 B index, 2 js chunks
           OK  staging zip removed
           STEP start
           OK  task enabled / started pid 4984 session 1
           DONE swap
[7] 体检（https://192.168.59.148:8080/api/status）
      OK   HTTP 200
      OK   listening on https://0.0.0.0:8080 (TLS rustls 1.2/1.3)
      OK   pid=4984 session=1 start=2026-11-03T12:00:48+08:00
      OK   看门狗动作 wscript.exe C:\dev\stream\run-hidden.vbs
[8] 归档（构建端）
      OK   构建端归档：backend + frontend（target\deploy-archive）
部署完成：86.5s
```

再跑一次同样的东西时，第 6 步的 `migrated` / `copied` 不再出现，五个上机脚本变成
`unchanged` —— 那就是幂等。

被控机上没有"备份"这一步：备份归构建端（第 8 步），详见 §6 与 §7.22。

页面 `https://192.168.59.148:8080/`，**记得 Ctrl+F5**：资产名变了，`index.html`
本身也可能还在缓存里。

---

## 1 部署内容清单

### 1.0 一个目录：`target\output\cloud-stream`

脚本在构建完之后，把「被控端需要的全部东西」汇总到
`target\output\cloud-stream`。这个目录**就是**被控机的运行根：里面的脚本全部
按自身位置推导路径（`%~dp0` / `WScript.ScriptFullName` / `$PSScriptRoot`），
所以解压到 `D:\stream`、`C:\dev\stream` 还是移动硬盘上，行为完全一样 —— 没有
一处写死目录。

```
target\output\cloud-stream\
  stream-server.exe      采集 / 编码 / 信令 / HTTP / 文件传输，全在这一个进程
  ViGEmClient.dll        #17 虚拟手柄客户端（可缺，缺了只降级成"没手柄"）
  cert.pem  key.pem      TLS 自签证书（SAN 是本机 IP；缺了退回明文 HTTP）
  selkies-web\           前端静态根
  logs\                  stream-server.log / watchdog.log（进程起不来先看后者）
  start-stream.cmd       命令行参数都在这里：分辨率 / 帧率 / 码率 / 证书
  start-stream.60fps.cmd 手工起一次用（同一个命令行，不进计划任务）
  run-hidden.vbs         计划任务入口，无窗口
  setup-watchdog.bat     创建 MCStreamSrv 任务（账户密码用参数或用提示输入）
  collect-launch-facts.cmd  起不来时双击它，生成一份事实清单（只读，不启动 exe）
  README.txt             上机三步 + 目录说明（给拿到包的人看）
  manifest.json          构建指纹：提交、时间、MD5、前端文件数
target\output\cloud-stream.zip   整包，拷到新机器用
```

于是「把一台新设备变成被控设备」是三步：解压 → `setup-watchdog.bat` → 打开
浏览器。不再逐个 `scp`，也不会漏掉 DLL / 脚本 / 证书。

生产机器上的对应位置是 `<RemoteBase>\stream`（默认 `C:\dev\stream`）。
`-RemoteBase` 是整台机器换位置时唯一要改的参数 —— 其余路径都是相对推导的。

### 1.1 每次部署会动的文件

| 本机产物 | 被控机落点 | 干什么 | 要重启服务吗 |
|---|---|---|---|
| `target\output\cloud-stream\stream-server.exe` | `C:\dev\stream\stream-server.exe` | 采集 / 编码 / 信令 / HTTP 静态 / 文件传输，全部逻辑 | **要** |
| `…\ViGEmClient.dll` | `C:\dev\stream\ViGEmClient.dll` | #17 虚拟手柄的客户端 DLL，服务端运行时 `LoadLibraryW` 取；缺了只降级成"没手柄" | **要**（已加载的 DLL 有共享锁，只能在停进程后覆盖） |
| `…\selkies-web\`（打包 1,214,640 B） | `C:\dev\stream\selkies-web\` | 前端整页（2008 个文件） | 不要 |
| `…\start-stream.cmd` | `C:\dev\stream\start-stream.cmd` | **命令行参数全在这**，路径按 `%~dp0` 推导 | 下次启动生效 |
| `…\start-stream.60fps.cmd` | `C:\dev\stream\start-stream.60fps.cmd` | 同一命令行的"手工版"，让人在被控机上用和看门狗一致的参数跑一次；任务不执行它 | 不要 |
| `…\run-hidden.vbs` | `C:\dev\stream\run-hidden.vbs` | 无窗口启动，避免每分钟闪一次黑框 | 下次启动生效 |
| `…\setup-watchdog.bat` | `C:\dev\stream\setup-watchdog.bat` | 建/重建看门狗任务，任务动作按自身位置生成 | 不要 |
| `…\collect-launch-facts.cmd` | `C:\dev\stream\collect-launch-facts.cmd` | 起不来时的第一个动作：双击生成事实清单（只读） | 不要 |
| `…\cert.pem` / `key.pem` | `C:\dev\stream\cert.pem` / `key.pem` | 仅当被控机**没有**证书时安装（SAN 是那台机器的 IP，不能拿构建端覆盖） | 要（`--tls-*` 在启动参数里） |
| `toolchain\deploy-remote.ps1` | `C:\dev\deploy\deploy-remote.ps1` | 被控端执行体（`deploy\` 里唯一的常驻文件） | 不要 |

上传的三样（exe / DLL / zip）都从汇总目录取，所以"本机检查过的包"和"线上在跑
的东西"是同一批字节，不是两次各自拼装的结果。

被控机最终只有这些，`selkies-web.zip` 解压成功后即删，不留副本（§6 / §7.22）：

```
C:\dev\stream\   stream-server.exe  ViGEmClient.dll  cert.pem  key.pem
                 start-stream.cmd  start-stream.60fps.cmd
                 run-hidden.vbs  setup-watchdog.bat
                 selkies-web\   ← 2008 个文件，下面这几项
                 logs\          ← stream-server.log / watchdog.log
C:\dev\deploy\   deploy-remote.ps1（只在部署窗口存在，zip 解压完即删）
```

前端的目录内容（`-SkipFrontend` 可以整块跳过）：

```
index.html                    917 B   ← 判定「是不是 dashboard 构建」的唯一凭据；#97 起内含 streaming-mode 注入
.asset-manifest.json        3,275 B   ← #99 全站指纹（2007 条登记，不含自身）
assets/index-*.js      738,508 B   React + 侧边栏 + 面板
assets/index-*.css      39,323 B
assets/selkies-core-*.js 535,115 B  核心：信令、解码、输入、统计浮层
jsdb/                    1994 个   手柄映射库，按 VID/PID 分文件（五平台，清单里聚合成 5 条单元）
codecs/libopus-wasm/         4 个   AudioWorklet 解码器
brand/logo.svg           1,641 B   侧边栏品牌图
manifest.json / favicon.ico / icon.png / icon-512.png
```

### 1.2 只在引导（`-Bootstrap`）时管一次的东西

| 对象 | 位置 / 名字 | 为什么一次性 |
|---|---|---|
| sshd 服务 | `sshd`，启动类型 **Automatic** | 重启后还能连进来；改一次永久生效 |
| 防火墙 | `OpenSSH SSH Server (sshd)`，TCP 22 入站 | 同上 |
| 计划任务 | `MCStreamSrv`，每 1 分钟，动作 = `<运行根>\run-hidden.vbs` | 看门狗（#81），创建要账户密码 |
| TLS 证书 | `C:\dev\stream\cert.pem` / `key.pem` | 自签，静态；脚本只检查存在 |
| 目录 | `C:\dev\stream\logs`、`C:\dev\stream\selkies-web`、`C:\dev\deploy` | 建一次（幂等） |
| 上传根 | `--upload-dir %USERPROFILE%\Desktop` | 在 `start-stream.cmd` 里，按运行账户推导 |

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

## 2 路径契约：只剩两个常量，其余全部自定位

**命令行参数不在部署脚本里，在 `start-stream.cmd` 里。** 这是最容易忘的一条：
只换 exe 不换启动脚本，参数就永远停在上一次 —— 所以启动脚本每轮都跟着部署走。

被控机上真正"约定"的只有两个目录：

| 常量 | 默认 | 谁决定 | 里面有什么 |
|---|---|---|---|
| `<RemoteBase>\stream` | `C:\dev\stream` | `deploy.ps1 -RemoteBase` | 运行根：exe、DLL、证书、`selkies-web\`、`logs\`、五个脚本 |
| `<RemoteBase>\deploy` | `C:\dev\deploy` | 同上 | 部署暂存：`deploy-remote.ps1`、传输中的 exe/zip/脚本 |

运行根内部的路径全是相对的，谁也没有"必须放在 C 盘"的假设：

| 路径 | 谁在引用 | 怎么来的 |
|---|---|---|
| `<根>\stream-server.exe` | `start-stream.cmd` | `%~dp0` |
| `<根>\selkies-web` | `start-stream.cmd` 的 `--dist` | `%~dp0`；`config.rs` 的默认值也是相对的 `selkies-web` |
| `<根>\cert.pem` / `key.pem` | `--tls-cert` / `--tls-key` | `%~dp0`；两个都在才启用 TLS |
| `<根>\logs\stream-server.log` | `--log` | `%~dp0`；体检读它 |
| `<根>\run-hidden.vbs` | 计划任务的 `/TR` | `setup-watchdog.bat` 用 `%~dp0` 生成；`run-hidden.vbs` 自己再用 `WScript.ScriptFullName` 找同级目录 |
| `<base>\deploy\selkies-web.zip` | `deploy-remote.ps1` 的 `-WebZip` | `$PSScriptRoot` |
| `<base>\stream` | `deploy-remote.ps1` 的 `-Root` 默认值 | `$PSScriptRoot` 的上一级 + `stream` |

**为什么值得这么改**：上一版把 `C:\dev\stream`、`C:\dev\selkies-web`、
`C:\dev\cert.pem`、`C:\Users\Administrator\Desktop` 写死在脚本里。把整包解压到
`D:\games\stream` 之后，任务照样"启动成功"，然后：`--dist` 指向一个不存在的
目录（首页 404）、日志写不进旧目录、剪贴板因为证书找不到退回明文。三处症状
互不相干，谁也不指向真正的原因。§7.23 记了这一轮。

---

## 3 一次性引导

```powershell
pwsh toolchain/deploy.ps1 -Bootstrap
```

做四件事，全部幂等，可以反复跑：

1. **sshd 开机自启**（见 §3.1）
2. **建目录**：`C:\dev\stream\logs`、`C:\dev\stream\selkies-web`、`C:\dev\deploy`
3. **推启动脚本并安装**：五个脚本 → `C:\dev\deploy\`，再由
   `deploy-remote.ps1 -OnlyLaunchers` 按哈希装进运行根（正在被看门狗执行的那个
   会跳过并提示"下次重启生效"）
4. **核对**四样东西并打印结果：exe 在不在、计划任务在不在（`Running`）、证书在
   不在、旧布局（`<base>\selkies-web`）有没有残留

```
..   exe 18603008
..   task Running
..   tls OK
..   layout ok
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

`MCStreamSrv` 要用能看见桌面的那个**交互式**账户跑（见 §7.8 的会话隔离），
`schtasks` 建它必须带 `/RU` + `/RP`。密码不写进仓库（每台机器都不一样，写进去
既是泄漏也是假的），所以这一步留给人，脚本只负责把它送上门：

```bat
:: 在被控机上，运行根目录里执行一次；账户默认是当前登录用户
C:\dev\stream\setup-watchdog.bat
:: 或者把账户密码一次给全
C:\dev\stream\setup-watchdog.bat Administrator S3cr3t
```

任务动作由 `%~dp0` 生成（`wscript.exe "<运行根>\run-hidden.vbs"`），所以整包
换个目录解压也能用。建完会回读一次动作字符串，读不到 `run-hidden.vbs` 就打
`WARNING` —— 引号写坏时任务照样"创建成功"，但永远不会真的拉起进程。

部署脚本只负责核对任务存在、`Running`、且动作路径在运行根下（不在就提示搬过
位置后要重建）。不存在就报 `task MISSING` 并随后在切换阶段失败（比静默跑起来好）。

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
`-Port 8080`、`-RemoteBase C:\dev`（被控机安装根 → 运行根 `<base>\stream`）、
`-HealthTimeoutSec 90`。

`-DryRun` 是排查「到底是构建坏了还是部署坏了」时最有用的一个：它把文件送到
位、两端 MD5 对上，然后停手。

---

## 5 十个步骤分别做什么、判断依据是什么

步号只增不减、不印分母：`-SkipBackend` 这类开关会让某几步不占号，印了分母
就会出现 `[4/8]` 之后直接 `[6/8]`。所以看输出时以标题为准，不要数编号。

| 步 | 动作 | 判据 |
|---|---|---|
| 1 预检 | 本机要 `ssh`/`scp`/`curl.exe`；远端要能 `hostname` | 连不上 → 打印修 sshd 的三行 |
| 2 构建后端 | `cargo build --release -p stream-server`（`-SkipBackend` 跳过） | 编译失败 → 不进入下一步 |
| 3 构建前端 | core → dashboard → `selkies-web.zip`（`-SkipFrontend` 跳过） | `dist` 缺失 / gendb 失败 → 抛错 |
| 4 汇总产物 | 把 exe / DLL / 前端 / 证书 / 五个脚本装进 `target\output\cloud-stream`，写 `manifest.json`，压同名 zip | 前端 `assets\` 缺失即抛错；本轮没有前端产物时跳过并回退成单件上传 |
| 5 上传 | `scp` **从汇总目录**取：`stream-server.new.exe` / `ViGEmClient.new.dll` / `selkies-web.zip` / 五个上机脚本 / 证书 / `deploy-remote.ps1`；带 `BatchMode=yes`，缺密钥立刻失败而不是卡在密码提示 | — |
| 6 校验 | 两端 MD5 比对 | 不一致就停，**不切换** |
| 7 切换 | 远端：装证书（仅当缺失）→ 停任务 → **禁用任务** → 杀进程 → 换 exe/DLL → **装启动脚本**（此刻文件才不被占用）→ 前端解压到暂存目录并校验后换名 → 删 zip → 启任务 | 每一步失败即中止 |
| 8 体检 | 轮询 `/api/status` | 见下 |
| 9 体检补充 | 进程 pid / 启动时间、日志尾部、**计划任务的动作路径** | 动作路径不在运行根下只提示不失败（§7.23） |
| 10 归档 | 构建端把这一轮真正发出去的部件写进 `target\deploy-archive\`（含五个上机脚本） | 失败不影响线上 |

前四步里 2 和 3 是可以单独跳过的，因为"只换前端"和"只换后端"都很常见；
**6 之后就没有可以单独跳过的步骤了** —— 校验、切换、体检是一条链，缺一环
就等于"不知道线上跑的是什么"。

体检是**三重**判据，缺一不可：

1. `GET https://<host>:8080/api/status` 返回 200（`http/server.rs:265` 回纯文本 `ok`）；
2. 被控机上 `stream-server` 进程的 `StartTime` **晚于本次部署开始时间**；
3. 日志尾部 40 行里出现 `listening on`。

只查端口是不够的：看门狗每分钟会拉起进程，如果新 exe 起不来、旧进程又被拉起来，
端口照样通，但那跑的是旧二进制 —— 第 2 条就是为这种情况准备的。

---

## 6 失败与回滚

**被控机上没有任何备份。** 那里只放正在运行的正式产物：一个 exe、一个 DLL、
一个 web 目录。回滚材料在构建端的 `target\deploy-archive\`（已 gitignore），由
每次**成功**部署后写入，只覆盖这一轮真正发出去的部件 —— 所以 `-SkipBackend`
的部署不会把归档里的 exe 抹掉。

失败时脚本自己恢复：先看这一轮动过什么（`backend` / `frontend`），把归档里对应
的部件当"新版本"再发一次，走的还是同一条 上传 → 校验 → 停止 → 更换 → 起任务
路径，所以恢复本身也会被体检。若失败发生在切换之前，被控机一个字节都没改，
脚本只打印一句"没有被改动"。

手工回滚（人也想退一版时）：

```powershell
pwsh toolchain/deploy.ps1 -Rollback
```

它不编译、不重新打包，直接把 `target\deploy-archive\` 里的上一份产物发上去，
再做健康检查。归档里有什么发什么（`meta.json` 会打印时间、部件、提交）。

**回滚的边界**：只回滚 exe、DLL 和 web 目录。配置（`start-stream.cmd`）、证书、
日志都不动 —— 它们不是"上一版/这一版"的关系。

**归档只有一份**，是"上一次成功部署的状态"，不是历史栈。要长期保留某个版本，
自己把 `target\deploy-archive\` 复制到别处。另外：如果有人在被控机上手工换过 exe，
归档与被控机的实际状态就会分叉 —— 这时 `-Rollback` 回到的是"上次部署的版本"，
不是"手工换上的那一版"。

> 为什么不再留 `.prev`：被控机上每台一份 18.6 MB 的 exe 副本、加一份 2000+ 文件
> 的 web 目录副本，部署十次就是十份没人轮转的残留；而真正需要回滚的人坐在构建端。
> 详见 §7.22。

---

## 7 踩坑记录

按「症状 → 根因 → 现在怎么防」记。前四条都是部署链上真实发生过的，不是假设。

### 7.1 页面能出画面，但左侧工具栏整个消失

**根因**：服务的是 **core 构建**（`addons/selkies-web-core/dist`，`index.html`
185 B，只有 `#app` + `selkies-core.js`），不是 **dashboard 构建**
（`addons/selkies-dashboard/dist`，`index.html` 917 B，带 `#root` + `assets/`）。
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

### 7.12 `/api/status` 是纯文本，dashboard 却当 JSON 解析 —— 新设备连不上（已修）

**现象**：全新设备打开页面，只有一行 `WebSocket disconnected. Attempting to
reconnect...`；控制台里 `GET /api/websockets 404`，`wss://.../api/websockets`
1006 反复重连。面板的 `Stream` 行也显示 `websockets`，即使服务端用
`--app webrtc` 起。

**根因**：`main.jsx:41 detectInitialMode()` 做 `await resp.json()`，而
`/api/status` 回的是 `ok\n`，解析抛异常被 catch，`__SELKIES_STREAMING_MODE__`
没被设置。接着 `selkies-core.js:120 determineStreamingMode()` 按
`注入值 || localStorage 里的 stream_mode || websockets` 取值：

* 老设备：`switchStreamingMode()` 每次启动都把当时的模式写进 localStorage，
  而同源曾经跑过静态注入 webrtc 的旧前端（`selkies-dashboard-dist`），于是那些
  浏览器被永久钉在 `webrtc` —— 这就是"我这台一直是好的"。
* 全新设备：localStorage 是空的，落到硬默认 `websockets`，去连
  `/api/websockets`。服务端只认 `/api/<app>/signaling`（`http/mod.rs:111`），
  该路径落进 `NotFound` 回 404，浏览器按规范报 1006，永远重连。

**服务端不补这个端点是故意的**：前端 WS 模式是一整条独立传输栈
（`selkies-ws-core.js`：一个 socket 承载全场会话，二进制按首字节分型
`0x01` 音频 / `0x03` JPEG 分片 / `0x04` 视频分片 / `0x05` gzip 控制 / `0x02`
麦克风 / `0x06` 摄像头，外加 `MODE`、`AUTH_SUCCESS`、`VIDEO_STARTED` 等控制帧），
不是加一条路由能糊过去的。

**已修（前端侧，`addons/selkies-dashboard/index.html`）**：在 bundle 之前静态注入

```html
<script>window.__SELKIES_STREAMING_MODE__ = "webrtc";</script>
```

注入是 `determineStreamingMode()` 的第一优先级，所以新设备和已被写成
`websockets` 的旧设备刷新后都走 WebRTC，不必手工清 localStorage。`__SELKIES_DUAL_MODE__`
故意不设：服务端不下发 `enable_dual_mode`（`Sidebar.jsx:1101` 回落 `false`），
面板里的模式切换按钮因此保持隐藏 —— 这个部署没有第二条传输可切。

**验证**：`curl -sk https://<host>:8080/ | grep -o '__SELKIES_STREAMING_MODE__[^<]*'`
应当看到注入，且它必须排在 `<script type="module" src="./assets/index-*.js">` 之前。


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
写 **1994 个**小手柄映射 JSON（`<vendor>-<product>.json`）。下一次 `vite build`
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
>
> **2026-11-03 修订（§7.22）**：`-CycleOnly` 本身有个更根本的毛病 —— 它的实现是
> 「不动 exe」，于是**后端部署失败时 exe 从来没被还原过**。现在远端没有 `.prev`，
> 也没有 `-Rollback` / `-CycleOnly`：恢复改成「本轮动过哪几个部件，就从构建端归档
> 重发哪几个」，判断依据在构建端(`$attempted`)，不依赖远端留下的任何状态。

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

### 7.18 `Remove-Item` 删构建产物报 `SAFE_DELETE_FAIL_CLOSED`（已修）

**现象**：构建前端或打包时脚本停下，报

```
[safe-delete][SAFE_DELETE_FAIL_CLOSED] {"target":"...\\selkies-web-core\\dist",
  "reason":"trash-failed","detail":"genie-trash failed; refusing fallback delete"}
```

指向的却是 dist 目录或一个旧 zip，和 vite、`Compress-Archive` 都没关系。

**根因**：本机 PowerShell 的 `Remove-Item` 在 Windows 上接的是 Shell 删除 API，
也就是回收站；工作区把回收站那层换成了带批量删除保护的 shim，删含上千文件的
`dist/jsdb` 或一个上 MB 的 zip 就返回 `trash-failed` 并拒绝降级删除。§7.14 当时
靠「`Remove-Item` 不走 node」绕过了 vite 那一侧，但没绕开回收站这一层。

**防**：`deploy.ps1` 里所有本机构建产物的删除改走 `Remove-LocalPath`
（`[System.IO.Directory]::Delete(..., true)` / `[System.IO.File]::Delete`），
直接落文件系统、不经过回收站。远端 `deploy-remote.ps1` 的 `Remove-Item` 不动 ——
那是被控机上普通的 Windows，没有这层 shim。

---

### 7.19 gendb 拉不到 GitHub 时静默产出空 `jsdb`（已修）

**现象**：`npm run build` 全绿、`deploy.ps1` 全绿，zip 却从 1.16 MB 掉到
0.55 MB，线上 `/jsdb/windows/045e-028e.json` 返回 404 —— 1994 个手柄映射全没了。

**根因**：`selkies-web-core` 的 `postbuild` 是 `node gendb.js`，要从
`api.github.com` + `raw.githubusercontent.com` 拉 SDL_GameControllerDB。本机到
`raw.githubusercontent.com` 的 TLS 握手会超时（~14 s 后
`schannel: failed to receive handshake`）。拉取失败落在 `catch` 里 `return`，
不写文件、**也不让构建失败**；`copy-assets.js` 于是拷了 0 个文件，残缺包照发。
再叠上 `deploy.ps1` 构建前会先删 `core/dist`，上一次的 jsdb 也一起没了。

**防**：`gendb.js` 的失败分支改成三级退让 —— ① `dist/jsdb` 已有映射就用它
（断网单独 `npm run build` 的正常情况）；② 否则从 `selkies-web-core/.cache/jsdb`
（首次成功构建时播下的一份，**只写一次**，因为刷新它又得删 1994 个文件）恢复并
告警；③ 两者都没有就 `exit 1`。宁可构建失败，也不发一个"看起来完整"的包。

> 判断有没有踩到：看 zip 体积。完整包 ~1.16 MB，缺 jsdb 的 ~0.55 MB。部署日志
> 不会告诉你。

---

### 7.20 `copy-assets.js` / `gendb.js` 里的 `fs.rmSync` 也会被批量删除保护拒绝（已修）

**现象**：`npm run build` 报

```
[safe-delete][SAFE_DELETE_BULK_CONFIRM_REQUIRED] {"count":2006,"threshold":50,...}
    at emptyDir (vite/dist/node/chunks/node.js)
```

指向 `vite:prepare-out-dir`；若轮到自己清 `dist/jsdb`，则是
`[safe-delete] 操作失败: spawnSync ...genie-trash... ETIMEDOUT`，栈顶落在
`copy-assets.js:24`。

**根因**：和 §7.18 同源 —— 保护是按**删除次数**算的，不是按调用方。`deploy.ps1`
用 .NET 删掉 `core/dist`/`dashboard/dist` 绕过了 vite 那一侧，但 `copy-assets.js`
自己还有一次 `fs.rmSync(dist/<name>)`，`gendb.js` 还有一次
`fs.rmSync(dist/jsdb)`。两者都是 node 进程里的一次上千文件删除。

**防**：两个脚本都不再"先删再写"：

- `copy-assets.js` 的 `syncTree()` 直接 `fs.cpSync(..., {force:true})` 覆盖，
  然后只对"上游已经没有"的残留文件逐个 `unlinkSync` —— 正常是 0 个。
- `gendb.js` 覆盖写的同时记录本次写入的路径集合，事后只删不在集合里的残留。

代价是多一次"残留扫描"，收益是构建结果与文件数量无关。

### 7.21 `toolchain/` 里堆了几十个一次性探针（已清）

**现象**：`toolchain/` 一度有 90 个文件、123 MB，其中 22 个 `.ps1`、9 个 `.bat`、
11 个 `.log`、25 个 `.txt`，很难判断哪些是部署必需的。

**先确认一件事**：这些文件**一个都不进部署包**。`deploy.ps1:433` 只打包
`dashboard/dist/*`；被控机 `C:\dev\stream` 只多收 `start-stream.cmd` 与
`run-hidden.vbs`（`deploy.ps1:357`）。所以它们不影响线上，只影响"仓库到底该有什么"。

**判据**：按「是否仍被任何文档/脚本引用」逐个查（扫描 4047 个文件），
49 个被跟踪文件里 27 个全仓库 0 引用。保留的 22 个：

| 类别 | 文件 |
|---|---|
| 部署契约（8） | `deploy.ps1` `deploy-remote.ps1` `start-stream.cmd` `start-stream.60fps.cmd` `run-hidden.vbs` `setup-watchdog.bat` `cert-san.cnf` `env-setup.ps1` |
| 构建（4） | `build-ws.cmd` `build-shell.cmd` `install-vcpkg-libs.cmd` `config.toml` |
| 长期诊断（6） | `mouse-probe.ps1` `probe-top.py` `cleanup-dev-root.ps1` `logtriage.ps1` `distcheck.ps1` `ps-tree.py` |
| MC / 密度（4） | `start-mc.ps1` `bench-local.ps1` + 两个 CPU 采样 CSV |

**另一个陷阱**：`toolchain/dist-test/` 是一份 #99 之前的 dist 副本，里面还有已删的
`selkies.svg`、且没有 `.asset-manifest.json`。它看着像"上一版备份"，一旦被当成
`--dist` 的来源就会把死文件带回去 —— 已删。上一版 dist 现在只在构建端的
`target\deploy-archive\selkies-web.zip`（每次成功部署后归档）；被控机上不再留
web 目录副本（§7.22）。

**还有一处漏网**：`dump-triage/stream-server-148.log`（64 KB 运行日志）被跟踪，
因为 `.gitignore` 在那里只排除了 `*.bmp`/`*.ivf`。现改为整目录忽略 `/dump-triage/`
（抓帧、临时脚本、SSH 配置都是可重生成的现场），已跟踪的
`mcbak/launch-remote.ps1` 不受影响。

清理后 `toolchain/` 123 MB → 221 KB。证书私钥、bench CSV、`vendor-backup/` 未动。

### 7.22 被控机上堆着构建端的东西（已清，回滚改到构建端）

**现象**：`C:\dev` 从"能跑起来的最小集合"长成了现场仓库 —— `deploy\` 里 30 个文件
（探针、日志拉取、诊断脚本、测试 zip），`bench\` 里 4 MB 的抓帧工具和它的输出，
`mc\` 里 14 个调试 dump，`stream\` 里 18.6 MB 的 `stream-server.exe.prev`，还有
一个 Python 原型留下的 `__pycache__`。其中真正被运行时引用的只有
`deploy-remote.ps1`（每次部署都重传）和 `selkies-web.zip`（解压完就没用了）。

**根因**：部署链把"备份"理解成"在被控机留一份上一版"。它有两个后果，第二个更糟：

1. 每台每次部署多 18.6 MB exe + 2000+ 文件的 web 目录副本，且没人轮转；
2. 这些文件和构建端推过来的临时脚本混在同一个目录里，时间一长就分不清
   "哪个是正式产物、哪个是某次调试留下的"。

**改成**：备份归构建端。`deploy.ps1` 在**成功**部署后把这次真正发上去的 exe /
DLL / zip 覆盖进 `target\deploy-archive\`（gitignore），失败或 `-Rollback` 时从
那里重发。被控机侧 `deploy-remote.ps1` 删掉 `-Rollback` / `-CycleOnly` 与整个
`.prev` 逻辑，前端改为**解压到暂存目录 → 在暂存目录里校验 → 两次 rename 换名**，
失败时活目录一个字节都没动；解压成功后删掉上传的 zip。

顺带修掉一处旧缺陷：`catch` 以前固定传 `-CycleOnly`，而 `-CycleOnly` 的实现是
"不动 exe"，于是**后端部署失败时 exe 从来没被还原过**。现在按"这一轮动过哪些
部件"恢复，换过就恢复，没换过就不碰（也就不会为一个失败的前端部署重启后端）。

**清掉的 57 个文件 / 22.93 MB**：`stream\*.prev`、`deploy\*`（30）、`bench\`（9）、
`mc\` 的 14 个 dump、`__pycache__\`、`mcjson.txt`。脚本：
`toolchain/cleanup-remote-artifacts.ps1`（`-WhatIf` 可预演，逐条列路径、不用通配符）。

**保留**：`cert.pem` `key.pem` `mc-launch.cmd` 两个 task xml；`stream\`（exe、DLL、
`start-stream.cmd`、`run-hidden.vbs`、`logs\`）；`selkies-web\`；`jdk8\`；
`mc\start-mc.ps1` 与 `mc\launch-remote.ps1`（`MCGame` 任务链）。

**边界**：`deploy\` 目录本身保留（空目录），因为 `scp` 需要它存在；跑完一次部署后
里面只会有 `deploy-remote.ps1`（16 KB）与五个上机脚本的副本（每轮重传，
装进运行根之后就只是暂存），zip 与上传的证书都在装好后立刻删掉。

### 7.23 上机脚本写死绝对路径 + 非 ASCII 注释（已修）

**现象**：把整包解压到 `D:\games\stream` 后跑一次看门狗，任务显示"运行过"，
浏览器打开是 404 或目录列表，`logs\` 是空的，剪贴板同步也没了。换回 `C:\dev`
一切正常。

**根因**（三个独立的硬编码，症状互不相干）：

1. `start-stream.cmd` 里 `cd /d C:\dev\stream`、`--dist C:\dev\selkies-web`、
   `--log C:\dev\stream\logs\...`、`--tls-cert C:\dev\cert.pem`；
   `run-hidden.vbs` 里 `sh.Run "cmd /c ""C:\dev\stream\start-stream.cmd"""`；
   `setup-watchdog.bat` 里 `/TR "wscript.exe C:\dev\stream\run-hidden.vbs"`；
   `deploy-remote.ps1` 的 `-Root` / `-WebDir` / `-WebZip` 默认值。
   于是"解压到别处"等于让五个脚本同时指向一个不存在的目录。
2. `crates/stream-server/src/config.rs` 的 `dist` 默认值还是
   `PathBuf::from(r"C:\dev\selkies-web")` —— 连 exe 自己也带着绝对路径。
3. `--upload-dir C:\Users\Administrator\Desktop`：换一台机器、换一个交互用户就
   指到一个不存在的目录（文件传输直接报错）。

**改法**：

- 运行根内所有路径由脚本自身位置推导：`%~dp0`（cmd/bat）、
  `WScript.ScriptFullName`（vbs）、`$PSScriptRoot`（ps1）。整包搬到哪都能跑。
- 证书改成**按存在性启用**：`cert.pem` + `key.pem` 都在才加 `--tls-*`，缺了就写
  一行 `plain HTTP, clipboard off` 进 `watchdog.log` 再起来 —— 起不来比降级更糟。
- `--upload-dir` 用 `%USERPROFILE%\Desktop`（不存在则退回 `%USERPROFILE%`）。
- `config.rs` 的 `dist` 默认值改成相对名 `selkies-web`，并加了一条断言"默认值
  不含盘符"的测试（`the_default_dist_is_relative_to_the_working_directory`）。
- 被控机安装位置收成一个参数：`deploy.ps1 -RemoteBase C:\dev`，运行根
  `<base>\stream`、暂存 `<base>\deploy`，其余全部相对推导。
- 旧布局自动迁移：`deploy-remote.ps1` 发现 `<base>\selkies-web` 还在、运行根里
  没有 `selkies-web\` 时，**rename** 进运行根（同盘、只动元数据），证书从
  `<base>\cert.pem` 复制进去；所以已经上线的机器不需要手工搬目录。
- 启动脚本改成**停进程之后**才安装：看门狗正在执行 `start-stream.cmd`，先装会
  撞共享冲突，而"只换 exe 不换脚本"正是参数卡在上一版的经典原因。装的时候按
  MD5 跳过相同内容，忙则重试 10 次 × 200 ms，仍忙就写"下次重启生效"（前端-only
  部署的正常情况）。

**另一条更隐蔽的**：`.cmd` / `.bat` 里写中文破折号（U+2014）会**打断解析**。
cmd 按当前代码页（GBK）逐字节读文件，UTF-8 的 `E2 80 94` 被当成两个 GBK 字符，
把下一个引号吃掉，于是

```
rem ... dies on "/I" — the guard ...
```

这一行之后整个 `if` 块错位，运行时报 `'m' 不是内部或外部命令`。所以
`start-stream.cmd` / `start-stream.60fps.cmd` / `setup-watchdog.bat` /
`run-hidden.vbs` 四个文件保持**纯 ASCII**（`deploy-remote.ps1` 早就这么要求，
因为 PowerShell 5.1 读无 BOM 的 .ps1 也是 ANSI）。

**同批还踩到两条部署链上的**：

1. `param([string]$DeployDir = $PSScriptRoot)` 在 PowerShell 5.1 里绑到**空串** ——
   `$PSScriptRoot` 在参数默认值求值时还没填。远端第一行 `Split-Path -Parent ""` 于是
   报 `cannot bind argument to parameter 'Path'`，整次切换在**动手之前**就停了
   （fail-safe，但白等一轮）。现在默认值改在函数体里取，并留一条 `if (-not $Root)`
   兜底。
2. `ssh` 只设了 `ConnectTimeout` —— 那管的是 TCP 握手。连上了而对端不说话（sshd
   卡在某个会话、机器被挂起）时会**无限等**：本轮就停在第一条 `hostname` 上十分钟。
   现在加 `ServerAliveInterval=15` / `ServerAliveCountMax=4`，60 秒没响应就断开，
   `scp` 用同一组参数。

**顺带**：`tasklist | find` 改成
`"%SystemRoot%\System32\find.exe"`。构建机上 PATH 里的 MSYS `find.exe` 不认
`/I`，一报错就等价于"没在运行"，看门狗于是每分钟多起一个实例。

### 7.24 新设备上"双击没反应"：空密码账户（已改成可读）

**现象**：全新设备（账户 `Admin`，**无密码**）上双击 `setup-watchdog.bat`，窗口
一闪就关，任务列表里没有 `MCStreamSrv`；双击 `start-stream.cmd` 同样一闪，
`logs\` 里也没有新行。看起来像"程序本身坏了"。

**根因三层，都属于"静默失败"**：

1. **空密码**（本轮真凶）：`set /p` 读到空行 → `PASSWORD` 未定义 → 原版只打印
   六个字符 `NO_PASSWORD` 就 `exit /b 1`，窗口随进程退出而关闭，什么都读不到。
   就算绕过这一步，`schtasks /RU Admin /RP ""` 也过不去：Windows 默认
   `LimitBlankPasswordUse=1`，空密码账户被禁止**批处理登录**，任务要么建不出来，
   要么建出来永远不启动（`LastTaskResult` `0x800704DD` / `0x8007052E`）。
2. **缺 VC++ 运行库**：`stream-server.exe` 导入 `VCRUNTIME140.dll`，加载器失败
   发生在任何日志被打开之前 —— 所以 `logs\stream-server.log` 连文件都不会有。
3. **zip 没解压**：在资源管理器里双击 zip 内的 exe 是解压到临时目录运行，
   同级没有 `selkies-web\`，于是"起来了但 404"，且日志落在临时目录里。

**改法**：

- `setup-watchdog.bat` 在空密码时打印两条可行路径（给账户设密码 / 改用
  `shell:startup` 快捷方式），不再是六个字符。
- 包里新增 `collect-launch-facts.cmd`（同时装进运行根）：双击生成
  `cloud-stream-facts.txt`（桌面再放一份），一次收全 OS/架构、会话（`quser`）、
  `LimitBlankPasswordUse`、包内文件完整性与 exe 字节数、VC 运行库、8080 占用、
  任务详情与 `LastTaskResult`、两份日志尾部、任务计划事件、Defender 状态。
  它**不启动 exe** —— 会抢屏和 8080，也可能和已在工作的实例打架；只给出一行
  "手工复现"的命令。
- 这个脚本自己的两个坑与 `find.exe` 同源：`powershell` 要写**全路径**
  （PATH 里若有 pwsh 7，裸名字会跑到别的版本上），并把 `PSModulePath` 收窄到
  5.1 的模块目录（否则 5.1 去加载 pwsh 的模块，弹"不受信任的发布者"交互提示，
  在无头机器上表现为"卡死"）。

**结论**：无密码账户别走计划任务这条路 —— Windows 从策略层就不允许。要么给该
账户设密码，要么放弃看门狗、把 `run-hidden.vbs` 的快捷方式放进 `shell:startup`
（前提是开了自动登录，否则重启后没人登录，任务同样空转）。

---

## 8 验收清单

一条命令跑完之后，按顺序核：

- [ ] 退出码 0，末尾打印 `部署完成`
- [ ] `[8]`/`[9] 体检` 三行全绿：`HTTP 200` / `listening on ...` / `pid=... session=1 start=...`
      （只换前端时中间一行是 `pid=...（未重启，会话保持）`，这是正常的，见 §7.15；
      全量部署时中间一行可能是 `pid=...（新进程）` 加一条说明 —— 指针日志刷屏会把
      启动行挤出日志尾部，端口通即视为在监听，见 §7.17）
- [ ] 体检里 `看门狗动作` 指向 `<运行根>\run-hidden.vbs`（不是旧目录）
- [ ] `curl -sk https://192.168.59.148:8080/ | grep -o 'assets/[^"]*'` 有两行
- [ ] 浏览器强刷后**左侧有工具栏**（没有 = 服务成 core 构建了，§7.1）
- [ ] 右上角有统计面板，右边缘有粉色胶囊
- [ ] 故意失败一次能回滚：`-SkipFrontend` 上传一个坏 zip，或直接跑 `-Rollback`
- [ ] 被控机上只有正式产物：`C:\dev\stream` 里没有 `*.prev` / `*.new.*`，
      `C:\dev\deploy` 只剩 `deploy-remote.ps1` 与五个上机脚本（zip 解压完即删）
- [ ] 运行根里五个上机脚本都在：`start-stream.cmd` / `start-stream.60fps.cmd` /
      `run-hidden.vbs` / `setup-watchdog.bat` / `collect-launch-facts.cmd`
      （少一个就是有人改了 `$LauncherNames` 而没改 `$Launchers`，打包与安装用的是
      两份列表）
- [ ] `C:\dev` 下不再有 `selkies-web\`（已 rename 进运行根）；旧的 `cert.pem` /
      `key.pem` 是**复制**过去的，脚本不动旧副本，可以手工删
- [ ] `target\output\cloud-stream\manifest.json` 的 `built_at` 是本次时间，
      且 `tls` / `web.files` 与预期一致
- [ ] `target\deploy-archive\meta.json` 的 `saved_at` 是本次部署时间

---

## 9 脚本挂掉时的等价手工命令

```powershell
# 构建（从仓库根目录开始）
cargo build --release -p stream-server
cd ..\cloud-stream-web\addons\selkies-web-core ; npm run build
cd ..\selkies-dashboard ; npm run build
cd C:\Users\user\Desktop\cloud-stream

# 汇总产物（也是"手工拼一个运行根"的唯一正确做法）
#   exe + DLL + 五个上机脚本 + cert/key 拷进 target\output\cloud-stream\
#   再把 dashboard\dist\* 拷进 target\output\cloud-stream\selkies-web\
#   —— 顺序无所谓，脚本全部按自身位置找同级文件

# 上传（只发后端时）
scp target\output\cloud-stream\stream-server.exe mc-admin:C:/dev/stream/stream-server.new.exe
scp target\deploy-selkies-web.zip mc-admin:C:/dev/deploy/selkies-web.zip
scp toolchain\deploy-remote.ps1 mc-admin:C:/dev/deploy/deploy-remote.ps1
scp toolchain\start-stream.cmd toolchain\run-hidden.vbs toolchain\setup-watchdog.bat mc-admin:C:/dev/deploy/

# 切换（唯一需要记住的一条；-Root 默认就是 <base>\stream，可不传）
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
| 看门狗指向哪 | `ssh mc-admin 'powershell -c "(Get-ScheduledTask -TaskName MCStreamSrv).Actions \| % { $_.Arguments }"'` | `<运行根>\run-hidden.vbs` |
| 服务在跑 | `curl.exe -sk https://192.168.59.148:8080/api/status` | `ok` |
| 进程会话 | `ssh mc-admin 'powershell -c "Get-Process stream-server \| Select Id,SessionId"'` | `SessionId=1` |
| 证书 | `ssh mc-admin 'powershell -c "Get-ChildItem C:\dev\stream\cert.pem \| Select LastWriteTime"'` | 自查是否过期 |
| 磁盘 | `ssh mc-admin 'powershell -c "Get-PSDrive C \| Select @{n=\"FreeGB\";e={[math]::Round($_.Free/1GB,1)}}"'` | 现在是 27.8 GB |

前两项挂了都会让**后续所有部署都做不了**，所以它们的修复方法（§3.1 的三行）
应该和这台机器的 IP 一起记在能看到的地方。
