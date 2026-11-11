# 安装：一个 exe 装出整台机器

这一层现在有两套实现，**并存**，共用同一个结论文件：

| | 旧（脚本） | 新（原生） |
|---|---|---|
| 入口 | `toolchain/install.cmd`（525 行） | `cloud-setup.exe --install`（`crates/setup/src/install.rs`） |
| 用到的外部程序 | `setup-watchdog.bat` → `setup-task.ps1` → `schtasks.exe`，`open-inbound.cmd` → `netsh.exe` | `schtasks.exe`，`netsh.exe` |
| 命令行拼接的层数 | 3（cmd → powershell → 程序） | 0（`raw_arg` 与任务 XML 文件） |
| 分发物 | 整个目录（2000+ 个文件 + 8 个脚本） | **一个 exe**（`--features embed` 构建，约 13 MB） |
| 结论 | `%ProgramData%\cloud-stream\last-install.txt` | 同一个文件、同一套 `=== RESULT:` 标记 |

发布路径目前仍然是脚本那一套：`deploy.ps1` 还在打整包，控制台的「安装」按钮还在调
`install.cmd`。原生版要等 `deploy.ps1` 改成"先打包装再编安装器"之后才成为唯一入口 ——
在那之前它是可以单独跑的、已可验证的第二条路。

## 两段，因为管理员令牌只该用在需要它的地方

```text
第一段（用户令牌）   %LOCALAPPDATA%\cloud-stream：解压 → 复制自己 → 还回身份备份
第二段（管理员）     防火墙四条入站规则 → 注册任务 MCStreamSrv → 启动 → 等端口应答
```

**解压必须在用户令牌下做。** `runas` 有可能切换到另一个账户（本机唯一的
管理员不是当前用户时一定会），那时提权进程的 `%LOCALAPPDATA%` 是别人的目录 ——
装到"管理员的家"里，而用它的人找不到。所以第一段先在自己的令牌下把文件铺好，
第二段只做那两件非管理员做不了的事，并且用 `--dir` 把目标路径**显式**传过去
（提权进程的当前目录不可靠）。

第二段是同一个 exe 带 `--finish` 重新起来，所以两段之间没有任何需要解析的
中间产物：路径、账户、任务名都是命令行参数，结论写同一个文件。

## 一条命令长什么样

```powershell
# 双击的行为（不带参数）：有内置包且旁边没有 stream-server.exe → 安装器
.\cloud-setup.exe

# 装到别处 / 不问确认（给控制台按钮用）
.\cloud-setup.exe --install --dir 'D:\cloud-stream' --yes

# 只做管理员那一段（内部用；一般由第一段自己拉起）
.\cloud-setup.exe --install --finish --dir 'D:\cloud-stream' --account 'PC\user'

# 看看要注册的任务长什么样（诊断；不写任何东西）
.\cloud-setup.exe --task-xml
```

同一个 exe 两个角色，判断依据**不是**"我是不是被双击了"（这个问题在进程里
没有答案），而是我旁边有没有 `stream-server.exe`：

| 内置包 | 旁边有 `stream-server.exe` | 角色 |
|---|---|---|
| 有 | 没有 | 安装器（分发出去的那一份） |
| 有 | 有 | 控制台（装好之后的那一份，双击不会重装） |
| 无 | 任意 | 控制台（开发机构建，窗口会直说找不到服务） |

`--install` 压过一切：修复安装、旁边已经有服务时也要重装。

## 端口不在这里猜

防火墙要放行哪个 TCP 端口、哪个 UDP 区间，问服务端自己：

```text
stream-server.exe --print-config json   →   port / webrtc_udp_port_min / webrtc_udp_port_max
```

它已经处理过配置的三层优先级（`--config` → `config.toml` → `stream-config.toml`
→ 默认值）。在这里再抄一份解析就是第二套判断 —— `open-inbound.cmd` 里
"40000-40049 必须和配置默认值保持同步"那句注释，正是第二套判断迟早分叉的自白。

四条规则各自挡一类"服务在跑、外面连不上"：

| 规则 | 挡的是 |
|---|---|
| `cloud-stream <port>` TCP | 页面、WS 信令、文件传输 |
| `cloud-stream ICMPv4` | `ping` 没应答 —— 一台完全健康的机器看起来像死的 |
| `cloud-stream webrtc UDP` `<min>-<max>` | 媒体走 UDP、信令走 TCP，只开一条的症状是"连上了但黑屏" |
| `cloud-stream app` | 按路径匹配所有端口，兜住改了端口的部署 |

先删后建（`netsh` 允许同名规则多条），删的时候顺手清掉"Windows 已阻止此应用的
部分功能"留下的 BLOCK 规则 —— BLOCK 优先于 allow，一条被驳回的阻止规则看起来
像"规则都在，但一个包都不过"。

## 任务 XML 里三个改了就毁的设置

```xml
<LogonType>InteractiveToken</LogonType>              <!-- 不要密码，复用桌面会话的令牌 -->
<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>  <!-- 看门狗的全部机制 -->
<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>        <!-- 不限时；默认 72 小时会杀掉跑着的服务 -->
```

* `InteractiveToken` 替代了 `schtasks /RU /RP`：不需要密码、不读也不写 SAM、
  空密码账户也能用。采集和 `SendInput` 是按会话隔离的，跑到 session 0 的进程
  看到的是一张空桌面。
* `IgnoreNew`：活着的实例挡住新实例，死掉之后下一次触发把它拉回来。这就是
  看门狗，不需要额外的监控进程。
* `PT0S`：`schtasks` 对重复任务的默认上限是 72 小时，会悄悄杀掉一个已经跑了
  三天的服务。

## 结论文件就是契约

`%ProgramData%\cloud-stream\last-install.txt`，格式与 `install.cmd` 一致：

```text
cloud-stream install
mode    : install
target  : C:\Users\x\AppData\Local\cloud-stream
started : 2026-11-12 10:31:02
  [1/4] unpack 400/2004
  ...
=== RESULT: OK ===
finished: 2026-11-12 10:31:19
```

控制台窗口的 `watch_install_result` 只认 `=== RESULT:` 这个标记，不关心结论是
cmd 写的还是这个 exe 写的。写不进去时退回 `<目标目录>\logs\setup.log` ——
"装完了但一个字都没留下"是最难排查的一种结果。

## 现在能验证到什么程度

| 环节 | 怎么验的 | 结果 |
|---|---|---|
| 归档往返（解压 / 覆盖同名 / 保留用户文件 / 清临时目录） | `cargo test --features embed` 的 `embedded_payload_round_trips` | 通过 |
| 角色矩阵（四种组合） | `role_matrix` 单测 | 通过 |
| 任务 XML 被真实解析器接受 | 把 `--task-xml` 的输出交给 `Register-ScheduledTask -Xml`，读完属性立刻注销 | 通过：`Interactive / Limited / IgnoreNew / PT0S`，两个触发器，回读含 `--run` |
| `netsh` 接受含空格的规则名 | `show rule name="cloud-stream app"` 只读核对 | 通过（返回"没有匹配的规则"，不是语法错） |
| 提权两段、防火墙真建、90 秒探活、点安装/卸载的窗口流程 | **未验证** —— 开发机上 `schtasks.exe` 被沙箱拦着，且不该在别人的机器上改防火墙 | 待新机器验收 |

## 相关文档

* [`operations-run.md`](operations-run.md) —— 服务怎么被拉起来（任务动作 `--run`）
* [`console.md`](console.md) —— 控制台那七个按钮各落到哪条命令
* [`deploy-one-click.md`](deploy-one-click.md) —— 整包的部署与踩坑
