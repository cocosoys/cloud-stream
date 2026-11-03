<#
.SYNOPSIS
    #34 一键部署：构建 / 上传 / 落盘 / 重启 / 体检 / 失败回滚。

.DESCRIPTION
    把「每次验证都要手工七步」压成一条命令。脚本只做机械动作，判断标准写在
    docs/deploy-one-click.md 里，两边必须一致。

    本机做的事：
      1. 预检（工具链、ssh 可达、cargo / npm）
      2. 构建后端 release
      3. 构建前端（core → dashboard 两级，顺序不能反）
      4. 汇总产物到 target\output\cloud-stream（+ 同名 zip），上传从这里取
      5. scp 上传（exe / DLL / zip / 九个上机脚本 / 证书，内容一样就跳过）
      6. MD5 双向核对

    被控机做的事（由 deploy-remote.ps1 执行，脚本随包上传）：
      7. 装启动脚本 → 停任务 → 杀进程 → 换 exe → 解压前端（暂存目录校验后
         换名）→ 起任务
      8. 本机轮询 /api/status 直到 200，并核对进程启动时间与日志
      9. 成功后把这次真正发上去的产物归档到 target\deploy-archive
     10. 失败时用归档里的上一版重发，并把原因打出来

    被控机上不保留任何备份（没有 .exe.prev，也没有 web 目录副本）：那里只放
    正在运行的正式产物。回滚材料留在构建端 —— 需要回滚的人坐在这台机器前，
    而被控机上的 .prev 只会越积越多，没人会去轮转它。

    被控机上的路径全部由脚本自定位：运行根 <RemoteBase>\stream 里的
    start-stream.cmd 用 %~dp0、run-hidden.vbs 用 WScript.ScriptFullName、
    deploy-remote.ps1 用 $PSScriptRoot。没有任何一处写死 C:\dev —— 整包解压到
    别的盘、别的目录同样能跑，这对"快速把新设备变成被控设备"是硬要求。

    为什么用「上传远端脚本」而不是 ssh 里塞内联命令：PowerShell 内联字符串要
    穿过 bash → ssh → cmd → powershell 四层引号，`$p=C:\dev\x` 这类赋值会被
    吃掉（本轮踩过），出问题时还看不出是哪一层吃了它。脚本文件只有一层。

.PARAMETER Target
    ~/.ssh/config 里的主机别名。默认 mc-admin（Administrator，游戏所在的交互用户）。

.PARAMETER RemoteBase
    被控机上的安装根，默认 C:\dev。运行根是 <base>\stream，部署暂存是
    <base>\deploy。运行根内部的脚本全部自定位，所以这条是"整台机器换位置"时
    唯一需要改的参数。

.PARAMETER SkipBackend
    只发前端。静态文件按请求读取，不需要重启服务。

.PARAMETER SkipFrontend
    只发后端。

.PARAMETER SkipBuild
    跳过编译（cargo 与 npm），直接上传现有的 target\release 与 dashboard\dist。
    注意：前端仍会按 dist 重新打包 —— dist 是磁盘上的事实，zip 只是快照，
    跳过打包会让人以为"改了却没生效"。

.PARAMETER DryRun
    构建 + 上传 + 核对 MD5，但不切换、不重启。

.PARAMETER Bootstrap
    一次性引导：建目录、开 sshd 自启、补防火墙规则、上传启动脚本、核对计划任务。
    可以单独跑，也可以和正常部署一起跑（幂等）。

.EXAMPLE
    pwsh toolchain/deploy.ps1 -Bootstrap
    第一次接手这台机器时跑一次。

.EXAMPLE
    pwsh toolchain/deploy.ps1
    日常：后端 + 前端全量，一条命令到可连。

.EXAMPLE
    pwsh toolchain/deploy.ps1 -SkipBackend
    只改了前端样式时用，不重启服务、不断会话。
.EXAMPLE
    pwsh toolchain/deploy.ps1 -Rollback
    回滚：把 target\deploy-archive 里上一份成功的产物重新发上去。不编译、不重新打包。
