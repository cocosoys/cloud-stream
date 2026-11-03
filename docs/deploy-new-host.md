# 换设备部署：要几台、每台做什么（#96）

> 本文回答一个问题：这套东西搬到**另一台（批）机器**上，一共要几台设备、每台要
> 做哪些一次性动作。日常改代码后的增量部署看 `deploy-one-click.md`（那篇讲的是
> 同一条命令内部怎么跑）。
>
> 核对时间：2026-10-30，现网构建机 `192.168.59.142`（SERVER001）、被控机
> `192.168.59.148`（CLIENT001，Win11 Pro / 4 vCPU / 8 GB / VMware SVGA 3D）。

---

## 0 结论先给

| 你要做的事 | 需要的设备数 |
|---|---|
| 只**运行**一整套推流 | **1 台**（被控机） |
| 把构建好的整包拷过去跑 | **1 台**（被控机），见 §3.0 |
| 用现成的 `deploy.ps1` 一条命令部署 | **2 台**（构建机 + 被控机） |
| 再增加一台被控机 | **+1 台**，构建机不加 |
| 观众 / 玩家端 | **0 台**（任意设备开浏览器，不装东西） |

三个角色的分工是硬的：

| 角色 | 台数 | 必须吗 | 职责 | 关键前提 |
|---|---|---|---|---|
| 构建 / 部署机 | 1 | 用 `deploy.ps1` 时是 | `cargo` 编 exe、`npm build` 编前端、`scp` 上传、体检 | Windows + MSVC + vcpkg + Node + ssh 客户端 |
| 被控机 | 1（可 N） | **是** | 跑 `stream-server.exe`，采集这个桌面并注入输入 | Windows 10/11 x64 + **有交互式已登录会话** |
| 浏览器终端 | 0（N 个） | 是但不部署 | Chrome / Edge 打开 `https://<被控机IP>:8080/` | 与被控机能互通 |
| TURN 中继 | 0 | 否 | — | 当前**没有**：`ice_servers = []`，跨 NAT 不通 |

「1 台」那条的前提是：被控机自己也装得下工具链，并且用**本机**方式部署（脚本
不走这条路径，得手工拷 exe + 重启计划任务）。所以现实里按 **2 台** 规划。

### 0.1 部署完成后，构建机能不能撤

能。**运行时只有被控机一台**，构建机是"只在出包那一刻存在"的设备。

