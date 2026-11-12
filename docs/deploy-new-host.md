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
| 4 | **交互式已登录会话** | `quser` 看到 `administrator console 1 运行中` | 任务不会触发（它只在登录会话里跑）；从服务/session 0 启动的进程抓到空桌面 |
| 5 | **开机自动登录**（可选但强烈建议） | `AutoAdminLogon=1` 或手工登录后再离开 | 重启后没人登录 → 1 分钟看门狗每次空跑，`/api/status` 不通 |
| 6 | VC++ 运行库 | `Test-Path C:\Windows\System32\vcruntime140.dll` | exe 起不来（缺 `VCRUNTIME140.dll`） |
| 7 | OpenSSH Server | 装 → `Set-Service sshd -StartupType Automatic` → `Start-Service sshd` → 放行 TCP 22 | 部署脚本第一步就超时（脚本会把这三行打出来） |
| 8 | 目录 | `C:\dev\stream\logs`、`C:\dev\deploy`、`C:\dev\stream\selkies-web` | `-Bootstrap` 会建，但 exe/前端仍要落位 |
| 9 | TLS 证书 | `C:\dev\stream\cert.pem` + `C:\dev\key.pem`，**SAN 里要有这台机器的新 IP** | 浏览器证书不匹配；剪贴板 API 直接不可用（非安全上下文） |
| 10 | 计划任务 `MCStreamSrv` | 在运行根里**以管理员身份**跑一次 `stream-setting.exe --install`（不需要账户密码）：它会顺手放行入站、建任务、立刻启动并打印 `[ok]` / `[fail]` | `task MISSING`，切换阶段失败 |
| 11 | **入站放行** | 管理员跑一次 `stream-setting.exe --inbound`，或让第 10 项代劳；判据是从别的机器 `ping <IP>` 有回、`curl -k https://<IP>:8080` 不是 TIMEOUT | 服务跑得好好的，但外部一律连不上：ping 无响应、8080 TIMEOUT（连 RST 都不回）。`stream-server.exe --run` 每次启动会尝试补，但提权失败时只能记一行日志 |

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
| TCP 8080 | HTTPS 页面 + WS 信令 + 文件传输 | `stream-setting.exe --inbound` 建 `cloud-stream 8080`（profile=any） |
| UDP 40000-40049 | WebRTC 媒体，ICE **host** 候选 | `cloud-stream webrtc UDP`；区间由 `--webrtc-udp-port-min/max` 定（#122） |
| 任意 TCP/UDP | `stream-server.exe` 本体 | `cloud-stream app` 程序规则，按路径匹配，覆盖改区间/换端口 |
| TCP 22 | 部署（ssh/scp） | **不随包放行**；要远程部署才手动开 |

**为什么 WebRTC 必须单独放行**：ICE 的 host 候选每个本机地址绑一个 UDP socket，
库的默认值是 `:0` —— 每个会话一个随机端口，任何按端口的规则都追不上它。#122 把
区间钉死（默认 40000-40049），规则才写得出来。**这是最容易漏的一条**：WS 模式只
用 TCP 8080，所以一台机器可以画面完全正常、一切切到 WebRTC 就报"媒体路径失败"。

早期文档写过"服务端出站发起检查即可回包，同网段不需要入站规则"，**这句是错的**：
浏览器自己也会发检查，那些包是**无状态入站**，不在任何出站流的回程里；而只有
浏览器自己的检查成功，它才会把 `connectionState` 报成 `connected`。.148 上没暴露，
是因为那台机器的防火墙**三档全关**（上表第一列已改），全新设备默认是开着的。

**跨 NAT 不行**：`webrtc/mod.rs` 里 `ice_servers = []`（照 hbb_common 的写法），
只有 host candidate。要跨公网必须另配 STUN/TURN —— 前端仓库里有 `coturn` 目录，
但本仓库没有接。固定端口区间是那一步的前提：随机端口连端口映射都配不出来。

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

### 3.0 只有一台机器时：拷一个 exe

构建端跑一次部署（或只跑到「汇总产物」）会得到两样东西：

* `target\output\stream-setting.exe` —— **约 9 MB 的单文件**，整个运行包嵌在里面。
  装一台新机器只需要把它拷过去双击。
