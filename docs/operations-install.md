# 安装与卸载：一个 exe 装出整台机器，同一个 exe 把它收回原样

这一层原本是六个脚本、四层命令（`install.cmd` → `setup-watchdog.bat` →
`setup-task.ps1` → `schtasks.exe`，`open-inbound.cmd` → `netsh.exe`）。四类
失败方式分别来自 cmd 的引号、`-Command` 的第二层引号、`schtasks /RP` 存密码、
`netsh` 重复建规则 —— 全都不是"逻辑写错了"，而是"命令行长到会出错"。

现在这三个动作都在 `cloud-setup.exe` 里，**没有命令行可拼**：

| 动作 | 命令 | 需要管理员 | 实现在 |
|---|---|---|---|
| 安装 | `cloud-setup.exe --install` | 第二段才要 | `install.rs` |
| 卸载 | `cloud-setup.exe --uninstall` | 整段要 | `uninstall.rs` |
| 只补防火墙 | `cloud-setup.exe --inbound` | 是 | `install.rs` |

## 产物：一个目录、一个文件、一个 zip

```text
target\output\
  cloud-stream\          ← 运行根：exe / dll / 前端 / 证书 / 配置 / README
  cloud-stream.zip       ← 上面那个目录的压缩快照（部署上传用）
  cloud-setup.exe        ← 单文件安装器：自带整包（约 9 MB）
```

`cloud-setup.exe` **不在** `cloud-stream\` 里面，这是刻意的：归档里放进
`cloud-setup.exe` 就是"exe 里嵌着包含自己的包"。装完之后 `pack::place_self`
把它复制进运行根，从那以后它就是控制台。所以：

* 装一台新机器 = 拷 `cloud-setup.exe` 过去双击，不拷目录；
* 已经装好的机器 = 运行根里那个 `cloud-setup.exe` 就是控制台。

体积上有个反直觉的点：运行根 24.0 MB（exe 自己 20.2 MB），装进 exe 之后只有
8.7 MB（9,144,832 B）。因为它不是"把文件原样拼在后面"，而是过了一遍 flate2
—— 20 MB 的 PE 里大部分是没压缩的段与前端资源。压缩的代价是安装时多几百毫秒，
收益是跨机器传的那个文件小一半。

构建顺序因此是**先整包、后安装器**：`deploy.ps1` 先装满
`target\output\cloud-stream`，再带 `--features embed` 和
`CS_PAYLOAD_DIR=那个目录` 编一次，产物拷到 `target\output\cloud-setup.exe`。
反过来的话，exe 里嵌的是一个还不存在的包。

## 同一个 exe 两个角色

判据**不是**"我是不是被双击了"（这个问题在进程里没有答案），而是我旁边有没有
`stream-server.exe`：

| 内置包 | 旁边有 `stream-server.exe` | 角色 |
|---|---|---|
| 有 | 没有 | 安装器（分发出去的那一份） |
| 有 | 有 | 控制台（装好之后的那一份，双击不会重装） |
| 无 | 任意 | 控制台（开发机构建，点「安装」会直说找不到包） |

`--install` / `--uninstall` / `--purge` / `--inbound` 压过判据：这些参数在
`main` 里、在角色判断**之前**就被拦下来了。`--purge` 尤其必须如此 —— 它跑在
`%TEMP%` 里，那儿没有 `stream-server.exe`，而它自带内置包，按判据会被认成
"安装器"，于是它去解压一个包，而不是去删目录。

## 安装：两段，因为管理员令牌只该用在需要它的地方

```text
第一段（用户令牌）   %LOCALAPPDATA%\cloud-stream：解压 → 复制自己 → 还回身份备份
第二段（管理员）     四条入站规则 → 注册任务 MCStreamSrv → 启动 → 等端口应答
```

**解压必须在用户令牌下做。** `runas` 有可能切换到另一个账户（本机唯一的
管理员不是当前用户时一定会），那时提权进程的 `%LOCALAPPDATA%` 是别人的目录 ——
装到"管理员的家"里，而用它的人找不到。所以第一段先在自己的令牌下把文件铺好，
第二段只做那两件非管理员做不了的事，并且用 `--dir` 把目标路径**显式**传过去
（提权进程的当前目录是 `System32`）。

第二段是同一个 exe 带 `--finish` 重新起来，两段之间没有需要解析的中间产物。

## 卸载：五步，次序不是装饰

```text
1. 计划任务 MCStreamSrv   先停、再删 —— 先杀进程会有一个"它又被拉起来"的窗口
2. stream-server.exe      只杀 exe 路径在这个目录下的（隔壁那个安装不动）
3. 默认录音设备           服务启动时把它抢走了，且从不自己还回来
4. 四条入站规则           端口问服务端自己，所以 9000 的机器也能清干净
5. 目录本身               交给替身进程（见下）
```

### 第 5 步为什么要一个"替身"

删不掉自己：`cloud-setup.exe` 正在跑，Windows 不会让任何人删掉一个正在执行的
exe（可以重命名，不能删除）。也没有"退出前把自己删了"这种操作。

所以：把自己复制到 `%TEMP%`，以 `--purge --dir <目标> --wait <pid>` 拉起，
然后本进程退出。替身等这些 pid 都消失才开始删 —— 那时整棵树里没有任何人持有
句柄。**对话框必须在交棒之前弹完**：反过来的话，本进程会停在 `MessageBox` 上
等用户点确定，而替身正等着它退出，用户只要读得慢一点，删除就会在一个"exe 还
被占着"的目录上开始。

控制台的「卸载」按钮同理：提权之后**立刻 `DestroyWindow`**，因为那个窗口本身
就占着目录里的 exe。

### 删不掉怎么办

**删，但失败就退回提示**（这是明确的选择，不是兜底）：目录保留、退出码 8、
备注写 `=== RESULT: UNINSTALL-FAIL (files) ===`、弹框把**具体文件路径**交出来。

所以删目录不用 `std::fs::remove_dir_all` —— 它失败时只说"拒绝访问"，不说
是哪个文件，而"删不掉"这个结论的全部用处就是告诉用户该关掉什么。自己走一遍
树：清只读位、跳过联接、按路径长度倒序删目录，失败时带着路径返回。

### 任务为什么读磁盘而不是问 `schtasks`

`%SystemRoot%\System32\Tasks\<名字>` 是任务计划程序自己的存法（实测 UTF-16LE
XML）。读它比 `schtasks /Query /XML` 好三处：

* 不要子进程 —— 也就不受"`schtasks.exe` 被策略拦着"影响（开发机上它就在安全
  策略的程序黑名单里）；
* 不受控制台代码页影响。`schtasks` 的输出走 OEM 代码页（中文系统上是 GBK），
  转成 UTF-8 时非 ASCII 路径会变成问号，于是"这条任务是我的吗"没法回答；
* 存在性就是文件在不在，比解析一行本机化的提示可靠。

`schtasks` 仍然是**动手**的那一个（`/Delete`、`/Change /DISABLE`）。

删不掉时先 `/DISABLE` 再判断，而不是像脚本版那样直接放弃：禁用之后看门狗再也
拉不起服务，"任务残留 + 目录照删"因此是安全的。禁不掉才停手（退出码 1），
而且停手之前一个文件都不动。

### 判成败看退出码，不看文案

安装那一步的 `/Create` 曾经用 `contains("success") || contains("成功")` 判断，
2026-11-11 在 `.150` 上因此报了一次**不存在的失败**：

```text
注册计划任务失败：
  schtasks 说: 鎴愬姛: 鎴愬姛鍒涘缓璁″垝浠诲姟 "MCStreamSrv"銆?