| 运行时要什么 | 谁提供 | 是否依赖构建机 |
|---|---|---|
| 采集（DXGI）+ 编码（libvpx/opus）+ WebRTC 推流 | `stream-server.exe`，本机 | 否 |
| HTTPS 8080 + 静态前端 + 信令 + 文件传输 | 同一个进程，静态根是 `<运行根>\selkies-web`（相对工作目录，`config.rs`） | 否 |
| 输入注入（`SendInput`） | 同一进程，跑在会话 1 | 否 |
| 证书 | 本机 `<运行根>\cert.pem` / `key.pem`（自签） | 否 |
| 进程自愈 | 本机计划任务 `MCStreamSrv`，每分钟一次 | 否 |
| 前端构建产物 | 已在 `<运行根>\selkies-web\` | 否 |

代码里没有指向 `.142` 的地址、没有远程配置源、没有 CDN 依赖（`grep 192.168`
只命中 `health.rs` 的测试字面量）。所以构建机可以关机、可以搬走、可以换人。

三条代价，撤之前先认下：

1. **以后改一行代码也得把构建能力找回来**。`deploy.ps1` 只做"编 → 传 → 启"，
   它不会替你编。要么原构建机再开，要么任意一台装齐工具链的 Windows，要么把
   工具链整个搬到被控机上（那就回到"1 台"）。
2. **别把 sshd 也撤了**。22 是运维通道，不是运行时依赖——但没它就只能靠远程
   桌面/物理进入，而"抓不到桌面"这类故障恰恰是远程桌面也能看到的假象。
3. **虚拟化场景还多一层宿主**。现网被控机是 VMware 里的 Win11（SVGA 3D），
   宿主必须开着，否则虚拟机连同推流一起停。这层不计入"部署设备"，但算在
   "必须通电的机器"里。

---

## 1 被控机：一次性准备（11 项）

做完这 11 项，这台机器就能被 `pwsh toolchain/deploy.ps1 -Target <别名> -Hostname <IP> -Bootstrap` 接管。

| # | 项 | 怎么确认 / 怎么做 | 漏了会怎样 |
|---|---|---|---|
| 1 | Windows 10/11 **x64** | `systeminfo` | 采不了：DXGI/WASAPI/`SendInput` 都是 Windows API，且只有 Windows 构建 |
| 2 | 硬件基线 | 4 vCPU / 8 GB（现网基线，VP9 1080p60 约 2× CPU） | 掉帧、QoS 一路降档 |
| 3 | **有真实显示输出** | VMware SVGA 3D 或物理显示器；分辨率设成要推的尺寸 | 采集黑屏或退化成 1024×768 |
| 4 | **交互式已登录会话** | `quser` 看到 `administrator console 1 运行中` | 计划任务的 `/IT` 任务不启动；从服务/session 0 启动的进程抓到空桌面 |
| 5 | **开机自动登录**（可选但强烈建议） | `AutoAdminLogon=1` 或手工登录后再离开 | 重启后没人登录 → 1 分钟看门狗每次空跑，`/api/status` 不通 |
| 6 | VC++ 运行库 | `Test-Path C:\Windows\System32\vcruntime140.dll` | exe 起不来（缺 `VCRUNTIME140.dll`） |
| 7 | OpenSSH Server | 装 → `Set-Service sshd -StartupType Automatic` → `Start-Service sshd` → 放行 TCP 22 | 部署脚本第一步就超时（脚本会把这三行打出来） |
| 8 | 目录 | `C:\dev\stream\logs`、`C:\dev\deploy`、`C:\dev\stream\selkies-web` | `-Bootstrap` 会建，但 exe/前端仍要落位 |
| 9 | TLS 证书 | `C:\dev\stream\cert.pem` + `C:\dev\key.pem`，**SAN 里要有这台机器的新 IP** | 浏览器证书不匹配；剪贴板 API 直接不可用（非安全上下文） |
| 10 | 计划任务 `MCStreamSrv` | 在运行根里**以管理员身份**跑一次 `setup-watchdog.bat`（账户密码用参数给，或按提示输入）：它会顺手放行入站、建任务、立刻启动并打印 `[ok]` / `[fail]` | `task MISSING`，切换阶段失败 |
| 11 | **入站放行** | 管理员跑一次 `setup-inbound.cmd`，或让第 10 项代劳；判据是从别的机器 `ping <IP>` 有回、`curl -k https://<IP>:8080` 不是 TIMEOUT | 服务跑得好好的，但外部一律连不上：ping 无响应、8080 TIMEOUT（连 RST 都不回）。`start-stream.cmd` 每次启动会尝试补，但提权失败时只能记一行日志 |

额外两条只跟游戏有关，不属于推流链：

- 游戏本体 / Java / 服务端各自启动，推流重启不该碰它们（`deploy-notes-mc-coexist.md`）。
- 上传根 `--upload-dir` 现在是 `%USERPROFILE%\Desktop`（按运行账户推导，Desktop
  不存在则退回账户主目录），换机器一般不用改。

### 1.1 证书

`toolchain/cert-san.cnf` 里的 `CN` 与 `subjectAltName` 是**写死的 IP**（现在写着
`.148` 和 `.143`）。换机器有两步：

1. 改 `CN=` 和 `subjectAltName=IP:<新IP>`；
2. 重新签发，产物放 `<运行根>\cert.pem` / `key.pem`（默认 `C:\dev\stream\`）。

构建端也留一份、文件名为 `toolchain\cert-<IP末位>.pem` + `key-<IP末位>.pem`，部署
时会自动打进整包；装到被控机时**只在被控机没有证书时**才落地（SAN 是那台机器的
IP，拿构建端的覆盖等于把 HTTPS 换成浏览器报警）。

脚本只检查文件在不在，**不检查是否过期、是否匹配这台机器的 IP**。

### 1.2 网络

| 端口 | 用途 | 现状 |
|---|---|---|
| TCP 22 | 部署（ssh/scp） | 被控机防火墙**三档全关**，所以没有显式规则也能连 |
| TCP 8080 | HTTPS 页面 + WS 信令 | 同上 |
| UDP（临时） | WebRTC 媒体，ICE **host** 候选，端口随机 | 服务端出站发起连通性检查即可回包，同网段不需要入站规则 |

**跨 NAT 不行**：`webrtc/mod.rs` 里 `ice_servers = []`（照 hbb_common 的写法），
只有 host candidate。要跨公网必须另配 STUN/TURN —— 前端仓库里有 `coturn` 目录，
但本仓库没有接。

**没有鉴权**：监听面 22 个入口默认 `--policy allow-all`（令牌解析待做，见
`roadmap-l2-l3-gaps.md`）。所以「部署到其他设备」的边界是**内网/可信网段**。
要跨网，先做鉴权，别先做端口映射。

### 1.3 免密 ssh

构建机的公钥（现网是 `~/.ssh/id_ed25519_mc.pub`）要进被控机的
`C:\ProgramData\ssh\administrators_authorized_keys`（注意这个文件名，管理员组的
密钥不走 `authorized_keys`），并在 `~/.ssh/config` 里加一个别名：

```sshconfig
Host mc-admin-2
  HostName 192.168.59.15x
  User Administrator
  IdentityFile ~/.ssh/id_ed25519_mc
  StrictHostKeyChecking accept-new