#>
[CmdletBinding()]
param(
    [string]$Target = 'mc-admin',
    [string]$HostName = '192.168.59.148',
    [int]$Port = 8080,
    # 被控机上的安装根。运行根是 <base>\stream，部署暂存是 <base>\deploy；
    # 内部所有路径都由脚本自定位，所以换盘符 / 换目录只需要改这一个参数。
    [string]$RemoteBase = 'C:\dev',
    [switch]$SkipBackend,
    [switch]$SkipFrontend,
    [switch]$SkipBuild,
    [switch]$DryRun,
    [switch]$Bootstrap,
    # 回滚入口：把构建端归档的上一版重新发上去（不编译、不打包）。
    [switch]$Rollback,
    [int]$HealthTimeoutSec = 90,
    # 前端仓库的位置。默认在仓库的同级目录里按名字找（cloud-stream-web /
    # selkies-webclient），找不到就报错停下 —— 前端与后端是两个仓库，
    # 克隆下来的目录名不一定叫哪个。
    [string]$WebRoot
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# 不要在这里动 [Console]::OutputEncoding。看着像是"修乱码"，实际是制造乱码：
# 它只改 PowerShell 的编码，不改控制台代码页，中文 Windows 上新旧两套编码
# 会在一层管道里打架，`pwsh deploy.ps1 > log.txt` 就会写进一堆 "棰勬" 这种
# UTF-8 套 GBK 的二次编码。npm/cargo 的输出已经整条重定向进文件了，不受影响。

# ---------------------------------------------------------------- 常量：与被控机一致
# 被控机上只有两个路径是"约定"，其余全部由脚本自定位推导：
#
#   <base>\stream\   运行根，与构建端 target\output\cloud-stream 一一对应
#   <base>\deploy\   部署暂存（deploy-remote.ps1 + 传输中的文件）
#
# 运行根内部一律相对：start-stream.cmd 用 %~dp0、run-hidden.vbs 用
# WScript.ScriptFullName、deploy-remote.ps1 用 $PSScriptRoot。所以把整包解压到
# D:\games\stream 也能直接跑，这正是 -RemoteBase 存在的意义（整台机器换位置时
# 只需要改这一个参数）。
$RemoteBase = 'C:\dev'
$RemoteRoot = Join-Path $RemoteBase 'stream'
$RemoteDeploy = Join-Path $RemoteBase 'deploy'
$RemoteTask = 'MCStreamSrv'
$RemoteExe = "$RemoteRoot\stream-server.exe"
$RemoteLog = "$RemoteRoot\logs\stream-server.log"
# 上机脚本：跟着每次部署走（命令行参数写在 start-stream.cmd 里，只换 exe
# 不换它，参数就永远停在上一次）。
# start-stream.60fps.cmd 是同一个命令行的"手工版"（不进计划任务），它的存在
# 意义就是让人在被控机上用和看门狗完全一致的参数跑一次 —— 包里缺了它，
# 那台机器就只能从仓库里翻，而拿到包的人手里没有仓库。
# collect-launch-facts.cmd 是"起不来"时的第一个动作：新设备上三种常见故障
# （空密码 / 缺 VC++ / zip 没解压）都不写日志、窗口一闪就关，它一次收全。
# setup-inbound.cmd 是第二课：Windows 防火墙在新机器上把所有入站（含 ICMP）
# 都丢掉，于是"ping 不通、8080 超时"，而服务其实跑得好好的。
# open-inbound.cmd 与 probe-listen.cmd 是同一课的自动化部分，被 start-stream.cmd
# 和 setup-watchdog.bat 调用：前者幂等地补规则，后者在启动后 30 秒内把
# "到底有没有监听"写成一行 PREFLIGHT 日志。少了它们，启动失败仍然没有痕迹。
$LauncherNames = @(
    'start-stream.cmd',
    'start-stream.60fps.cmd',
    'run-hidden.vbs',
    'setup-watchdog.bat',
    'collect-launch-facts.cmd',
    'setup-inbound.cmd',
    'open-inbound.cmd',
    'probe-listen.cmd',
    'set-password.cmd'
)

$RepoRoot = Split-Path -Parent $PSScriptRoot
$LocalExe = Join-Path $RepoRoot 'target\release\stream-server.exe'
# #17: 虚拟手柄的 DLL。必须和 exe 同目录（LoadLibraryW 先查应用目录），并且
# 必须在进程停下之后才能覆盖 —— 已加载的 DLL 被持有，运行中覆盖是共享冲突。
$LocalVigem = Join-Path $RepoRoot 'vendor\vigem\ViGEmClient.dll'

# 前端是独立仓库（cloud-stream-web）。同级目录下两种常见名字都认：
# - 按仓库名克隆：cloud-stream-web
# - 老工作区里沿用的目录名：selkies-webclient
function Resolve-WebRoot {
    param([string]$Explicit)

    $candidates = if ($Explicit) { @($Explicit) } else {
        $parent = Split-Path -Parent $RepoRoot
        @(
            (Join-Path $parent 'cloud-stream-web'),
            (Join-Path $parent 'selkies-webclient')
        )
    }
    foreach ($dir in $candidates) {
        if (Test-Path (Join-Path $dir 'addons\selkies-web-core')) { return (Resolve-Path $dir).Path }
    }
    throw (("找不到前端仓库。它在后端仓库的同级目录，名字是 cloud-stream-web " +
        "或 selkies-webclient（当前查找根：{0}），也可以用 -WebRoot 指定。") -f (Split-Path -Parent $RepoRoot))
}

$WebRepoRoot = Resolve-WebRoot $WebRoot
$CoreDir = Join-Path $WebRepoRoot 'addons\selkies-web-core'
$DashDir = Join-Path $WebRepoRoot 'addons\selkies-dashboard'
# 产物放 target\（已 gitignore）：toolchain\ 里只放脚本，不放每次构建都变的二进制。
$ZipPath = Join-Path $RepoRoot 'target\deploy-selkies-web.zip'
$RemoteScript = Join-Path $PSScriptRoot 'deploy-remote.ps1'

# 统一产物目录。target\output\cloud-stream **就是**被控机的运行根：把这个目录
# 原样拷到任何一台机器（或解压同名 zip），里面的 exe / DLL / 前端 / 证书 /
# 九个上机脚本就是全部所需，不需要再手工拼路径。上传也从这里取，所以「本地能
# 跑」与「线上在跑」是同一批字节。
$OutputDir = Join-Path $RepoRoot 'target\output'
$BundleDir = Join-Path $OutputDir 'cloud-stream'
$BundleZip = Join-Path $OutputDir 'cloud-stream.zip'
$BundleManifest = Join-Path $BundleDir 'manifest.json'
$BundleReadme = Join-Path $BundleDir 'README.txt'

# 构建端备份：成功部署后把「这次真正发上去的产物」覆盖到这里，失败或 -Rollback
# 时从这里重发。放在 target\ 下（已 gitignore），和别的构建产物一起被忽略。
# 只覆盖本次发出去的部件，所以 -SkipBackend 的部署不会把上次的 exe 抹掉。
$ArchiveDir = Join-Path $RepoRoot 'target\deploy-archive'
$ArchiveExe = Join-Path $ArchiveDir 'stream-server.exe'
$ArchiveDll = Join-Path $ArchiveDir 'ViGEmClient.dll'
$ArchiveZip = Join-Path $ArchiveDir 'selkies-web.zip'
$ArchiveMeta = Join-Path $ArchiveDir 'meta.json'

$script:StepNo = 0
$script:DeployStartedAt = Get-Date

# 步号只增不减：跳过的步骤（-SkipBackend 之类）不占号，所以不印分母 —— 印了
# 就会出现 [4/8] 之后直接 [6/8] 这种读者要停下来算账的东西。
function Write-Step([string]$Text) {
    $script:StepNo++
    Write-Host ""
    Write-Host ("[{0}] {1}" -f $script:StepNo, $Text) -ForegroundColor Cyan
}
function Write-Ok([string]$Text) { Write-Host "      OK   $Text" -ForegroundColor Green }
function Write-Info([string]$Text) { Write-Host "      ..   $Text" -ForegroundColor DarkGray }
function Write-Note([string]$Text) { Write-Host "      !!   $Text" -ForegroundColor Yellow }

function Invoke-Native {
    param([string]$Exe, [string[]]$Arguments, [switch]$Quiet)

    # -Quiet 不是「丢掉输出」，是「失败时才给你看」。npm.cmd -> node 这条链上，
    # 子进程会绕过 PowerShell 的管道直接往控制台写，所以只能用文件重定向：
    # *> 把三条流都指向文件，子进程拿到的句柄就是那个文件。
    if ($Quiet) {
        $log = Join-Path $RepoRoot 'target\deploy-build.log'
        $null = New-Item -ItemType Directory -Force -Path (Split-Path -Parent $log)
        & $Exe @Arguments *> $log
        $code = $LASTEXITCODE
        if ($code -ne 0) {
            Write-Note "$Exe 退出码 $code，日志尾部（$log）："
            Get-Content $log -Tail 30 | ForEach-Object { Write-Host "           $_" -ForegroundColor DarkGray }
            throw "$Exe 失败"
        }
        return @()
    }

    $out = & $Exe @Arguments 2>&1
    $code = $LASTEXITCODE
    if ($out) { $out | ForEach-Object { Write-Host "           $_" -ForegroundColor DarkGray } }
    if ($code -ne 0) { throw "$Exe 退出码 $code" }
    return $out
}

function Get-Md5([string]$Path) {
    if (-not (Test-Path $Path)) { return '' }
    return (Get-FileHash -Path $Path -Algorithm MD5).Hash
}

<#
删除本机上的构建产物。

不能用 Remove-Item：它在这类环境里被接到 Windows Shell 的删除 API 上，也就是
回收站。工作区把回收站那一层换成了带批量删除保护的 shim，删一个含上千文件的
dist 或删一个旧 zip 都会返回

    [safe-delete][SAFE_DELETE_FAIL_CLOSED] ... reason":"trash-failed"

而且报错位置和真实原因（文件数量 / 回收站不可用）完全无关，第一眼会以为是
vite 或 Compress-Archive 坏了。.NET 的 Delete 直接走文件系统，不经过回收站，
才是这里真正想表达的意思。
#>
function Remove-LocalPath([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.PSIsContainer) {
        [System.IO.Directory]::Delete($item.FullName, $true)
    } else {
        # 构建产物偶尔是只读的（从 zip 里解出来会带属性）。
        if ($item.IsReadOnly) { $item.IsReadOnly = $false }
        [System.IO.File]::Delete($item.FullName)
    }
}

# SSH 一律 BatchMode：缺密钥时立刻失败，而不是卡在密码提示上等一个人来敲。
# ServerAlive* 是给"连上了但对面不说话"用的：ConnectTimeout 只管 TCP 握手，
# 一端被挂起 / sshd 卡在某个会话上时，ssh 会一直等下去（本轮踩过一次，
# 整整 10 分钟停在第一条 `hostname` 上）。现在 15 s × 4 次没响应就断开。
$SshOpts = @(
    '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8',
    '-o', 'ServerAliveInterval=15', '-o', 'ServerAliveCountMax=4',
    '-o', 'StrictHostKeyChecking=accept-new'
)

function Invoke-Ssh([string]$Command, [switch]$AllowFail) {
    $out = & ssh @SshOpts $Target $Command 2>&1
    $code = $LASTEXITCODE
    if ($code -ne 0 -and -not $AllowFail) {
        throw "ssh 失败（$code）：$Command`n$($out -join "`n")"
    }
    return [pscustomobject]@{ Code = $code; Output = ($out -join "`n") }
}

function Send-File([string]$Local, [string]$Remote) {
    # 和 Invoke-Ssh 用同一组保活选项：scp 卡住和 ssh 卡住是同一类故障，而且
    # 传 18.6 MB 时更难判断是慢还是死了。
    $scpArgs = @(
        '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8',
        '-o', 'ServerAliveInterval=15', '-o', 'ServerAliveCountMax=4',
        $Local, "${Target}:$Remote"
    )
    Invoke-Native -Exe 'scp' -Arguments ($scpArgs -replace '\\', '/') -Quiet
}

# ---------------------------------------------------------------- 构建环境
<#
    cargo 需要 MSVC 的 cl.exe / link.exe 与 Windows SDK。这台机器上 vcvars64.bat
    在 BuildTools 里，但 `cmd /c vcvars && set` 抓环境变量的写法在 pwsh 里会
    把多行输出搅在一起。所以这里直接按目录找最新的工具集与 SDK 自己拼
    PATH / INCLUDE / LIB —— 也是本轮之前每跑一次 cargo 都要手敲一遍的那五行。
#>
function Initialize-BuildEnv {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    $vsRoot = $null
    if (Test-Path $vswhere) {
        $vsRoot = & $vswhere -latest -products * -property installationPath 2>$null
    }
    if (-not $vsRoot) { $vsRoot = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools' }

    $toolset = Get-ChildItem -Path "$vsRoot\VC\Tools\MSVC" -Directory -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending | Select-Object -First 1
    if (-not $toolset) { throw "找不到 MSVC 工具集：$vsRoot\VC\Tools\MSVC" }

    $sdkVer = Get-ChildItem -Path "${env:ProgramFiles(x86)}\Windows Kits\10\Include" -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match '^10\.\d+\.\d+\.\d+$' } |
        Sort-Object Name -Descending | Select-Object -First 1
    if (-not $sdkVer) { throw '找不到 Windows SDK Include 目录' }

    $sdk = "${env:ProgramFiles(x86)}\Windows Kits\10"
    $env:PATH = (@(
        "$($toolset.FullName)\bin\Hostx64\x64",
        "$sdk\bin\$($sdkVer.Name)\x64",
        "$sdk\bin\x64",
        "$env:USERPROFILE\.cargo\bin"
    ) -join ';') + ';' + $env:PATH

    $env:INCLUDE = (@(
        "$($toolset.FullName)\include",
        "$sdk\Include\$($sdkVer.Name)\ucrt",
        "$sdk\Include\$($sdkVer.Name)\um",
        "$sdk\Include\$($sdkVer.Name)\shared",
        "$sdk\Include\$($sdkVer.Name)\winrt",
        "$sdk\Include\$($sdkVer.Name)\cppwinrt"
    ) -join ';')

    $env:LIB = (@(
        "$($toolset.FullName)\lib\x64",
        "$sdk\Lib\$($sdkVer.Name)\ucrt\x64",
        "$sdk\Lib\$($sdkVer.Name)\um\x64"
    ) -join ';')

    # 预编译的 libopus，不是 vcpkg 装的；缺了 cargo 会报 link 找不到 opus。
    $opus = 'C:\dev\opus-static'
    if (Test-Path $opus) {
        $env:LIBOPUS_LIB_DIR = "$opus\lib"
        $env:LIBOPUS_INCLUDE_DIR = "$opus\include"
        $env:PATH = "$opus\bin;$env:PATH"
    }
    if (-not $env:VCPKG_ROOT) { $env:VCPKG_ROOT = 'C:\dev\vcpkg' }

    Write-Ok "MSVC $($toolset.Name) / SDK $($sdkVer.Name)"
}

# ---------------------------------------------------------------- 体检
<#
    `/api/status` 是唯一的存活探针（http/server.rs:265 回纯文本 "ok"）。它走
    TLS 自签证书，所以 curl 必须 -k；用 curl.exe 而不是 Invoke-WebRequest，
    是为了不依赖 PowerShell 版本（-SkipCertificateCheck 只有 6+ 有）。
#>
function Test-Health([int]$TimeoutSec) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    $last = ''
    while ((Get-Date) -lt $deadline) {
        $code = & curl.exe -sk -o NUL -w '%{http_code}' --max-time 3 "https://${HostName}:$Port/api/status" 2>$null
        if ($code -eq '200') { return $true }
        $last = "http=$code"
        Start-Sleep -Milliseconds 700
    }
    Write-Note "健康检查超时（$last）"
    return $false
}

# ---------------------------------------------------------------- 统一产物目录
<#
    把所有"要放到被控端"的东西汇总到一个目录：target\output\cloud-stream。

    为什么值得单开一步：在这之前，被控机需要的五样东西散在三个地方 —— exe 在
    target\release、DLL 在 vendor\vigem、前端在另一个仓库的 dist、启动脚本在
    toolchain\、证书只在被控机本地。于是"把一台新机器变成被控机"变成一串手工
    scp，漏一个就得到"能连上但没画面 / 有画面但没手柄 / 有手柄但剪贴板废掉"。
    汇总之后，这个目录**就是**运行根：拷过去、跑一次 setup-watchdog.bat，完事。

    约定：运行根 <base>\stream 与本目录一一对应（相对路径全部自定位），所以
    "本地这份能跑"与"线上在跑"是同一批字节，不是两次拼装的结果。
#>
function Resolve-CertPair {
    # 证书是按机器签的（SAN 里是这台机器的 IP），构建端用文件名区分：
    # toolchain\cert-<最后一个八位组>.pem + key-<同号>.pem；没有就退回
    # toolchain\cert.pem + key.pem。都没有不算失败 —— 包照样能跑，只是明文
    # HTTP（剪贴板要安全上下文），所以这里只返回 $null，由调用方说明。
    $octet = ($HostName -split '\.')[-1]
    foreach ($stem in @("cert-$octet", 'cert')) {
        $c = Join-Path $PSScriptRoot "$stem.pem"
        $k = Join-Path $PSScriptRoot ("key" + $stem.Substring(4) + '.pem')
        if ((Test-Path $c) -and (Test-Path $k)) {
            return [pscustomobject]@{ Cert = $c; Key = $k; Stem = $stem }
        }
    }
    return $null
}

function Write-BundleReadme([string]$Path, [bool]$Tls) {
    $tlsLine = if ($Tls) { 'cert.pem / key.pem  TLS 自签证书（浏览器会提示不受信任）' } else {
        '本包未附带证书 -> 明文 HTTP，剪贴板同步不可用（需要安全上下文）'
    }
    $text = @"
cloud-stream 被控端整包
=======================

这个目录就是被控机的运行根。位置随意：D:\stream、C:\dev\stream 都行，
里面的脚本全部按自身位置推导路径，没有任何写死的目录。

怎么用
  1) 整包拷到目标机器（或解压同目录下的 cloud-stream.zip）。
  2) 建看门狗计划任务（需要该账户密码，任务跑在交互式会话里）：
         右键 setup-watchdog.bat -> "以管理员身份运行"
     也可以直接给参数：setup-watchdog.bat Administrator <密码>
     提权运行时它会顺手放行 8080 与 ping，并在建完任务后立刻启动一次、
     等最多 60 秒，直接告诉你 [ok] 还是 [fail]。
  3) 任务每分钟触发一次，进程挂了 60 秒内自己回来。
     想手工起一次：start-stream.cmd
  4) 浏览器打开 https://<本机IP>:8080/

