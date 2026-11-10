# 给一台"从没开过 SSH"的机器开 SSH

`deploy.ps1` 的每一次远端操作都走 ssh，所以它解决不了"这台机器还没有 ssh"
—— 那是鸡生蛋。这份文档讲的是那个蛋：一个一次性引导脚本，跑一次，之后就
可以像 `.148` 一样用别名连接。

## 1 什么时候需要它

| 现象 | 说明 |
|---|---|
| `ssh <host>` → `Connection timed out` | 22 没有监听，或者被防火墙丢弃 |
| `Test-NetConnection <host> -Port 22` → `TcpTestSucceeded : False`，但 8080 通 | 典型的"服务在跑、只有网页端口开着" |
| `deploy.ps1` 预检报"ssh 不可达" | 同一条链的上游 |

`.150` 目前就是这个状态：8080 通、22 超时。这不是故障，Windows **默认不装**
OpenSSH Server，也**默认不放行** 22。

## 2 跑一次

把 `toolchain\enable-ssh.ps1` 和 `toolchain\enable-ssh.cmd` 拷到目标机
（同一个目录），双击 `enable-ssh.cmd`：

```bat
enable-ssh.cmd                                  :: 默认只允许 192.168.59.146
enable-ssh.cmd -Check                           :: 只读，看现状，不需要管理员
enable-ssh.cmd -ClientAddress 192.168.59.10     :: 换一台构建机
enable-ssh.cmd -KeepPassword                    :: 保留密码登录（公钥可能写错时用）
```

`enable-ssh.cmd` 只是一层包装：`.ps1` 双击会被记事本接管，而且新机器的
执行策略是 `Restricted`，需要 `-ExecutionPolicy Bypass`。真正的逻辑全在 `.ps1` 里。

脚本自己提权（非管理员时弹一次 UAC，然后**另开一个窗口干活并留在那里**，
因为报告就在那个窗口里）。输出是纯 ASCII 英文 —— 控制台代码页是 GBK，
中英混排在出问题时恰好最难读。

## 3 它按什么顺序做

顺序不是随便排的。**"公钥写错 + 关掉密码登录"等于一台谁也进不去的机器**，
所以先证明钥匙能用，再上锁。

| 步 | 动作 | 失败时 |
|---|---|---|
| 1 | 装 `OpenSSH.Server` 能力（已装则跳过），失败退到 `DISM` | 报清楚是"这台系统没有该能力"还是"没有本地负载也没有网" |
| 2 | `sshd` 设为自动启动并启动；缺主机密钥就 `ssh-keygen -A` | 直接停，不往下走 |
| 3 | 公钥写进 `administrators_authorized_keys`（控制台用户不在管理员组时**另外**写他的 `~\.ssh\authorized_keys`），ACL 收紧到 `SYSTEM` + `Administrators` | 公钥格式不对就停 —— 检查一次比登不进去再查便宜 |
| 4 | 防火墙：允许 TCP 22 **只从** `<ClientAddress>` 与回环；把系统自带的 `OpenSSH SSH Server (sshd)` 等"任意来源"规则**禁用** | 有"任意来源"规则在，限定规则等于没写，所以必须禁 |
| 5 | 改 `sshd_config`（先备份）→ 重启 → **从 127.0.0.1 用公钥登录自测** | 自测失败：打 `sshd.log` 尾部，**不动密码登录**，退出码 1 |
| 6 | 只有自测通过才 `PasswordAuthentication no` → 重启 → **再测一次** | 第二次失败：还原备份、重启、退出码 1 |

第 5、6 步用的是 `ssh -o BatchMode=yes`，所以任何提示都会立即失败而不是
挂在那里等输入 —— 一个没人看的窗口挂住比报错更糟。

## 4 安全边界

* **不是"开 22"**：规则绑定源地址，默认只有 `192.168.59.146` 和回环能连。
* **系统自带规则被禁用**：`OpenSSH SSH Server (sshd)` 默认 `Allow Any`。
  留着它，上面那条限定规则就只是一句装饰。脚本会在输出里写明"已禁用"，
  免得后来人看到它被关掉以为是误操作。
* **不碰账户**：不设密码、不改密码、不动 SAM。密钥是密钥，账户是账户。
* **密码登录默认关闭**，且只在公钥被证明可用之后才关。要保留就 `-KeepPassword`。
* **可回滚**：`sshd_config` 每次改之前备份成 `sshd_config.bak-<时间戳>`，
  路径在报告最后一行。卸载能力会连同配置一起带走。