```

---

## 2 构建 / 部署机：一次性准备

| 依赖 | 版本 / 位置 | 备注 |
|---|---|---|
| Rust | **1.75** + `x86_64-pc-windows-msvc` | `crates/stream-server/Cargo.toml` 的 `rust-version` |
| VS 2022 Build Tools | C++ 工作负载 + Windows SDK | `deploy.ps1` 会自己拼 `PATH/INCLUDE/LIB`，`vswhere` 找不到时写死 `BuildTools` 路径 |
| vcpkg | `C:\dev\vcpkg`，三元组 `x64-windows-static` | 需要 `libyuv` 与 `libvpx[realtime]`，走本仓库 `vcpkg-ports/` 的 overlay |
| LLVM | `LIBCLANG_PATH=C:\dev\llvm\bin` | `bindgen` 用 |
| CMake + Ninja + Python | — | vcpkg 构建期用 |
| libopus 静态库 | `C:\dev\opus-static`（`LIBOPUS_LIB_DIR`） | 不是 vcpkg 装的；`/MT` 静态 CRT。缺了链接阶段报找不到 opus |
| Node.js | 本机 v22.22.2 构建通过 | 前端两条构建：`core` → `dashboard`，顺序不能反 |
| PowerShell 7 | `pwsh` | 脚本用了 `??`、`-not` 等 pwsh 语义 |
| ssh / scp / curl.exe | — | `curl.exe` 而不是 `Invoke-WebRequest`（免得挑 PowerShell 版本） |

路径写死的地方（换构建机要一起改）：`toolchain/build-ws.cmd`、
`toolchain/install-vcpkg-libs.cmd`、`toolchain/env-setup.ps1`、`deploy.ps1` 的
`Initialize-BuildEnv`。前端仓库必须在**后端仓库同级目录**（`cloud-stream-web` 或
`selkies-webclient`），否则第一步就 `throw`，或用 `-WebRoot` 指路。

---

## 3 部署到新机器：命令

### 3.0 只有一台机器时：拷整包

构建端跑一次部署（或只跑到「汇总产物」）就会得到 `target\output\cloud-stream`
—— 这个目录本身就是运行根，脚本全部按自身位置推导路径，没有写死的 `C:\dev`。

**新机器上先确认四件事**（缺了不是"跑不起来"，是"看起来跑起来了但没人能用"）：

| 项 | 怎么查 | 缺了会怎样 |
|---|---|---|
| Windows 10/11 **x64** | `systeminfo` | 无法运行：DXGI / WASAPI / `SendInput` 都是 Windows API，且只有 Windows 构建 |
| VC++ 运行库 | `Test-Path C:\Windows\System32\vcruntime140.dll` | 双击后窗口一闪即退；`stream-server.exe` 导入表里有 `VCRUNTIME140.dll`，缺了加载器就拒绝启动（日志里一行都没有） |
| **交互式已登录会话** | `quser` 看到 `console 1 运行中` | 计划任务的 `/IT` 任务不启动；从服务/session 0 起进程抓到空桌面 |
| **账户有密码** | `net user %USERNAME%` 看 `Password required` | 看门狗建不出来或永远不启动：Windows 默认禁止空密码账户**批处理登录**（`LimitBlankPasswordUse=1`），`schtasks /RP ""` 直接失败。无密码机器见本节末尾的启动文件夹方案 |
| 真实显示输出 | VMware SVGA 3D 或物理显示器，分辨率设成要推的尺寸 | 黑屏，或退化成 1024×768 |
| **入站已放行** | 从别的机器 `ping <IP>`、`curl -k https://<IP>:8080` | 全部 TIMEOUT（连 RST 都不返回）就是整档被丢：服务跑得再好也连不上。管理员跑一次 `setup-inbound.cmd` |