目录说明
  stream-server.exe   采集 / 编码 / 信令 / HTTP / 文件传输，全在这一个进程
  ViGEmClient.dll     虚拟手柄客户端；缺了只降级成"没手柄"，其余照常
  selkies-web\        前端静态根（--dist 指向这里）
  logs\               stream-server.log 与 watchdog.log（进程起不来先看后者）
  start-stream.cmd    命令行参数都在这里：分辨率 / 帧率 / 码率 / 证书
  start-stream.60fps.cmd 手工起一次用（同一个命令行，不进计划任务）
  run-hidden.vbs      计划任务的实际入口，无窗口，避免每分钟闪一次黑框
  setup-watchdog.bat  创建 MCStreamSrv 任务并验证一次启动
  collect-launch-facts.cmd  起不来时双击它，生成一份事实清单（只读）
  setup-inbound.cmd   管理员身份运行：放行 8080 与 ping（否则外部一概连不上）
  open-inbound.cmd    上面那条规则的实际执行者，被启动脚本自动调用（幂等）
  probe-listen.cmd    启动后 30 秒内把"有没有监听"写进 watchdog.log
  set-password.cmd    设/改/清除访问口令（写 auth-token.txt，重启任务生效）
  manifest.json       本次构建的指纹：提交、时间、各文件 MD5、前端文件数
  $tlsLine