## 5 两个必须先知道的坑

### 5.1 空密码账户连不上，密钥也一样

Windows 的 `LimitBlankPasswordUse=1`（默认）禁止**空密码账户**做网络登录。
sshd 属于网络登录，所以哪怕公钥完全正确也会被拒。脚本会检测这个策略并提示，
但**不会**替用户设密码 —— 那是账户的事。

之前 `setup-watchdog.bat` 那轮已经证明这台机器上确实存在空密码账户
（`schtasks /RP` 撞 `0x800704DD`）。如果 `.150` 是这种账户，先跑：

```bat
net user <account> *
```

设一个密码，再从构建机试连接。**这一步只影响 SSH 登录，不影响桌面自动登录**
（自动登录靠 `AutoAdminLogon` 的注册表值，与账户密码是否为空无关，但设了密码
之后要同步更新那里 —— 否则下次开机会停在登录界面）。

### 5.2 sshd 跑在 session 0，看不到你的桌面

这是最容易浪费时间的一条。sshd 作为服务运行在**服务会话**里，而我们要测的是
**控制台交互会话**：屏幕内容、光标位置、`SendInput` 注入。

所以：

* 在 ssh 里直接跑 `GetCursorPos`，拿到的是 session 0 那个空桌面的坐标，永远 (0,0)；
* 想抓真实数据，必须**从 ssh 发起、在交互会话里执行** —— 也就是项目里已经在用的
  那招：`setup-task.ps1` 注册的 `Interactive` 计划任务。探针脚本由任务在 session 1
  里跑，把 JSON 写到 `C:\dev\stream\logs\`，再从 ssh 取走。

不要为了"能远程跑命令"去开 3389：RDP 的暴露面比 ssh 大得多，而且同样要现场
开一次防火墙。

## 6 验证清单

脚本最后会打印一段结果，可以直接贴回来。它包含：

```
service: Running / Auto
listening: 0.0.0.0:22
pubkeyauthentication yes
passwordauthentication no          (或 yes，视 -KeepPassword)
rule: cloud-stream ssh 22 enabled=True from=192.168.59.146,127.0.0.1,::1
administrators_authorized_keys ACL: ...
config backup: ...\sshd_config.bak-<时间戳>
```

从构建机上确认：

```bash
ssh -o BatchMode=yes <account>@192.168.59.150 'hostname && whoami'
```

`BatchMode=yes` 是关键：它不允许任何交互，能连上就是真的免密可用。

## 7 回滚

```powershell
Stop-Service sshd
Set-Service sshd -StartupType Manual
Get-NetFirewallRule -DisplayName 'cloud-stream ssh 22' | Remove-NetFirewallRule
Get-WindowsCapability -Online -Name 'OpenSSH.Server*' | Where-Object State -eq Installed |
    Remove-WindowsCapability -Online