**五步**（0–1 是包与解压，2–4 在新机器上做）：

```powershell
# 0. 取包：target\output\cloud-stream\（目录）或 cloud-stream.zip（8.17 MB）
#    从网上 / IM 拿到的 zip 先解锁，否则 SmartScreen 会拦 exe 与 .cmd：
Unblock-File .\cloud-stream.zip

# 1. 解压到任意位置，例如 D:\stream（纯 ASCII 短路径最省事）
#    判据：stream-server.exe 与 start-stream.cmd 在同一层

# 2. 证书 —— 包里那对 PEM 的 SAN 是 192.168.59.148 / .143
#    a) 新机器 IP 就是这两个之一：不用动
#    b) 只是先看看画面、不介意告警：留着，浏览器点"继续前往"（剪贴板不可用）
#    c) 要剪贴板：在构建机重签，两份 PEM 覆盖运行根里的同名文件
#         openssl req -x509 -newkey rsa:2048 -nodes -days 825 ^
#           -keyout key.pem -out cert.pem -config toolchain\cert-san.cnf
#       （先把 cert-san.cnf 的 CN 与 subjectAltName 改成新 IP。签出来仍会提示
#        "不受信任"——自签就是这样，点继续即可，但上下文已是安全的，剪贴板可用）
#    注意：删掉 cert.pem/key.pem 不是捷径 —— 那会退回明文 HTTP，剪贴板**照样**
#    不可用（Clipboard API 要安全上下文），只是少了一个告警。

# 3. 初始化：右键 setup-watchdog.bat -> "以管理员身份运行"
#    它一次做三件事，全都在这个窗口里给出结果：
#      a) 放行 8080/TCP 与 ICMP echo（提权才做得到）
#      b) 建 MCStreamSrv 任务：wscript.exe "D:\stream\run-hidden.vbs"
#      c) 立刻 schtasks /Run 一次，最多等 60 秒，然后打印 [ok] 或 [fail]
#    也可以给参数：setup-watchdog.bat Administrator <密码>
#    密码是这台机器**该账户的登录密码**，不是这里新设的什么密码

# 4. 浏览器打开 https://<本机IP>:8080/
#    窗口里已经告诉你 [ok] 了；若还要复核，见下面的三条命令
```

第 3 步的 `[ok]` 意味着**本机 loopback 上的健康检查已返回 200**，也就是服务真的
起来了。若它说 `[fail]`，窗口里会直接给出下一步（看哪两个日志、跑哪个脚本），
不用猜。

**如果第 3 步是以普通权限双击的**（没有"以管理员身份运行"）：任务多半建得出来，
但入站规则加不上，脚本会打印 `[inbound]` 警告并在 `[ok]` 后面提醒。补一次即可：

```powershell
# 管理员 PowerShell 或右键"以管理员身份运行"
D:\stream\setup-inbound.cmd
```

等价的单行命令（管理员）：

```powershell
Remove-NetFirewallRule -DisplayName 'cloud-stream*' -ErrorAction SilentlyContinue
New-NetFirewallRule -DisplayName 'cloud-stream 8080' -Direction Inbound `
  -Protocol TCP -LocalPort 8080 -Action Allow -Profile Any
New-NetFirewallRule -DisplayName 'cloud-stream ICMPv4' -Direction Inbound `
  -Protocol ICMPv4 -IcmpType 8 -Action Allow -Profile Any
```

`-Profile Any` 不是偷懒：VMware NAT / host-only 网卡经常被识别成"公用"，
限定 `Private` 的规则在那里**静默不生效**。判断依据：

```powershell
Get-NetConnectionProfile | Select-Object InterfaceAlias, NetworkCategory
```

### 3.0.1 之后每次启动，脚本自己会做这些

不需要人守在那里，也不需要记得"先放行再启动"：