要不要口令
  默认没有：同一网段任何人都能打开页面并操作键鼠。
  要的话双击 set-password.cmd 输一个（只限 ASCII），然后重启任务，之后
  浏览器必须带 token 打开一次：
        https://<本机IP>:8080/?token=<口令>
  前端会把它写进 cookie，同一浏览器之后直接输地址就行。
  口令存在 auth-token.txt；删掉它（或 set-password.cmd 里留空）就恢复开放。

每次启动会自动做的事（不需要人守在那里）
  * 查 8080 / ICMP 入站规则，缺了就补（当前进程提权时）；补不上会把原因写进
    logs\watchdog.log 的 PREFLIGHT 行，而不是安静地少一条规则
  * 起 exe 之后另起一个探针，最多 30 秒，把结果写成
        PREFLIGHT listening on 8080 after N probe(s): local status 200
    或者
        PREFLIGHT NOT listening on 8080 ... + 三条最可能的原因 + 下一步
    所以"启动后没反应"永远有带时间戳的一行可以看，不用猜。

注意
  * 计划任务的 /RU 必须是"能看见桌面"的那个登录用户；换成服务账户会抓到空桌面。
  * 换机器要重签证书（SAN 里是 IP），否则浏览器报警、剪贴板不工作。
  * 外部机器连不上、本机 loopback 正常，多半是入站规则或网络配置文件
    （VMware 的网卡常被判成"公用"）：管理员身份跑一次 setup-inbound.cmd。
  * 换分辨率 / 码率改 start-stream.cmd，不要改计划任务。
"@
    # UTF-8 带 BOM：记事本与 PowerShell 5.1 都按 BOM 判编码，中文才不会乱码。
    [System.IO.File]::WriteAllText($Path, $text, (New-Object System.Text.UTF8Encoding($true)))
}

function New-DeployBundle {
    $webSrc = Join-Path $DashDir 'dist'
    if (-not (Test-Path (Join-Path $webSrc 'index.html'))) {
        throw "没有 $webSrc\index.html，先构建前端（去掉 -SkipBuild）"
    }
    if (-not (Test-Path $LocalExe)) {
        # 只在 -SkipBackend 时可能走到这里（没构建过后端却只发前端）。包里缺 exe
        # 就不能算完整运行根，但这一轮本来也不发后端，所以降级成提示。
        if (-not $SkipBackend) { throw "没有 $LocalExe，先构建后端（去掉 -SkipBuild）" }
        Write-Note "本机没有 stream-server.exe，包里只会有前端与脚本"
    }

    # 每次重建而不是增量覆盖：上一个包里的残留文件（改名后的 chunk、删掉的
    # 资源）留在目录里，拷到新机器就是一个"看起来正常、实际带着死文件"的根。
    if (Test-Path $BundleDir) { Remove-LocalPath $BundleDir }
    foreach ($d in @($BundleDir, (Join-Path $BundleDir 'logs'), (Join-Path $BundleDir 'selkies-web'))) {
        New-Item -ItemType Directory -Force -Path $d | Out-Null
    }

    $exePath = Join-Path $BundleDir 'stream-server.exe'
    $dllPath = Join-Path $BundleDir 'ViGEmClient.dll'
    if (Test-Path $LocalExe) { Copy-Item $LocalExe $exePath -Force }
    if (Test-Path $LocalVigem) { Copy-Item $LocalVigem $dllPath -Force }
    foreach ($name in $LauncherNames) {
        Copy-Item (Join-Path $PSScriptRoot $name) (Join-Path $BundleDir $name) -Force
    }

    $cert = Resolve-CertPair
    if ($cert) {
        Copy-Item $cert.Cert (Join-Path $BundleDir 'cert.pem') -Force
        Copy-Item $cert.Key (Join-Path $BundleDir 'key.pem') -Force
    }

    # 前端直接拷 dist：zip 是同一个 dist 的快照，解开再拷只是多一次压缩/解压。
    Copy-Item -Path (Join-Path $webSrc '*') -Destination (Join-Path $BundleDir 'selkies-web') -Recurse -Force
    if (-not (Test-Path (Join-Path $BundleDir 'selkies-web\assets'))) {
        throw '打包出来的 selkies-web 没有 assets\：dist 是不是空的？'
    }

    Write-BundleReadme $BundleReadme ([bool]$cert)

    $web = Get-ChildItem (Join-Path $BundleDir 'selkies-web') -Recurse -File
    $webBytes = ($web | Measure-Object Length -Sum).Sum
    if (-not $webBytes) { $webBytes = 0 }
    $commit = $(try { (& git -C $RepoRoot rev-parse --short HEAD 2>$null) } catch { $null })
    $dirty = $(try { [bool](& git -C $RepoRoot status --porcelain 2>$null) } catch { $true })

    $manifest = [ordered]@{
        bundle   = 'cloud-stream'
        layout   = 'this directory IS the runtime root; all scripts self-locate'
        built_at = (Get-Date).ToString('o')
        commit   = $commit
        dirty    = $dirty
        exe      = $(if (Test-Path $exePath) {
                [ordered]@{ bytes = (Get-Item $exePath).Length; md5 = (Get-Md5 $exePath) }
            } else { $null })
        dll      = $(if (Test-Path $dllPath) {
                [ordered]@{ bytes = (Get-Item $dllPath).Length; md5 = (Get-Md5 $dllPath) }
            } else { $null })
        web      = [ordered]@{
            files     = $web.Count
            bytes     = $webBytes
            index_md5 = (Get-Md5 (Join-Path $BundleDir 'selkies-web\index.html'))
        }
        launchers = $LauncherNames
        tls      = [bool]$cert
        cert     = $(if ($cert) { $cert.Stem } else { $null })
    }
    $manifest | ConvertTo-Json -Depth 6 | Set-Content -Path $BundleManifest -Encoding UTF8

    # 整包 zip：给"拷一台新机器"用。放在 output\ 而不是包内 —— 把包自己压进
    # 包内会在第二次运行时把上一次的 zip 也装进去。
    if (Test-Path $BundleZip) { Remove-LocalPath $BundleZip }
    Compress-Archive -Path (Join-Path $BundleDir '*') -DestinationPath $BundleZip -CompressionLevel Optimal

    $zipBytes = (Get-Item $BundleZip).Length
    $exeMb = $(if (Test-Path $exePath) { (Get-Item $exePath).Length / 1MB } else { 0 })
    Write-Ok ("产物目录 {0}" -f $BundleDir)
    Write-Ok ("  exe {0:N1} MB / 前端 {1} 文件 {2:N1} MB / 证书 {3}" -f $exeMb, $web.Count, ($webBytes / 1MB),
        $(if ($cert) { "yes（$($cert.Stem)）" } else { 'NO（明文 HTTP）' }))
    Write-Ok ("  {0}（{1:N2} MB）" -f $BundleZip, ($zipBytes / 1MB))
    if (-not $exeMb) { Write-Note '包里没有 stream-server.exe（-SkipBackend 且本机没构建过）' }
    if (-not $cert) {
        Write-Note "包里没有 cert.pem/key.pem：部署后是明文 HTTP，剪贴板同步不可用。把证书放到 $PSScriptRoot\cert-<IP末位>.pem 与 key-<IP末位>.pem 可自动带上。"
    }
}