Remove-Item "$env:ProgramData\ssh\administrators_authorized_keys" -Force
```

系统自带的 `OpenSSH SSH Server (sshd)` 规则如果被禁用了，要手工
`Enable-NetFirewallRule` 回来。**这两条规则都不是我们建的，所以脚本不替它们
决定命运** —— 只在自己被卸载时说明"我禁用过它们"。

## 8 第一次实机跑出来的六个坑

脚本写出来当天就在一台真机上跑通了：装能力包 → 自测 → 关密码 → 用真私钥登录成功
→ 完整还原。下面六条都**只有实跑才会暴露**，语法解析和静态检查一条都发现不了。

### 8.1 `-N ''` 和 `-N '""'` 都会生成带密码的临时密钥

自测要用一次性密钥（构建机的私钥在构建机上，目标机只有公钥）。第一版这样写：

```
ssh-keygen -t ed25519 -N '""' -q -f <临时路径>
```

结果是**带 passphrase 的密钥**（cipher 是 `aes256-ctr`）。它造成的现象极具误导性：

* 服务端：`Accepted key ... found at administrators_authorized_keys:2`、`Postponed publickey`，
  紧接着 `ssh_dispatch_run_fatal: ... Unknown error`；
* 客户端：`Server accepts key` 之后直接 `No more authentication methods to try`；
* 两边**都没有**一个字提到 passphrase。

原因是 `BatchMode=yes` 下客户端不能问密码，于是放弃签名，连接被重置（服务端看到的是
`WSARecv ... 10054`）。整条链路读起来像"服务端拒绝了这个 key"，实际是**客户端没能用
这把 key 签名**。

更麻烦的是 PowerShell 两个版本行为不同：

| 写法 | pwsh 7 | Windows PowerShell 5.1（脚本实际运行的） |
|---|---|---|
| `-N ''` | 空密码，可用 | **非空密码，不可用** |
| `-N '""'` | 两个引号字符当密码 | 同左 |

正解是把 `-N ""` 作为**一整条命令行字符串**交给 `Start-Process -ArgumentList`，让引号
直接进到 `ssh-keygen` 的 argv 里，绕开 PowerShell 的参数绑定：

```
$quote = [char]34
$argLine = '-t ed25519 -N {0}{0} -q -C cs-sshprobe -f {0}{1}{0}' -f $quote, $tempKey
Start-Process -FilePath $keygen -ArgumentList $argLine -Wait -PassThru -NoNewWindow
```

脚本还多了一道断言：读私钥 base64 里的 ciphername，必须是 `none`，否则跳过自测 ——
不拿一把用不了的钥匙去"证明"能登录。

### 8.2 先拆继承再 grant，会做出一个谁也删不掉的文件

第一版收紧 ACL 用的是两步：`icacls file /inheritance:r`，然后 `icacls file /grant:r ...`。

真机上第二步失败了一次，文件被留成**空 DACL**：没有 DELETE、没有 WRITE_DAC，
**连它的 owner 都改不了**。实测全部无效：

| 手段 | 结果 |
|---|---|
| `Remove-Item` | Access denied |
| `Rename-Item` | Access denied |
| `[IO.File]::Delete` | Access denied |
| `takeown /F` | 拒绝访问 |
| `icacls /reset` | 拒绝访问 |
| `Set-Acl`（owner 是 Administrators） | unauthorized operation |

唯一能救回来的是 **SYSTEM 上下文**（它有 SeTakeOwnership / SeRestore，绕得开 DACL）：

```
$act  = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\icacls.exe" -Argument "`"$path`" /reset"
$prin = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
Register-ScheduledTask -TaskName FixSshAcl -Action $act -Principal $prin -Force
Start-ScheduledTask -TaskName FixSshAcl; Start-Sleep 3
Unregister-ScheduledTask -TaskName FixSshAcl -Confirm:$false
```

所以现在用的是**一次 `Set-Acl` 原子替换整个 DACL**：要么文件正好有那两条 ACE，
要么保持原样。中间不存在"已经拆掉、还没补上"的窗口。脚本还会在动手之前先 `Get-Acl`
看 `Access.Count`，发现是空的就直接把上面这条修复命令打出来，不再继续写。

### 8.3 `$check` 和 `[switch]$Check` 是同一个变量

脚本有 `[switch]$Check` 参数，第 6 步里写了 `$check = & $sshdExe -T -f $cfg`。

PowerShell 变量名**不区分大小写**，于是把 `sshd -T` 输出的 88 行数组赋给了一个 switch：

```
Cannot convert value "System.Object[]" to type "...SwitchParameter"
```

错误行号指向那一行本身，但那一行看起来只是"执行 sshd 接输出"，和 switch 毫无关系。
同一个写法在第 5 步用了 `$effective`，从来没出过事 —— 因为名字不一样。

**教训**：脚本级参数名是全局占用的，`$Check` / `$Force` / `$Verbose` / `$Debug`
不要拿来当局部变量。现在叫 `$cfgCheck`。

### 8.4 `New-NetFirewallRule -RemoteAddress` 不接受 `::1`

给它 `-RemoteAddress <ip>,127.0.0.1,::1` 会报"指定的多播、广播或环回 IPv6 地址"。
Windows 防火墙不允许把规则限定到 IPv6 回环。

而且这个异常直接中止了整个第 4 步（`$ErrorActionPreference = 'Stop'`），后面的配置
和自测根本不执行 —— 屏幕上只留一段 PowerShell 堆栈，看起来像脚本崩了。

现在只放 `127.0.0.1`（自测走 IPv4，`::1` 没用处），并且把建规则包在 try/catch 里，
失败时打印"允许的来源是什么、该改哪个参数"。

### 8.5 另外两条容易踩的

* **`Get-NetFirewallRule -DisplayName X -Direction Inbound` 不能用**：两个参数属于不同
  参数集，报 `Parameter set cannot be resolved`。先按名字取，再 `Where-Object` 过滤方向。
* **`sshd -T` 看不到 `Match` 块里的设置**：它打印全局 `AuthorizedKeysFile`
  （`.ssh/authorized_keys`），看起来像"管理员 key 文件根本没被用"。要问某个账户的
  实际值，得用 `sshd -T -C user=<name>,host=localhost,addr=127.0.0.1`。脚本的报告里两者都打。

### 8.6 `2>&1` 在 `$ErrorActionPreference = 'Stop'` 下会掐死脚本

这是最阴的一条：**它发生在 .150 上前四步全部成功之后**，把人留在"看起来跑完了、
其实最关键的一步没跑"的状态里。

第 5 步的自测这样写：

```
$out = & $sshCli -o BatchMode=yes -o StrictHostKeyChecking=no `
    -o UserKnownHostsFile=NUL -o ConnectTimeout=8 `
    -o PreferredAuthentications=publickey -i $tempKey `
    "$env:USERNAME@127.0.0.1" 'echo CS_SSH_SELFTEST_OK' 2>&1
```