* `target\output\cloud-stream\` —— 解开后的运行根（目录）与 `cloud-stream.zip`，
  那是给"手工摆位 / 现场调试"用的，不是安装路径。

**新机器上先确认四件事**（缺了不是"跑不起来"，是"看起来跑起来了但没人能用"）：

| 项 | 怎么查 | 缺了会怎样 |
|---|---|---|
| Windows 10/11 **x64** | `systeminfo` | 无法运行：DXGI / WASAPI / `SendInput` 都是 Windows API，且只有 Windows 构建 |
| VC++ 运行库 | `Test-Path C:\Windows\System32\vcruntime140.dll` | 双击后窗口一闪即退；`stream-server.exe` 导入表里有 `VCRUNTIME140.dll`，缺了加载器就拒绝启动（日志里一行都没有） |
| **交互式已登录会话** | `quser` 看到 `console 1 运行中` | 任务建得出来但不会跑；从服务/session 0 起进程抓到空桌面 |
| ~~账户有密码~~ | `net user %USERNAME%` 看 `Password required` | **不再是前提**（2026-11 改）：任务改用交互式令牌注册，不存凭据、不做批处理登录，空密码账户同样可用 |
| 真实显示输出 | VMware SVGA 3D 或物理显示器，分辨率设成要推的尺寸 | 黑屏，或退化成 1024×768 |
| **入站已放行** | 从别的机器 `ping <IP>`、`curl -k https://<IP>:8080` | 全部 TIMEOUT（连 RST 都不返回）就是整档被丢：服务跑得再好也连不上。管理员跑一次 `stream-setting.exe --inbound` |

**三步**（0 取文件，1 在新机器上点一下，2 开浏览器）：

```powershell
# 0. 只拷 target\output\stream-setting.exe 这一个文件（约 9 MB）
#    从网上 / IM 拿到的先解锁，否则 SmartScreen 会拦：
Unblock-File .\stream-setting.exe
#    想先看它打算做什么、一个文件都不动：stream-setting.exe --plan（不需要管理员）

# 1. 双击 stream-setting.exe
#    窗口先显示这台机器当前的状态，点「安装 / 修复」。它会自己弹 UAC，然后：
#      a) 把内嵌的包解压到 %LOCALAPPDATA%\cloud-stream（--dir 可改）
#      b) 把自己复制到那个目录 —— 之后那儿也有一份控制台
#      c) 放行 TCP 8080、ICMP echo、UDP 40000-40049 与程序规则
#      d) 建 MCStreamSrv 任务：<运行根>\stream-server.exe --run
#      e) 立刻启动一次，最多等 60 秒，结论贴回窗口的日志面板
#
#    **不需要任何账户密码**。任务注册成"仅当用户登录时运行"
#    （InteractiveToken）：它复用当前登录会话已经持有的令牌，不做登录、
#    不存凭据，也不读改 SAM 里的任何东西。空密码账户照样能建、能跑。
#    任务落在**控制台上登录的那个账户**上，而不是"以管理员身份运行"时
#    临时提权的那个身份 —— 因为桌面属于前者。
#
#    没有窗口的机器（纯 ssh）走同一条命令：
#      stream-setting.exe --install --yes --dir D:\stream

# 2. 浏览器打开 https://<本机IP>:8080/
#    窗口里已经说了 [ok]；要复核的，见下面的三条命令
```

安装的 `[ok]` 意味着**本机 loopback 上的健康检查已返回 200**，也就是服务真的
起来了。若是 `[fail]`，窗口里会直接给出下一步（看哪两个日志、跑哪条命令），
不用猜。整条链的细节与"删不掉怎么办"见 `operations-install.md`。

**证书**：包里那对 PEM 的 SAN 是 `192.168.59.148` / `.143`（`toolchain/cert-san.cnf`）。
新机器 IP 不是它们时，最省事的做法是装完把两份 PEM 覆盖到运行根里重签的版本
（`openssl req -x509 -newkey rsa:2048 -nodes -days 825 -keyout key.pem -out cert.pem -config toolchain\cert-san.cnf`，
先把 CN 与 subjectAltName 改成新 IP）。签出来仍会提示"不受信任"—— 自签就是这样，
点继续即可，但上下文已是安全的，剪贴板可用。**删掉 PEM 不是捷径** —— 那会退回
明文 HTTP，剪贴板**照样**不可用（Clipboard API 要安全上下文），只是少了一个告警。