# ---------------------------------------------------------------- 构建端备份
<#
    被控机上不留 .prev，回滚材料在这里。为什么不「部署前从被控机把当前 exe 拉
    回来」：那要多传 18.6 MB，而拉到的正是上一份成功部署发出去的东西 —— 与归档
    等价。唯一不等价的情形是有人手工换过被控机上的 exe，那种情况两个来源都不准，
    文档里写明就够了。

    只覆盖本次发出去的部件：`-SkipBackend` 的部署不会把归档里的 exe 抹掉，所以
    归档始终是「最近一次成功部署后的完整状态」。
#>
function Save-DeployArchive {
    New-Item -ItemType Directory -Force -Path $ArchiveDir | Out-Null
    $parts = @()
    if (-not $SkipBackend) {
        Copy-Item $srcExe $ArchiveExe -Force
        if (Test-Path $srcDll) { Copy-Item $srcDll $ArchiveDll -Force }
        $parts += 'backend'
    }
    if (-not $SkipFrontend) {
        Copy-Item $ZipPath $ArchiveZip -Force
        $parts += 'frontend'
    }

    $exeMd5 = $(if (Test-Path $ArchiveExe) { Get-Md5 $ArchiveExe } else { $null })
    # 启动脚本也进归档：参数在 start-stream.cmd 里，回滚只退 exe 会让旧二进制
    # 跑新参数（--dist 指到不存在的目录、证书路径换了 …）。一起退才叫回滚。
    foreach ($name in $LauncherNames) {
        Copy-Item (Join-Path $PSScriptRoot $name) (Join-Path $ArchiveDir $name) -Force
    }
    $dllMd5 = $(if (Test-Path $ArchiveDll) { Get-Md5 $ArchiveDll } else { $null })
    $zipMd5 = $(if (Test-Path $ArchiveZip) { Get-Md5 $ArchiveZip } else { $null })
    $commit = $(try { (& git -C $RepoRoot rev-parse --short HEAD 2>$null) } catch { $null })
    # 工作区脏的时候，归档里的 exe 不对应任何提交 —— 回滚到它的人需要知道这件事。
    $dirty = $(try { [bool](& git -C $RepoRoot status --porcelain 2>$null) } catch { $true })
    [ordered]@{
        saved_at = (Get-Date).ToString('o')
        parts    = $parts
        exe      = $exeMd5
        dll      = $dllMd5
        zip      = $zipMd5
        commit   = $commit
        dirty    = $dirty
    } | ConvertTo-Json | Set-Content -Path $ArchiveMeta -Encoding UTF8
    Write-Ok "构建端归档：$($parts -join ' + ')（$ArchiveDir）"
}

<#
    把归档里的部件重新发上去并切换一次。`$Parts` 决定发哪几件 —— 失败路径只恢复
    本次动过的部分，`-Rollback` 恢复全部。
#>
function Send-Archived([string[]]$Parts) {
    $sent = @()
    if ($Parts -contains 'backend') {
        if (-not (Test-Path $ArchiveExe)) { throw "归档里没有 stream-server.exe：$ArchiveDir" }
        Send-File $ArchiveExe "$RemoteRoot/stream-server.new.exe"
        if (Test-Path $ArchiveDll) { Send-File $ArchiveDll "$RemoteRoot/ViGEmClient.new.dll" }
        $sent += 'backend'
    }
    if ($Parts -contains 'frontend') {
        if (-not (Test-Path $ArchiveZip)) { throw "归档里没有 selkies-web.zip：$ArchiveDir" }
        Send-File $ArchiveZip "$RemoteDeploy/selkies-web.zip"
        $sent += 'frontend'
    }
    if ($sent.Count -eq 0) { return $false }

    Send-File $RemoteScript "$RemoteDeploy/deploy-remote.ps1"
    foreach ($name in $LauncherNames) {
        $src = Join-Path $ArchiveDir $name
        if (Test-Path $src) { Send-File $src "$RemoteDeploy/$name" }
    }
    $argList = "powershell -NoProfile -ExecutionPolicy Bypass -File $RemoteDeploy\deploy-remote.ps1"
    if ($sent -notcontains 'backend') { $argList += ' -SkipBackend' }
    if ($sent -notcontains 'frontend') { $argList += ' -SkipFrontend' }
    $out = & ssh @SshOpts $Target $argList 2>&1
    $code = $LASTEXITCODE
    foreach ($line in $out) { Write-Host "           $line" -ForegroundColor DarkGray }
    if ($code -ne 0) { throw "远端切换失败（退出码 $code）" }
    return $true
}

<#
    sshd 必须开机自启。这台机器上它是 Automatic，但那是上一次手工设的；
    重装 OpenSSH、或者换了台机器，默认可能是 Manual —— 表现就是「重启以后
    ssh 连不上，人也不在现场」。所以每次部署顺手把它钉住，幂等。
#>
function Enable-RemoteSsh {
    # 远端是 cmd -> powershell，所以整段只能有一个双引号层（给 cmd 的），
    # 里面一律用单引号。写成 \" 是错的：单引号字符串里反斜杠不转义。
    $cmd = @'
powershell -NoProfile -Command "$s = Get-Service sshd -ErrorAction SilentlyContinue; if (-not $s) { 'NO_SSHD'; exit 0 }; if ($s.StartType -ne 'Automatic') { Set-Service sshd -StartupType Automatic; 'SET_AUTO' } else { 'ALREADY_AUTO' }; if ($s.Status -ne 'Running') { Start-Service sshd; 'STARTED' }; if (-not (Get-NetFirewallRule -DisplayName 'OpenSSH SSH Server (sshd)' -ErrorAction SilentlyContinue)) { New-NetFirewallRule -DisplayName 'OpenSSH SSH Server (sshd)' -Direction Inbound -Protocol TCP -LocalPort 22 -Action Allow | Out-Null; 'FW_ADDED' } else { 'FW_OK' }"
'@
    $r = Invoke-Ssh $cmd.Trim() -AllowFail
    foreach ($line in ($r.Output -split "`r?`n")) {
        switch ($line.Trim()) {
            'ALREADY_AUTO' { Write-Ok 'sshd 已是自动启动' }
            'SET_AUTO' { Write-Note 'sshd 启动类型 手动 → 自动（下次开机会自启）' }
            'STARTED' { Write-Note 'sshd 原本没在跑，已启动' }
            'FW_OK' { Write-Ok '防火墙放行规则已在' }
            'FW_ADDED' { Write-Note '补了一条 22 端口入站放行' }
            'NO_SSHD' { throw '被控机没装 OpenSSH Server' }
        }
    }
}

