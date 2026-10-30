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

---

## 1 被控机：一次性准备（10 项）

做完这 10 项，这台机器就能被 `pwsh toolchain/deploy.ps1 -Target <别名> -Hostname <IP> -Bootstrap` 接管。

| # | 项 | 怎么确认 / 怎么做 | 漏了会怎样 |
|---|---|---|---|
| 1 | Windows 10/11 **x64** | `systeminfo` | 采不了：DXGI/WASAPI/`SendInput` 都是 Windows API，且只有 Windows 构建 |
| 2 | 硬件基线 | 4 vCPU / 8 GB（现网基线，VP9 1080p60 约 2× CPU） | 掉帧、QoS 一路降档 |
| 3 | **有真实显示输出** | VMware SVGA 3D 或物理显示器；分辨率设成要推的尺寸 | 采集黑屏或退化成 1024×768 |
| 4 | **交互式已登录会话** | `quser` 看到 `administrator console 1 运行中` | 计划任务的 `/IT` 任务不启动；从服务/session 0 启动的进程抓到空桌面 |
| 5 | **开机自动登录**（可选但强烈建议） | `AutoAdminLogon=1` 或手工登录后再离开 | 重启后没人登录 → 1 分钟看门狗每次空跑，`/api/status` 不通 |
| 6 | VC++ 运行库 | `Test-Path C:\Windows\System32\vcruntime140.dll` | exe 起不来（缺 `VCRUNTIME140.dll`） |
| 7 | OpenSSH Server | 装 → `Set-Service sshd -StartupType Automatic` → `Start-Service sshd` → 放行 TCP 22 | 部署脚本第一步就超时（脚本会把这三行打出来） |
| 8 | 目录 | `C:\dev\stream\logs`、`C:\dev\deploy`、`C:\dev\selkies-web` | `-Bootstrap` 会建，但 exe/前端仍要落位 |
| 9 | TLS 证书 | `C:\dev\cert.pem` + `C:\dev\key.pem`，**SAN 里要有这台机器的新 IP** | 浏览器证书不匹配；剪贴板 API 直接不可用（非安全上下文） |
| 10 | 计划任务 `MCStreamSrv` | 在被控机跑一次 `C:\dev\deploy\setup-watchdog.bat`（**先把里面的 `/RP` 密码改成这台机器的**） | `task MISSING`，切换阶段失败 |

额外两条只跟游戏有关，不属于推流链：

- 游戏本体 / Java / 服务端各自启动，推流重启不该碰它们（`deploy-notes-mc-coexist.md`）。
- 上传根 `--upload-dir` 写死在 `start-stream.cmd` 里（现网是 `C:\Users\Administrator\Desktop`），换机器要改这一行再跑 `-Bootstrap`。

### 1.1 证书

`toolchain/cert-san.cnf` 里的 `CN` 与 `subjectAltName` 是**写死的 IP**（现在写着
`.148` 和 `.143`）。换机器有两步：

1. 改 `CN=` 和 `subjectAltName=IP:<新IP>`；
2. 重新签发，产物放 `C:\dev\cert.pem` / `C:\dev\key.pem`。

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

```powershell
# 第一次搬上一台新被控机
pwsh toolchain/deploy.ps1 -Target mc-admin-2 -HostName 192.168.59.15x -Bootstrap

# 之后日常（后端 + 前端）
pwsh toolchain/deploy.ps1 -Target mc-admin-2 -HostName 192.168.59.15x
```

远端路径是**常量**（`C:\dev\stream`、`C:\dev\selkies-web`、`C:\dev\deploy`），
写成别的目录要同时改 `deploy.ps1` 的 `$Remote*`、`start-stream.cmd`、
`setup-watchdog.bat` —— 三处，别只改一处。

`-Bootstrap` 只做四件事：sshd 自启、建目录、推 `start-stream.cmd` /
`run-hidden.vbs`、核对 exe / 任务 / 证书。**它不建计划任务、不签证书、不动密码**
——那三件是人做的（见 §1 的第 9、10 项和 §1.3）。

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

## 5 换设备最容易踩的 6 条

| 现象 | 原因 | 处置 |
|---|---|---|
| 页面能开、画面全黑 | 进程跑在 session 0（服务/schtasks 没加 `/IT`），抓的是空桌面 | 部署体检打印 `session=0` 就是这条；用 `setup-watchdog.bat` 建任务 |
| 重启之后 8080 不通 | 没人登录 → `/IT` 任务不启动；或 `sshd` 是「手动」 | 开自动登录；`Set-Service sshd -StartupType Automatic` |
| 页面打不开但 ssh 正常 | 新机器防火墙是开的，8080 没放行 | 放行 TCP 8080（22 也要） |
| 证书报警、剪贴板失灵 | SAN 还是旧 IP；或根本没配证书（退回明文 http） | 按 §1.1 重签；剪贴板 API 要求安全上下文 |
| 信令连上、视频不出 | 跨网段/跨 NAT，`ice_servers=[]` 只有 host candidate | 同网段部署，或后续接 STUN/TURN |
| 指针灵敏度/界面缩放改了没反应还报错 | 服务用**非游戏账号**跑，改的是另一个 HKCU | `/RU` 必须是游戏所在的交互用户（现网 `Administrator`） |

---

## 6 多台被控机怎么管

- 同一份 `stream-server.exe` 和前端 zip 可以发到所有机器（`-SkipBuild` 复用产物）。
- 每台机器各自的：证书（IP 不同）、计划任务（密码不同）、`start-stream.cmd` 里的
  `--upload-dir` 与分辨率参数。
- 现在**没有**集中管理面：每台一套 `-Target`，靠 ssh 别名区分；`/api/v1/health`
  是单机自述，不做聚合。