`2>&1` 看着像"把 stderr 收进 `$out`"，在 Windows PowerShell 5.1 里它的实际含义是：
**stderr 的每一行都变成一个 `NativeCommandError` 错误记录**。而脚本第 71 行是
`$ErrorActionPreference = 'Stop'`，所以第一行就终止整个脚本。

触发它的是这一句 —— 注意它是 ssh **连成功之后**才打的：

```
Warning: Permanently added '127.0.0.1' (ED25519) to the list of known hosts.
```

`UserKnownHostsFile=NUL` 让它每次都"新加"一次主机密钥，于是每次都打。后果：

| 现场 | 实际状态 |
|---|---|
| 屏幕上 | 停在第 5 步的 `[i] probing key auth...` 后面，一段 `NativeCommandError` 堆栈 |
| 第 6 步 | **完全没执行** → `PasswordAuthentication` 仍是 `yes` |
| 临时密钥 | 已追加进 `administrators_authorized_keys`，且**私钥还在 `%TEMP%\cs-sshprobe-*/id_probe`** |
| 退出码 | 非零，但报告没打完，看不出差了什么 |

在本机用同一台 .150 做对照，两条命令的差别是**确定性**的（5.1.26100）：

```
A. & ssh ... -o UserKnownHostsFile=NUL ... 2>&1
   THREW RemoteException: Warning: Permanently added '192.168.59.150' (ED25519)...
B. Invoke-Native ssh ...             -> exit=0, 输出命中, stderr 被捕获, 不抛
C. Invoke-Native ssh ... + LogLevel=ERROR -> exit=0, stderr 0 行
```

修法是两条一起上，缺一不可：

1. **不再走管道**：所有外部程序都过 `Invoke-Native`，用
   `Start-Process -RedirectStandardOutput/-RedirectStandardError` 落到临时文件再读。
   没有管道就没有 `ErrorRecord`，5.1 和 7 行为一致；
2. **把警告源头上消掉**：加 `-o LogLevel=ERROR`。`Permanently added` 是 INFO 级，
   在 ERROR 下不出现 —— 报告也因此干净。

配套的三个防漏措施：

* 自测前先 `Remove-ProbeKeyLines` / `Remove-StaleProbeDirs`，**上一次半途而废的
  残留会被自动清掉** —— 所以"再跑一遍"永远是对的操作，不需要手工收拾；
* 兜底用脚本级 `trap`：真抛了也先 `Clear-ProbeArtifacts` 再报错退出，绝不留私钥在盘上；
* 自测失败时立刻把探针行摘掉，但**密钥文件保留到第 6 步复测完**（复测要用它），
  最后由 `Clear-ProbeArtifacts` 统一清。

> 附带一条操作纪律：**不要在 ssh 会话里跑这个脚本**。第 5、6 步都会
> `Restart-Service sshd`，而你自己那条连接正是 sshd 的子进程 —— 一重启就把它连同
> 脚本一起掐断，可能停在"密码已关、还没复测"的中间态。它必须从目标机的本地控制台跑。

### 8.7 追加到文件末尾 = 写进 `Match` 块，`sshd -T` 里完全看不出来

第 6 步补上 `KbdInteractiveAuthentication no` 时，`Set-SshdOption` 的原实现是
"找不到这一行就 `$lines += "名字 值"`"。在 Windows 上这是错的：

```
$ tail -2 C:\ProgramData\ssh\sshd_config
Match Group administrators
       AuthorizedKeysFile __PROGRAMDATA__/ssh/administrators_authorized_keys
```