任务 XML 留在：C:\Users\Admin\AppData\Local\Temp\cloud-stream-task-4804.xml
文件已经在 C:\Users\Admin\AppData\Local\cloud-stream，但没有任务，开机不会自己起来。
```

而现场是：任务建好了、状态 Running、`listening on https://0.0.0.0:8080`。
那句话原文是 `成功: 成功创建计划任务 "MCStreamSrv"。`，退出码 0 —— 只是被
`from_utf8_lossy` 当成 UTF-8 解成了乱码，于是两个 `contains` 都为假。

两处一起改的：

* `sys::decode` 先严格试 UTF-8、失败再按 `GetOEMCP()` 走
  `MultiByteToWideChar`。我们自己的 exe（`--ops rollback`）输出 UTF-8，
  `schtasks` / `netsh` 输出 GBK，两头都要对；
* `register_task` 只看退出码（`sys::run` 把它带回来）。文案本来也不可靠：
  英文机说 `SUCCESS`、中文机说 `成功`、下个版本还可能换个说法，而退出码不翻译。

连带的一个后果值得记住：那次失败发生在 `[3/4]`，`return 6` 把 `[4/4]` 的
"启动并等应答"整段跳过了。服务之所以还是起来了，是任务的 `TimeTrigger`
（`PT1M`）在下一分钟把它拉起来的 —— 也就是说**看门狗救了安装器一次**。

## 端口不在这里猜

防火墙要放行哪个 TCP 端口、哪个 UDP 区间，问服务端自己：