**要不要口令**（默认没有：同网段任何人打开地址就能操作键鼠）：

最省事的是双击运行根里的 `stream-setting.exe`，点「改配置」，找到"访问口令"
那一格填一个、保存，然后重启任务（改配置只写文件，重启才生效）。不想点窗口
就直接编辑 `stream-config.toml`：

```toml
auth_token = 'x'      # 把 x 换成你的口令；'…' 是 TOML 单引号字符串
```

保存后重启任务：

```cmd
schtasks /End /TN MCStreamSrv
schtasks /Run /TN MCStreamSrv
```

之后浏览器必须带 token 打开一次：

```
https://<本机IP>:8080/?token=<口令>
```

前端会把它存进 cookie，同一浏览器之后直接输地址就行。

口令**不再限 ASCII**：以前它是 `set-password.cmd` 写的纯文本文件，要经过 cmd
的 OEM 代码页，中文进去就变了。现在它走 TOML（UTF-8），非 ASCII 没问题；写进
文件时会被转义，含引号、反斜杠、空格都可以。

口令是明文存在 `stream-config.toml` 里的，那份文件别提交到公开仓库。文档里
还留着一个老名字 `auth-token.txt`：老机器升级上来时仍然认它，但**它的优先级
低于配置** —— 用表单设过之后，那个文件就可以删了。

细节见 `deploy-one-click.md` §7.26。

**入站规则没加上怎么办**（UAC 被拒、或只提权了安装没提权规则）：补一次即可，
不会碰任何文件：