| 时机 | 动作 | 结果写在哪 |
|---|---|---|
| `start-stream.cmd` 跑起来（看门狗每分钟一次，但只在真的要起进程时执行） | 查 `cloud-stream 8080` / `ICMPv4` 两条规则，缺了就补（提权才有权限；没权限只记一行） | `logs\watchdog.log` 的 `PREFLIGHT` 行 |
| exe 启动后 | 另起 `probe-listen.cmd`，每 5 秒探一次 loopback，最多 30 秒 | 同上：`PREFLIGHT listening on 8080 after N probe(s)` 或 `PREFLIGHT NOT listening ...` + 三条最可能原因 + 下一步 |
| `setup-watchdog.bat` 跑完 | 建任务 → 立刻 `/Run` → 最多等 60 秒 → 打印 `[ok]` / `[fail]` | 屏幕上，窗口就是结果 |
| `collect-launch-facts.cmd` | 第 9 节专门列入站：网络配置文件、本机 IPv4、两条规则的实际参数、最近 12 条 `PREFLIGHT` 行 | `cloud-stream-facts.txt` |

所以"启动后发现没效果"必然留下痕迹：先看 `logs\watchdog.log` 里最后一条
`PREFLIGHT`，它要么说 `listening on 8080`（那就是网络/防火墙），要么直接给出
"8080 被占用 / 进程 panic / 加载器失败"三选一。

**不建任务的最小路径**：双击 `start-stream.cmd`。它开一个黑窗口跑
`stream-server.exe`（关窗口 = 停服务），崩溃不会自愈。它同样会做上面那套
preflight，所以窗口里看不到异常时，日志里也有判决。适合先验证画面，再回头
建任务。

**账户没有密码时**：计划任务这条路走不通（策略层限制，不是脚本问题）。
要么先 `net user Admin *` 给账户设个密码再建任务；要么放弃看门狗，把
`run-hidden.vbs` 的快捷方式丢进 `shell:startup`（需要开自动登录，否则重启后
没人登录，什么都起不来）：

```powershell
# 无密码机器的替代方案：登录后自动起（不是服务，挂了不会自愈）
$lnk = "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\Startup\cloud-stream.lnk"
$s = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk)
$s.TargetPath = 'D:\stream\run-hidden.vbs'
$s.WorkingDirectory = 'D:\stream'
$s.Save()
```

**起不来先双击 `collect-launch-facts.cmd`**：它生成 `cloud-stream-facts.txt`
（桌面也放一份），把 OS/会话/账户策略/包完整性/VC 运行库/端口/任务状态/
`LastTaskResult`/两份日志尾部/任务计划事件/Defender/入站规则一次收全。它不启动
exe，只给你一行手工复现的命令。

**跑起来之后核这三条**：

```powershell
curl.exe -sk -o NUL -w '%{http_code}' https://<新IP>:8080/api/v1/health   # 200
Get-Content D:\stream\logs\watchdog.log -Tail 6                            # PREFLIGHT listening ... / starting ... from D:\stream
Get-Content D:\stream\logs\stream-server.log | Select-String 'listening on' # https://0.0.0.0:8080
```

页面 404 → `selkies-web\` 没解压全（`selkies-web\index.html` 必须在）；
页面能开但黑屏 → 进程在 session 0，重跑 `setup-watchdog.bat`；
任务"运行过"但端口不通 → 任务动作指向别的目录，重跑 `setup-watchdog.bat`；
本机 `PREFLIGHT listening` 但别的机器打不开 → 入站规则或网络配置文件，
`setup-inbound.cmd`。

要改分辨率 / 帧率 / 码率：编辑运行根里的 `start-stream.cmd`（默认
1920×1080 / 60 fps / 2500 kbps），然后重启任务。改完别动计划任务本身。

这一路不需要构建机、不需要 ssh、不需要工具链。包里的 `README.txt` 就是三步版
（解压 → `setup-watchdog.bat` → 浏览器）。

### 3.1 有构建机时：一条命令

```powershell
# 第一次搬上一台新被控机
pwsh toolchain/deploy.ps1 -Target mc-admin-2 -HostName 192.168.59.15x -Bootstrap

# 之后日常（后端 + 前端）
pwsh toolchain/deploy.ps1 -Target mc-admin-2 -HostName 192.168.59.15x