```text
stream-server.exe --print-config json   →   port / webrtc_udp_port_min / webrtc_udp_port_max
```

它已经处理过配置的三层优先级（`--config` → `config.toml` → `stream-config.toml`
→ 默认值）。在这里再抄一份解析就是第二套判断。

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
<LogonType>InteractiveToken</LogonType>                       <!-- 不要密码，复用桌面会话的令牌 -->
<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>  <!-- 看门狗的全部机制 -->
<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>                 <!-- 不限时；默认 72 小时会杀掉跑着的服务 -->
```

* `InteractiveToken` 替代了 `schtasks /RU /RP`：不需要密码、不读也不写 SAM、
  空密码账户也能用。采集和 `SendInput` 是按会话隔离的，跑到 session 0 的进程
  看到的是一张空桌面。
* `IgnoreNew`：活着的实例挡住新实例，死掉之后下一次触发把它拉回来。这就是
  看门狗，不需要额外的监控进程。
* `PT0S`：`schtasks` 对重复任务的默认上限是 72 小时，会悄悄杀掉一个已经跑了
  三天的服务。

`--task-xml` 把这份 XML 原样打到 stdout（不写任何东西）：字段里排查"任务建不
起来"就靠它 —— `schtasks` 的报错只到"XML 无效"这一层。验证它用的是任务计划
程序自己的解析器（`Register-ScheduledTask -Xml`，读完属性立刻注销）。

## 结论文件就是契约

`%ProgramData%\cloud-stream\last-install.txt`，安装与卸载写同一个文件、同一套
`=== RESULT:` 标记：

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

卸载多一段 —— 替身删完目录之后自己追加尾部：

```text
=== RESULT: PENDING (目录由替身删除) ===
--- 替身：删目录（2026-11-11 10:32:19）---
  [ok] 目录已删除：C:\Users\x\AppData\Local\cloud-stream
=== RESULT: UNINSTALLED ===
```

控制台窗口的 `watch_install_result` 只认 `=== RESULT:` 这个标记，不关心结论是
谁写的。写不进去时退回 `<目标目录>\logs\setup.log` —— "装完了但一个字都没留下"
是最难排查的一种结果。

## 诊断入口

三个都走 stdout、都不改动任何东西（和 `--print-config` 同一类）：

| 参数 | 回答什么 |
|---|---|
| `--task-xml` | 要注册的那个任务长什么样 |
| `--uninstall --plan` | 在这台机器上卸载会做什么（含"拒绝卸载"的理由） |
| `--uninstall --whatif` | 同上，弹出窗口版（双击场景） |

## 现在能验证到什么程度

| 环节 | 怎么验的 | 结果 |
|---|---|---|
| 归档往返（解压 / 覆盖同名 / 保留用户文件 / 清临时目录） | `cargo test --features embed` 的 `embedded_payload_round_trips` | 通过 |
| 角色矩阵（四种组合） | `role_matrix` 单测 | 通过 |
| 任务 XML 被真实解析器接受 | 把 `--task-xml` 的输出交给 `Register-ScheduledTask -Xml`，读完属性立刻注销 | 通过：`Interactive / Limited / IgnoreNew / PT0S`，两个触发器，回读含 `--run` |
| 禁用状态能从磁盘认出来 | `Register-ScheduledTask` → `Export-ScheduledTask` 对照：启用的导出**没有** `<Enabled>`，禁用的有 `<Enabled>false</Enabled>` | 通过（判据据此写成"设置块里出现 false"） |
| 替身删目录 | 探针树（5 文件含嵌套）→ `--purge` | 通过：退出码 0、目录消失、结论写 `UNINSTALLED`、`MoveFileEx DELAY_UNTIL_REBOOT` 真的进了 `PendingFileRenameOperations` |
| 删不掉时点名文件 | 独占打开一个文件（没有 `FILE_SHARE_DELETE`）再 `--purge` | 通过：退出码 8、目录保留、结论写着 `删不掉 ...\logs\held.log：另一个程序正在使用此文件 (os error 32)` |
| `--plan` | 对真实整包目录跑 | 通过（任务/进程/规则/目录四项都能报出来） |
| 提权两段串联、防火墙真建真删、录音设备真还原、真机点安装/卸载 | **未验证** —— 开发机上 `schtasks.exe` 被安全策略拦着，而且不该在别人的机器上改防火墙 | 待新机器验收 |

## 相关文档

* [`operations-run.md`](operations-run.md) —— 服务怎么被拉起来（任务动作 `--run`）
* [`console.md`](console.md) —— 控制台那几个按钮各落到哪条命令
* [`deploy-one-click.md`](deploy-one-click.md) —— 整包的部署与踩坑