```powershell
# 管理员 PowerShell，或右键"以管理员身份运行"
D:\stream\stream-setting.exe --inbound --dir D:\stream
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
| `stream-server.exe --run` 跑起来（看门狗每分钟一次，但只在真的要起进程时执行） | 查 `cloud-stream 8080` / `ICMPv4` 两条规则，缺了就补（提权才有权限；没权限只记一行） | `logs\watchdog.log` 的 `PREFLIGHT` 行 |
| exe 启动后 | 自己另起 `--probe` 子进程，每 2.5 秒探一次 loopback，最多 30 秒 | 同上：`PREFLIGHT listening on 8080 after N probe(s)` 或 `PREFLIGHT NOT listening ...` + 三条最可能原因 + 下一步 |
| 安装器跑完（`stream-setting.exe --install`） | 建任务（交互式令牌，无密码）→ 立刻启动 → 最多等 60 秒 → 结论贴回窗口日志面板，同时写 `%ProgramData%\cloud-stream\last-install.txt` | 窗口面板 + 那个文件 |
| `stream-server.exe --diagnose` | 九节现场取证：OS/会话、账户策略、包完整性、VC 运行库、端口与任务、两份日志尾部、任务计划事件、Defender、入站可达性（含最近 12 条 `PREFLIGHT`） | `cloud-stream-facts.txt`（运行根 + 桌面各一份） |

所以"启动后发现没效果"必然留下痕迹：先看 `logs\watchdog.log` 里最后一条
`PREFLIGHT`，它要么说 `listening on 8080`（那就是网络/防火墙），要么直接给出
"8080 被占用 / 进程 panic / 加载器失败"三选一。

**不建任务的最小路径**：在运行根敲 `stream-server.exe --run`。它不开窗口跑
`stream-server.exe`（关窗口 = 停服务），崩溃不会自愈。它同样会做上面那套
preflight，所以窗口里看不到异常时，日志里也有判决。适合先验证画面，再回头
建任务。

**账户没有密码时**：照常建任务，不需要给账户设密码。任务用的是
`InteractiveToken`（"仅当用户登录时运行"）—— 复用会话已有的令牌，不做批处理
登录，所以 `LimitBlankPasswordUse=1` 根本不参与。这一条 2026-11 改过，
老版本的 `schtasks /RP` 才要求密码，现在的安装器不碰它。

真正的前提只有一条：**那个会话是登录着的**。冷启动停在锁屏时，没有桌面可抓，
任务也不会触发；要么人工登录一次，要么开自动登录（`Autologon` 用 LSA 加密存
口令，比在注册表里写明文强）。

**不想建任务的最小路径**：在运行根敲 `stream-server.exe --run`。它不开窗口跑
`stream-server.exe`（关窗口 = 停服务），崩溃不会自愈。它同样会做上面那套
preflight，所以窗口里看不到异常时，日志里也有判决。适合先验证画面，再回头
建任务。把它放进 `shell:startup` 也能开机自起，但同样只在登录后发生、且没有
自愈——这就是为什么要建任务。

**起不来先跑 `stream-server.exe --diagnose`**：它生成 `cloud-stream-facts.txt`
（桌面也放一份），把 OS/会话/账户策略/包完整性/VC 运行库/端口/任务状态/
`LastTaskResult`/两份日志尾部/任务计划事件/Defender/入站规则一次收全。它不启动
exe，只给你一行手工复现的命令。工具箱里也能转调它（`stream-setting.exe
--diagnose`），敲哪个名字都行 —— 两个 exe 躺在同一个目录里。

**跑起来之后核这三条**：

```powershell
curl.exe -sk -o NUL -w '%{http_code}' https://<新IP>:8080/api/v1/health   # 200
Get-Content D:\stream\logs\watchdog.log -Tail 6                            # PREFLIGHT listening ... / starting ... from D:\stream
Get-Content D:\stream\logs\stream-server.log | Select-String 'listening on' # https://0.0.0.0:8080
```

页面 404 → `selkies-web\` 没解压全（`selkies-web\index.html` 必须在）；
页面能开但黑屏 → 进程在 session 0，重跑一次 `stream-setting.exe --install`；
任务"运行过"但端口不通 → 任务动作指向别的目录，重跑一次 `stream-setting.exe --install`；
本机 `PREFLIGHT listening` 但别的机器打不开 → 入站规则或网络配置文件，
`stream-setting.exe --inbound`。

要改分辨率 / 帧率 / 码率：编辑运行根里的 `stream-config.toml`，或用
`stream-setting.exe` 的「改配置」按钮（默认
1920×1080 / 60 fps / 2500 kbps），然后重启任务。改完别动计划任务本身。

这一路不需要构建机、不需要 ssh、不需要工具链。包里的 `README.txt` 就是三步版
（拷 exe → 双击 → 浏览器）。

### 3.0.2 把一台机器退回去：`stream-setting.exe --uninstall`

同一个目录里的 `stream-setting.exe`，双击后点「卸载」即可（它会自己提权，然后
**把窗口关掉** —— 那个窗口占着目录里的 exe，不退出谁都删不掉）。顺序是排过的，
不是想起哪个做哪个：

| 步骤 | 动作 | 为什么是这个顺序 |
|---|---|---|
| 1 | 停任务 + 注销 `MCStreamSrv` | 看门狗每分钟一次；先杀进程的话它会在这中间把服务拉回来，然后 exe 被占用、目录删不掉。删不掉时先 `--disable`：禁用之后它再也拉不起服务，所以"任务残留 + 目录照删"是安全的 |
| 2 | 杀掉**本目录**下的 `stream-server.exe` | 按完整路径前缀过滤，另一份安装在别处的不会被误杀 |
| 3 | 还原默认录音设备 | 服务启动时会把它切到虚拟麦，而进程从来是被强杀的；原值只在 `logs\mic-default.json`，这个文件下一步就随目录消失 |
| 4 | 删四条入站规则（含程序规则） | 规则名是机器级的，所以也会清掉"另一份副本"用的那几条。端口不猜 —— 问 `--print-config json`，改过端口的机器也能清干净 |
| 5 | 删整个目录 | 删不掉自己：先复制到 `%TEMP%`，用 `--purge --dir … --wait <pid>` 拉起替身，本进程退出；替身等 pid 消失后动手 |

删不干净时它**不会假装成功**：目录留着，弹一个框把卡住的那个文件路径交给你
（`另一个程序正在使用此文件`），结论文件里也写着。

任务是"按目录认领"的：如果 `MCStreamSrv` 的动作指向别的目录，它会**跳过**并
告诉你那个目录在哪（要强删加 `--force`）。另外三个模式：

```cmd
stream-setting.exe --uninstall --plan     :: 只列清单，什么都不改、不弹框（不需要管理员）
stream-setting.exe --uninstall --whatif   :: 同一份清单，弹框显示
stream-setting.exe --uninstall --keep     :: 卸服务、留文件（换新版本重装时用）
```

卸载前会把"这台机器的身份"（配置、证书、录音设备原值，几 KB）备份到
`%ProgramData%\cloud-stream\uninstall-backup`；下次 `--install` 发现它在，
就用 `--ops rollback` 放回去，所以"卸载 → 重装"不会静默丢掉端口和口令。

不碰的东西：`--upload-dir` 指向的桌面目录里的文件、虚拟声卡驱动、VC++ 运行库。

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

`-Bootstrap` 只做四件事：sshd 自启、建目录、推九个上机脚本并装进运行根、核对
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
| 页面能开、画面全黑 | 进程跑在 session 0（注册成服务、或任务没用交互式令牌），抓的是空桌面 | 部署体检打印 `session=0` 就是这条；重跑 `stream-setting.exe --install` 建任务 |
| 重启之后 8080 不通 | 没人登录 → 交互式任务不触发；或 `sshd` 是「手动」 | 开自动登录；`Set-Service sshd -StartupType Automatic` |
| 页面打不开但 ssh 正常 | 新机器防火墙是开的，8080 没放行 | 管理员跑 `stream-setting.exe --inbound`（22 也要放行） |
| **服务在跑、日志也写了 `starting`，但外部完全连不上** | 入站被整档丢弃：Public 配置文件默认丢全部入站（含 ICMP），VMware NAT / host-only 网卡常被判成 Public。表现是 `ping` 无响应、8080 **TIMEOUT**（不是 REFUSED） | 先看 `logs\watchdog.log` 最后一条 `PREFLIGHT`：若是 `listening on 8080`，问题就在网络层 —— 管理员跑 `stream-setting.exe --inbound`；`Get-NetConnectionProfile` 确认不是被 `Private` 规则挡在 Public 上 |
| 日志里只有 `启动：<目录>`，没有 `PREFLIGHT` 行 | 跑的是旧包（新版的 `--run` 会自己 spawn `--probe`） | 用当前整包覆盖运行根；新版每次启动都会写 `PREFLIGHT` 判决 |
| 安装器的结论是 `[fail] nothing listening ... after 60 s` | 8080 被占用 / exe panic / 缺 VC++，三者之一 | `last-install.txt` 里已列出要看的两份日志；或跑 `stream-server.exe --diagnose`（九节事实清单） |
| 证书报警、剪贴板失灵 | SAN 还是旧 IP；或根本没配证书（退回明文 http） | 按 §1.1 重签；剪贴板 API 要求安全上下文 |
| 信令连上、视频不出 | 跨网段/跨 NAT，`ice_servers=[]` 只有 host candidate | 同网段部署，或后续接 STUN/TURN |
| 指针灵敏度/界面缩放改了没反应还报错 | 服务用**非游戏账号**跑，改的是另一个 HKCU | `/RU` 必须是游戏所在的交互用户（现网 `Administrator`） |
| 任务显示"运行过"，但页面 404 / 日志空 | 整包被搬到了别的目录，计划任务里还存着旧路径 | 在新目录重跑一次 `stream-setting.exe --install`；部署体检会把动作路径打出来 |

---

## 6 多台被控机怎么管

- 同一份整包（`target\output\cloud-stream.zip`）可以发到所有机器；`-SkipBuild`
  时也能直接复用上一轮的产物。
- 每台机器各自的：证书（IP 不同）、计划任务（控制台账户不同，但不需要密码）、`stream-config.toml`
  里的分辨率 / 码率参数（`--upload-dir` 已按账户推导，通常不用改）。
- 现在**没有**集中管理面：每台一套 `-Target`，靠 ssh 别名区分；`/api/v1/health`
  是单机自述，不做聚合。