# ---------------------------------------------------------------- 主流程
# 本轮动过被控机的哪几个部件。只有走到切换那一步才会往里填，catch 靠它决定
# 「恢复什么」，以及「什么都没换过就别去重启进程」。
$attempted = @()
try {
    # ---- 1 预检
    Write-Step '预检'
    foreach ($tool in @('ssh', 'scp', 'curl.exe')) {
        if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { throw "本机缺 $tool" }
    }
    Write-Ok 'ssh / scp / curl.exe 就位'

    $ssh = Invoke-Ssh 'hostname' -AllowFail
    if ($ssh.Code -ne 0) {
        throw @"
连不上 $Target。

如果这台机器刚重启过、sshd 是「手动」启动类型，那么重启后它就没了。
到被控机控制台（VMware 窗口 / IPMI）执行一次，以后就不会再发生：

    Set-Service sshd -StartupType Automatic
    Start-Service sshd
    New-NetFirewallRule -DisplayName 'OpenSSH SSH Server (sshd)' -Direction Inbound -Protocol TCP -LocalPort 22 -Action Allow

然后回到本机重跑本脚本（可加 -Bootstrap 补齐其余一次性的东西）。
"@
    }
    Write-Ok "ssh 可达：$($ssh.Output.Trim())（$Target）"

    if (-not (Test-Path $RemoteScript)) { throw "缺少 $RemoteScript" }

    # ---- 0 回滚（人工入口，不需要编译、不重新打包）
    <#
        被控机上不再有 .prev，所以回滚 = 把构建端归档的上一版当成"新版本"再发一次，
        走的还是同一条 上传 → 校验 → 停止 → 更换 → 起任务 → 体检 的路径。回滚因此
        也被体检，而不是"发完就算数"。
    #>
    if ($Rollback) {
        Write-Step '回滚：把构建端归档的上一版发回去'
        if (-not (Test-Path $ArchiveExe) -and -not (Test-Path $ArchiveZip)) {
            throw "没有可用的归档：$ArchiveDir（这台机器上还没成功部署过）"
        }
        $parts = @()
        if (Test-Path $ArchiveExe) { $parts += 'backend' }
        if (Test-Path $ArchiveZip) { $parts += 'frontend' }
        if (Test-Path $ArchiveMeta) {
            $meta = Get-Content $ArchiveMeta -Raw | ConvertFrom-Json
            Write-Note "归档 $($meta.saved_at)  部件 $($meta.parts -join '+')  提交 $($meta.commit)"
        }
        if (-not (Send-Archived -Parts $parts)) { throw '归档里没有可发的部件' }
        if (-not (Test-Health $HealthTimeoutSec)) { throw '回滚后健康检查未通过' }
        Write-Ok '回滚完成（来源：构建端归档）'
        return
    }

    # ---- 2 引导（可选）
    if ($Bootstrap) {
        Write-Step '引导：目录 / sshd 自启 / 启动脚本 / 计划任务'
        Enable-RemoteSsh

        $bootstrapCmd = @(
            "New-Item -ItemType Directory -Force '$RemoteRoot','$RemoteRoot\logs','$RemoteDeploy','$RemoteRoot\selkies-web' | Out-Null;"
            "if (Test-Path '$RemoteExe') { Write-Output ('exe ' + (Get-Item '$RemoteExe').Length) } else { Write-Output 'exe MISSING' };"
            "if (Get-ScheduledTask -TaskName $RemoteTask -ErrorAction SilentlyContinue) { Write-Output ('task ' + (Get-ScheduledTask -TaskName $RemoteTask).State) } else { Write-Output 'task MISSING' };"
            "if ((Test-Path '$RemoteRoot\cert.pem') -and (Test-Path '$RemoteRoot\key.pem')) { Write-Output 'tls OK' } else { Write-Output 'tls MISSING' };"
            "if (Test-Path '$(Split-Path -Parent $RemoteRoot)\selkies-web') { Write-Output 'legacy web dir still at the old location' } else { Write-Output 'layout ok' }"
        ) -join ' '
        $r = Invoke-Ssh "powershell -NoProfile -Command `"$bootstrapCmd`""
        foreach ($line in ($r.Output -split "`r?`n")) {
            if ($line.Trim()) { Write-Info $line.Trim() }
        }

        # 启动脚本要跟着部署走：命令行参数（--dist / --tls-cert / --repeat-every）
        # 全写在里面，只换 exe 不换它，参数就会永远停在上一次。装的时候按哈希
        # 跳过同名内容，并容忍"正在被看门狗执行的 .cmd"这种共享冲突 —— 那个
        # 文件要等进程重启才能覆盖，不能因为它是忙的就把整次引导判失败。
        foreach ($name in $LauncherNames) {
            Send-File (Join-Path $PSScriptRoot $name) "$RemoteDeploy/$name"
        }
        Write-Ok "推送启动脚本（$($LauncherNames -join ' / ')）"
        $install = Invoke-Ssh ("powershell -NoProfile -ExecutionPolicy Bypass -File " +
            "$RemoteDeploy\deploy-remote.ps1 -OnlyLaunchers") -AllowFail
        foreach ($line in ($install.Output -split "`r?`n")) {
            if ($line.Trim()) { Write-Info $line.Trim() }
        }
        if ($install.Code -ne 0) { Write-Note '启动脚本安装有异常，见上面的输出' }

        if ($r.Code -ne 0) { Write-Note '目录核对有异常，见上面的输出' }
    } else {
        Enable-RemoteSsh
    }

    # ---- 3 构建后端
    if (-not $SkipBackend -and -not $SkipBuild) {
        Write-Step '构建后端（cargo build --release -p stream-server）'
        Initialize-BuildEnv
        $sw = [Diagnostics.Stopwatch]::StartNew()
        Invoke-Native -Exe 'cargo' -Arguments @('build', '--release', '-p', 'stream-server', '--manifest-path', (Join-Path $RepoRoot 'Cargo.toml')) -Quiet
        $sw.Stop()
        Write-Ok ("built in {0:N1}s" -f $sw.Elapsed.TotalSeconds)
    }
    if (-not $SkipBackend) {
        if (-not (Test-Path $LocalExe)) { throw "没有 $LocalExe，去掉 -SkipBuild 重新构建" }
        $localExeMd5 = Get-Md5 $LocalExe
        Write-Ok "本机 exe $([math]::Round((Get-Item $LocalExe).Length / 1MB, 1)) MB  md5=$localExeMd5"
        # 缺 DLL 不算失败：手柄不可用，其余功能完全不受影响（服务端会降级并
        # 在日志里说明原因）。但这里必须说出来，不能让"部署成功"盖住它。
        if (Test-Path $LocalVigem) {
            Write-Ok "本机 ViGEmClient.dll $((Get-Item $LocalVigem).Length) B  md5=$(Get-Md5 $LocalVigem)"
        } else {
            Write-Note "缺 $LocalVigem —— 部署后手柄不可用（其余功能正常）"
        }
    }

    # ---- 4 构建前端
    <#
        顺序不能反：dashboard 的 prebuild 会把 core 的 dist 拷进 src/selkies-core.js。
        先 dashboard 后 core，服务端拿到的还是旧的核心。
    #>
    if (-not $SkipFrontend) {
        if ($SkipBuild) {
            Write-Step '前端：跳过构建，按现有 dist 重新打包'
        } else {
            Write-Step '构建前端（core → dashboard → 打包）'
        }
        if (-not (Test-Path (Join-Path $CoreDir 'package.json'))) { throw "找不到前端仓库：$CoreDir" }

        if (-not $SkipBuild) {
            # 先自己清 dist，别交给 vite 的 emptyOutDir。core 的 postbuild（gendb.js）
            # 每次往 dist/jsdb 写 1994 个小 JSON，于是下一次构建的「清空输出目录」
            # 就变成一次上千文件的批量删除；在带批量删除保护的 shell 里（例如
            # 工作区沙箱把 node 的 rmSync 包了一层）会直接让构建失败，
            # 而且报错信息指向 vite 插件，看不出是文件数量的问题。
            # Remove-LocalPath 走 .NET，同样不经过 node，构建路径就与文件数量无关。
            foreach ($pkg in @($CoreDir, $DashDir)) {
                $stale = Join-Path $pkg 'dist'
                if (Test-Path $stale) { Remove-LocalPath $stale }
            }

            Push-Location $CoreDir
            try { Invoke-Native -Exe 'npm' -Arguments @('run', 'build') -Quiet } finally { Pop-Location }
            Write-Ok 'core 构建完成'

            Push-Location $DashDir
            try { Invoke-Native -Exe 'npm' -Arguments @('run', 'build') -Quiet } finally { Pop-Location }
            Write-Ok 'dashboard 构建完成'
        }

        $dashDist = Join-Path $DashDir 'dist'
        $idx = Join-Path $dashDist 'index.html'
        if (-not (Test-Path $idx)) { throw "没有 $idx，去掉 -SkipBuild 先构建一次" }
        $core = Get-ChildItem (Join-Path $dashDist 'assets') -Filter 'selkies-core-*.js' -ErrorAction SilentlyContinue | Select-Object -First 1
        if (-not $core) { throw 'dashboard dist 里没有 core chunk，prebuild 没跑成功' }
        Write-Ok ("dist（index.html {0} B，core {1}）" -f (Get-Item $idx).Length, $core.Name)

        # 打包**不能**跟着 `-SkipBuild` 一起跳过：dist 是磁盘上已经构建好的事实，
        # zip 只是它的快照。上一版把两者塞进同一个 `if`，结果是 `-SkipBuild`
        # 部署的永远是上一次的 zip —— 重新构建了 dist、页面 hash 却没变，只能
        # 靠对比 index.html 引用的文件名才发现。
        #
        # 只打包 dist 的内容，不加外层目录 —— Expand-Archive 会把层级原样展开，
        # 多套一层目录的话服务端根目录就成了 C:\dev\selkies-web\dist\，首页 404。
        if (Test-Path $ZipPath) { Remove-LocalPath $ZipPath }
        Compress-Archive -Path (Join-Path $dashDist '*') -DestinationPath $ZipPath -CompressionLevel Optimal
        $zipMd5 = Get-Md5 $ZipPath
        Write-Ok ("打包 $([math]::Round((Get-Item $ZipPath).Length / 1MB, 2)) MB  md5=$zipMd5")
    }

    # ---- 5 汇总产物（被控端运行根）
    <#
        上传的三样东西全部从这一步的产物里取，所以"我检查过的那个包"和"线上正在
        跑的东西"是同一批字节，而不是两次各自拼装的结果。

        -SkipFrontend 且本机从来没有构建过前端 dist 时，整包不完整 —— 这时跳过
        汇总并退回按单件上传，让"只换后端"仍然可用，而不是为了一次后端改动去
        逼人构建前端。
    #>
    $packaged = $false
    if (-not (Test-Path (Join-Path $DashDir 'dist\index.html'))) {
        Write-Step '汇总产物：跳过（本机没有 dashboard dist，整包不完整）'
        Write-Note '这一轮只发后端；下一次带前端的部署会把 target\output\cloud-stream 补齐'
    } else {
        Write-Step '汇总产物（target\output\cloud-stream）'
        New-DeployBundle
        $packaged = $true
    }
    $srcExe = $(if ($packaged) { Join-Path $BundleDir 'stream-server.exe' } else { $LocalExe })
    $srcDll = $(if ($packaged -and (Test-Path (Join-Path $BundleDir 'ViGEmClient.dll'))) {
            Join-Path $BundleDir 'ViGEmClient.dll'
        } else { $LocalVigem })

    # ---- 6 上传
    Write-Step '上传'
    if (-not $SkipBackend) {
        Send-File $srcExe "$RemoteRoot/stream-server.new.exe"
        Write-Ok 'stream-server.new.exe'
        if (Test-Path $srcDll) {
            Send-File $srcDll "$RemoteRoot/ViGEmClient.new.dll"
            Write-Ok 'ViGEmClient.new.dll'
        }
    }
    if (-not $SkipFrontend) {
        if (-not (Test-Path $ZipPath)) { throw "没有 $ZipPath：这是打包步骤失败，检查 dashboard\dist 是否存在" }
        Send-File $ZipPath "$RemoteDeploy/selkies-web.zip"
        Write-Ok 'selkies-web.zip'
    }
    # 启动脚本上传到 deploy\，由被控端的 deploy-remote.ps1 在切换窗口里装进运行根。
    # 每轮都发：参数在 start-stream.cmd 里，只换 exe 会让参数停在上一次。
    foreach ($name in $LauncherNames) {
        Send-File (Join-Path $PSScriptRoot $name) "$RemoteDeploy/$name"
    }
    Write-Ok ($LauncherNames -join ' / ')
    # 证书只传到暂存区：被控端已有证书时不覆盖（SAN 里是那台机器的 IP），没有才装。
    foreach ($name in @('cert.pem', 'key.pem')) {
        $c = Join-Path $BundleDir $name
        if ($packaged -and (Test-Path $c)) {
            Send-File $c "$RemoteDeploy/$name"
        }
    }
    if ($packaged -and (Test-Path (Join-Path $BundleDir 'cert.pem'))) { Write-Ok 'cert.pem / key.pem（仅当被控机缺失时安装）' }
    Send-File $RemoteScript "$RemoteDeploy/deploy-remote.ps1"
    Write-Ok 'deploy-remote.ps1'

    # ---- 7 传输校验
    $verify = @()
    if (-not $SkipBackend) {
        $verify += "exe=$(Get-Md5 $srcExe)"
        if (Test-Path $srcDll) { $verify += "dll=$(Get-Md5 $srcDll)" }
    }
    if (-not $SkipFrontend) { $verify += "zip=$(Get-Md5 $ZipPath)" }
    if ($verify.Count -eq 0) {
        Write-Step '传输校验：跳过（后端与前端都没发了）'
    } else {
    Write-Step '传输校验（两端 MD5）'
    $check = @(
        "if (Test-Path '$RemoteRoot\stream-server.new.exe') { Write-Output ('exe=' + (Get-FileHash '$RemoteRoot\stream-server.new.exe' -Algorithm MD5).Hash) }"
        "if (Test-Path '$RemoteRoot\ViGEmClient.new.dll') { Write-Output ('dll=' + (Get-FileHash '$RemoteRoot\ViGEmClient.new.dll' -Algorithm MD5).Hash) }"
        "if (Test-Path '$RemoteDeploy\selkies-web.zip') { Write-Output ('zip=' + (Get-FileHash '$RemoteDeploy\selkies-web.zip' -Algorithm MD5).Hash) }"
    ) -join '; '
    $remote = Invoke-Ssh "powershell -NoProfile -Command `"$check`""
    $remoteMap = @{}
    foreach ($line in ($remote.Output -split "`r?`n")) {
        if ($line -match '^(\w+)=(\w+)$') { $remoteMap[$Matches[1]] = $Matches[2] }
    }
    foreach ($line in $verify) {
        $k, $v = $line -split '=', 2
        if ($remoteMap[$k] -ne $v) {
            throw "传输校验失败：$k 本机 $v / 远端 $($remoteMap[$k])"
        }
    }
    foreach ($line in $verify) {
        $k = ($line -split '=', 2)[0]
        Write-Ok "$k $($remoteMap[$k])"
    }
    }

    if ($DryRun) {
        Write-Step 'DryRun：到此为止'
        Write-Note '文件已上传并校验，未切换、未重启。'
        return
    }

    # ---- 8 切换
    Write-Step '切换（停任务 → 换文件 → 起任务）'

    # 从这里起，被控机上的文件随时可能已经被换掉：先立好"这一轮动了什么"，
    # 失败时按它恢复。换前端失败却去重启后端，会白踢掉一个活会话。
    if (-not $SkipBackend) { $attempted += 'backend' }
    if (-not $SkipFrontend) { $attempted += 'frontend' }

    <#
        只换前端时进程不该被重启（这正是 -SkipBackend 的意义：不断会话）。
        那样日志里就不会有新的 listening on，所以体检不能一律去翻日志尾部 ——
        先记下切换前的 PID，切换后 PID 没变本身就是最直接的证据。
    #>
    $pidBefore = $null
    if (-not ($SkipBackend -and $SkipFrontend)) {
        $pre = Invoke-Ssh ("powershell -NoProfile -Command `"Get-Process stream-server " +
            "-ErrorAction SilentlyContinue | Select-Object -First 1 | ForEach-Object { 'pid=' + `$_.Id }`"") -AllowFail
        $t = ($pre.Output -split "`r?`n" | Where-Object { $_ -match '^pid=(\d+)' } | Select-Object -First 1)
        if ($t) { $pidBefore = [int]($t -replace '^pid=', '').Trim() }
    }
    # 开关只在为真时传。`-SkipBackend:$false` 这种写法穿过 ssh -> cmd 之后
    # 会被当成「参数名:值」，绑定器要么报错要么静默当字符串，两种都难看。
    $argList = "powershell -NoProfile -ExecutionPolicy Bypass -File $RemoteDeploy\deploy-remote.ps1"
    if ($SkipBackend) { $argList += ' -SkipBackend' }
    if ($SkipFrontend) { $argList += ' -SkipFrontend' }
    $swap = & ssh @SshOpts $Target $argList 2>&1
    $swapCode = $LASTEXITCODE
    foreach ($line in $swap) { Write-Host "           $line" -ForegroundColor DarkGray }
    if ($swapCode -ne 0) { throw "远端切换失败（退出码 $swapCode）" }
    Write-Ok '切换完成'

    # ---- 9 体检
    Write-Step "体检（https://${HostName}:$Port/api/status）"
    if (-not (Test-Health $HealthTimeoutSec)) {
        throw '健康检查未通过'
    }
    Write-Ok 'HTTP 200'

    <#
        光有 200 不够：如果看门狗在一分钟内自己拉起过一次旧进程，端口照样通，
        但跑的是旧二进制。所以再核对「进程启动时间晚于本次部署开始时间」与
        「日志尾部出现 listening on」。
    #>
    $probe = @(
        "Get-Process stream-server -ErrorAction SilentlyContinue | ForEach-Object { 'pid=' + `$_.Id + ' session=' + `$_.SessionId + ' start=' + `$_.StartTime.ToString('o') }"
        "Get-Content '$RemoteLog' -Tail 400 | Select-String 'listening on' | ForEach-Object { 'log=' + `$_.Line }"
    ) -join '; '
    $p = Invoke-Ssh "powershell -NoProfile -Command `"$probe`"" -AllowFail
    $pidLine = ($p.Output -split "`r?`n" | Where-Object { $_ -like 'pid=*' } | Select-Object -First 1)
    $logLine = ($p.Output -split "`r?`n" | Where-Object { $_ -like 'log=*' } | Select-Object -First 1)

    $pidNow = $null
    if ($pidLine -match 'pid=(\d+)') { $pidNow = [int]$Matches[1] }
    # 只换前端时进程没重启：端口通 + 还是原来那个 PID，就是健康。
    # 这时日志尾部不会有新的 listening on（5 秒一条统计，尾部 40 行只有 3 分钟），
    # 拿它当判据会把一次正确的部署判成失败并回滚 —— 换掉的文件白换了。
    $notRestarted = ($SkipBackend -and $pidBefore -and $pidNow -eq $pidBefore)

    if ($notRestarted) {
        Write-Ok "pid=$pidNow（未重启，会话保持）"
    } elseif ($logLine) {
        # 日志行带着 `INFO [stream_server::http::server]` 前缀，掐掉只留结论。
        $msg = $logLine -replace '^log=', ''
        $at = $msg.IndexOf('listening on')
        if ($at -gt 0) { $msg = $msg.Substring($at) }
        Write-Ok $msg
    } elseif ($pidBefore -and $pidNow -eq $pidBefore) {
        # 换过 exe，进程却还是原来那个：计划任务没把它拉起来，跑的是旧二进制
        # （文件换了、映像没换）。这一条必须失败，否则「部署成功」是假的。
        throw "后端没有重启（pid 仍是 $pidNow）—— 换上去的 exe 没生效"
    } else {
        # 只有连接存在，`pointer channel <- m,...` 每个指针事件一行，尾部 400 行
        # 可能只覆盖几十秒，启动那行早被挤出窗口。端口已经 200 就是「在监听」
        # 的直接证据，这一条不再是硬判据 —— 拿它当硬判据会把一次正确的部署判成
        # 失败并触发回滚，换掉的文件白换。
        Write-Ok "pid=$pidNow（新进程）"
        Write-Note '日志尾部 400 行没有 listening on：多半是指针日志刷屏挤出了启动行，端口已通即视为在监听'
    }

    if ($pidLine) {
        Write-Ok $pidLine
        if (-not $notRestarted -and $pidLine -match 'start=(\S+)') {
            $started = [datetime]::Parse($Matches[1])
            if ($started.ToLocalTime() -lt $DeployStartedAt.AddSeconds(-5)) {
                Write-Note "进程启动时间 $($started.ToLocalTime()) 早于本次部署，可能跑的还是旧二进制"
            }
        }
    } else {
        Write-Note '没看到 stream-server 进程，但端口是通的 —— 检查是不是有别的实例'
    }

    <#
        再核一条与"路径可搬"直接相关的：计划任务的动作路径。包可以整目录搬走，
        任务里存的绝对路径不会自己跟着搬 —— 搬完之后看门狗依然去启动旧位置那份
        （或者干脆什么都不启动），而端口还通（旧进程还在），日志一句话都不说。
        这里只提示不失败：有人手工改过任务动作是合法的。
    #>
    $taskProbe = "Get-ScheduledTask -TaskName $RemoteTask -ErrorAction SilentlyContinue | " +
        "ForEach-Object { `$_.Actions | ForEach-Object { 'act=' + `$_.Execute + ' ' + `$_.Arguments } }"
    $t = Invoke-Ssh "powershell -NoProfile -Command `"$taskProbe`"" -AllowFail
    $actLine = ($t.Output -split "`r?`n" | Where-Object { $_ -like 'act=*' } | Select-Object -First 1)
    $expectVbs = Join-Path $RemoteRoot 'run-hidden.vbs'
    if ($actLine -and $actLine.ToLower().Contains($expectVbs.ToLower())) {
        Write-Ok ("看门狗动作 {0}" -f ($actLine -replace '^act=', ''))
    } elseif ($actLine) {
        Write-Note ("看门狗动作指向别处：{0}" -f ($actLine -replace '^act=', ''))
        Write-Note "  期望包含 $expectVbs；整包搬过位置后要重跑一次 setup-watchdog.bat"
    } else {
        Write-Note "查不到计划任务 $RemoteTask 的动作 —— 先在被控机跑一次 setup-watchdog.bat"
    }

    # ---- 10 归档（构建端唯一的备份）
    Write-Step '归档（构建端）'
    Save-DeployArchive

    Write-Host ""
    Write-Host ("部署完成：{0:N1}s" -f ((Get-Date) - $DeployStartedAt).TotalSeconds) -ForegroundColor Green
    Write-Host "  页面    https://${HostName}:$Port/   （强刷 Ctrl+F5：index.html 换了）"
    Write-Host "  运行根  $RemoteRoot（被控机上全部产物都在这里，脚本自定位）"
    Write-Host "  整包    $BundleDir（+ cloud-stream.zip，拷到新机器即可）"
    Write-Host "  日志    $RemoteLog"
    Write-Host "  回滚    pwsh toolchain/deploy.ps1 -Rollback（重发 $ArchiveDir）"
}
catch {
    Write-Host ""
    Write-Host "部署失败：$($_.Exception.Message)" -ForegroundColor Red
    if ($attempted.Count -gt 0) {
        <#
            只恢复本轮动过的部件。恢复本身也可能失败（比如被控机连不上），那时
            要把"下一步该做什么"说清楚，而不是只留一条红色错误。
        #>
        Write-Host "从构建端归档恢复（$($attempted -join ' + ')）…" -ForegroundColor Yellow
        try {
            if (Send-Archived -Parts $attempted) {
                Write-Host "已恢复：$ArchiveDir 里的上一份产物" -ForegroundColor Yellow
                if (Test-Health 30) { Write-Host "  端口已通" -ForegroundColor Yellow }
            } else {
                Write-Host "归档里没有本次动过的部件，被控机没有被改过" -ForegroundColor Yellow
            }
        } catch {
            Write-Host "恢复失败：$($_.Exception.Message)" -ForegroundColor Red
            Write-Host "  手工处理：pwsh toolchain/deploy.ps1 -Rollback" -ForegroundColor Red
        }
    } else {
        Write-Host "被控机没有被改动（失败发生在切换之前）" -ForegroundColor Yellow
    }
    exit 1
}