# 装到别的盘 / 别的目录
pwsh toolchain/deploy.ps1 -RemoteBase D:\cloud
```

被控机上只有**一个**常量：安装根 `-RemoteBase`（默认 `C:\dev`）。运行根是
`<base>\stream`，部署暂存是 `<base>\deploy`；运行根内部的脚本、exe 默认值
（`--dist selkies-web`）、计划任务动作全部相对推导，所以换了 `-RemoteBase`
之后不需要再改任何脚本。

`-Bootstrap` 只做四件事：sshd 自启、建目录、推八个上机脚本并装进运行根、核对
exe / 任务 / 证书 / 旧布局。**它不建计划任务、不签证书、不动密码** —— 那三件是
人做的（见 §1 的第 9、10 项和 §1.3）。

---

## 4 验收 5 条（部署完按顺序核）

```powershell
curl.exe -sk -o NUL -w '%{http_code}' https://<IP>:8080/api/status      # 200
curl.exe -sk https://<IP>:8080/api/v1/health                            # ok:true, verdict
curl.exe -sk https://<IP>:8080/ | Select-String 'assets/index-'         # 两行（dashboard 构建）
ssh <别名> "powershell -c \"(Get-Process stream-server).SessionId\""     # 1，不是 0
```

浏览器打开后：左侧工具栏在、右上统计面板在、鼠标能动、剪贴板能同步（剪贴板同时
验证了 HTTPS 生效）。

---

## 5 换设备最容易踩的 10 条

| 现象 | 原因 | 处置 |
|---|---|---|
| 页面能开、画面全黑 | 进程跑在 session 0（服务/schtasks 没加 `/IT`），抓的是空桌面 | 部署体检打印 `session=0` 就是这条；用 `setup-watchdog.bat` 建任务 |
| 重启之后 8080 不通 | 没人登录 → `/IT` 任务不启动；或 `sshd` 是「手动」 | 开自动登录；`Set-Service sshd -StartupType Automatic` |
| 页面打不开但 ssh 正常 | 新机器防火墙是开的，8080 没放行 | 管理员跑 `setup-inbound.cmd`（22 也要放行） |
| **服务在跑、日志也写了 `starting`，但外部完全连不上** | 入站被整档丢弃：Public 配置文件默认丢全部入站（含 ICMP），VMware NAT / host-only 网卡常被判成 Public。表现是 `ping` 无响应、8080 **TIMEOUT**（不是 REFUSED） | 先看 `logs\watchdog.log` 最后一条 `PREFLIGHT`：若是 `listening on 8080`，问题就在网络层 —— 管理员跑 `setup-inbound.cmd`；`Get-NetConnectionProfile` 确认不是被 `Private` 规则挡在 Public 上 |
| 日志里只有 `starting`，没有后半句 | 包内缺 `probe-listen.cmd`（旧包），或 `start-stream.cmd` 是旧版 | 用当前整包覆盖运行根；新版每次启动都会写 `PREFLIGHT` 判决 |
| `setup-watchdog.bat` 打印 `[fail] nothing listening ... after 60 s` | 8080 被占用 / exe panic / 缺 VC++，三者之一 | 窗口里已列出要看的两份日志；或跑 `collect-launch-facts.cmd` |
| 证书报警、剪贴板失灵 | SAN 还是旧 IP；或根本没配证书（退回明文 http） | 按 §1.1 重签；剪贴板 API 要求安全上下文 |
| 信令连上、视频不出 | 跨网段/跨 NAT，`ice_servers=[]` 只有 host candidate | 同网段部署，或后续接 STUN/TURN |
| 指针灵敏度/界面缩放改了没反应还报错 | 服务用**非游戏账号**跑，改的是另一个 HKCU | `/RU` 必须是游戏所在的交互用户（现网 `Administrator`） |
| 任务显示"运行过"，但页面 404 / 日志空 | 整包被搬到了别的目录，计划任务里还存着旧路径 | 在新目录重跑一次 `setup-watchdog.bat`；部署体检会把动作路径打出来 |

---

## 6 多台被控机怎么管

- 同一份整包（`target\output\cloud-stream.zip`）可以发到所有机器；`-SkipBuild`
  时也能直接复用上一轮的产物。
- 每台机器各自的：证书（IP 不同）、计划任务（账户密码不同）、`start-stream.cmd`
  里的分辨率 / 码率参数（`--upload-dir` 已按账户推导，通常不用改）。
- 现在**没有**集中管理面：每台一套 `-Target`，靠 ssh 别名区分；`/api/v1/health`
  是单机自述，不做聚合。