**文件末尾在 `Match` 块里面。** 写在那里的指令是**条件生效**的，而 `sshd -T`（不带
`-C`）报的是全局值。于是出现一个最难查的组合：文件里明明有
`KbdInteractiveAuthentication no`，`sshd -T` 却照样答 `yes`。

在 .150 上实测过这一步（只读预演：复制一份改完，用真 sshd 解析，不碰在跑的配置）：

| 版本 | 追加位置 | `sshd -T` 的 `kbdinteractiveauthentication` |
|---|---|---|
| 追加到 EOF | 第 89 行，`Match` 块内 | **yes**（改了等于没改） |
| 插入到首个 `Match` 之前 | 第 87 行 | **no** |

现在的 `Set-SshdOption`：全局段里找到就原地替换；找不到就**插在第一条 `Match`
之前**；其余重复项（包括被塞进 `Match` 块的）一律注释掉 —— 因为 sshd 对多数指令
是"先到先得"，一条更早的全局值会压过后面的。

第 6 步也因此加了一道断言：改完读回 `sshd -T`，`passwordauthentication` 和
`kbdinteractiveauthentication` 必须**都是 no**，否则恢复备份并报
`not key-only; config restored`。不拿"我写了"当"它生效了"。

> 为什么 `KbdInteractiveAuthentication` 必须一起关：Windows 的 keyboard-interactive
> 底层走的还是账户密码。只关 `PasswordAuthentication` 时，客户端仍然会看到
> `(publickey,keyboard-interactive)`，输密码的那条路没断。.150 第一次跑完就是
> 这个状态 —— 报告写着 "key-only"，`sshd -T` 却是
> `kbdinteractiveauthentication yes`。

## 9 在 .150 上怎么做

1. 把 `toolchain\enable-ssh.ps1` 和 `toolchain\enable-ssh.cmd` 拷到那台机器同一个目录；
2. 右键 `enable-ssh.cmd` → 以管理员身份运行（或直接双击，它会自己弹 UAC）；
3. 等它把整段报告打完。第一段"installing"会停一会儿 —— 那是从 Windows Update 下载
   能力包，实测在一台机器上花了 **25 分钟**，这是正常的，不是卡死；
4. 报告最后是 `OK` 就把整段贴回来；是 `FAILED` 就贴 `[fail]` 附近的几行。

然后从构建机：

```bash
ssh -o BatchMode=yes mc-150 'hostname && whoami'
```

`~/.ssh/config` 里已经有 `mc-150`，`User` 先按那边 `C:\Users\Admin` 填的 `Admin`；
如果报告里的账户名不是它，改那一行即可。

### 9.1 半途而废之后：直接再跑一遍

2026-11-10 在 .150 上第一次跑，前四步全部成功，第 5 步被 §8.6 的 `2>&1` 掐断，
留下的是「公钥能登 + 密码也能登 + 一把一次性私钥在 `%TEMP%`」。远程确认的实际状态：

```
pubkeyauthentication yes
passwordauthentication yes          <- 本该是 no
administrators_authorized_keys: 2 行（1 行真 key + 1 行 cs-sshprobe）
%TEMP%\cs-sshprobe-fa7153af\id_probe       <- 私钥还在
```

**不需要手工收拾，再双击一次 `enable-ssh.cmd` 就行** —— 脚本现在是幂等的：

* 先打印 `[ok] removed N leftover self-test key line(s)` 和
  `[ok] removed N leftover self-test key dir(s) from %TEMP%`，把上一次的残留清掉；
* 第 3 步会说 `key already present`，不会写第二份；
* 自测通过后第 6 步才关密码，并**复测一次**；复测失败自动回滚 `sshd_config.bak-*`；
* 最后一次 `sshd -T` 应该读到 `passwordauthentication no`。

要在远程确认某台机器的状态（只读，不改任何东西）：

```bash
ssh <host> 'C:\Windows\System32\OpenSSH\sshd.exe -T -f C:\ProgramData\ssh\sshd_config | findstr /I "passwordauthentication kbdinteractiveauthentication"'
```

两条都得是 `no` 才算 key-only。2026-11-10 第二次跑完之后，.150 是：

```
pubkeyauthentication yes
passwordauthentication no
kbdinteractiveauthentication yes   <- 还留着一条输密码的路（§8.7）
```

所以还有第三次 —— 补 `KbdInteractiveAuthentication no`。这一步纯配置改动，
不再装任何东西，跑完就三行：`passwordauthentication no`、
`kbdinteractiveauthentication no`、`password auth off, key login still works`。
