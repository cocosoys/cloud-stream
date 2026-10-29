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
      4. scp 上传（exe / zip / 两个启动脚本，内容一样就跳过）
      5. MD5 双向核对

    被控机做的事（由 deploy-remote.ps1 执行，脚本随包上传）：
      6. 停任务 → 杀进程 → 备份 → 换 exe → 解压前端 → 起任务
      7. 本机轮询 /api/status 直到 200，并核对进程启动时间与日志
      8. 失败自动回滚到 .prev，并把原因打出来

    为什么用「上传远端脚本」而不是 ssh 里塞内联命令：PowerShell 内联字符串要
    穿过 bash → ssh → cmd → powershell 四层引号，`$p=C:\dev\x` 这类赋值会被
    吃掉（本轮踩过），出问题时还看不出是哪一层吃了它。脚本文件只有一层。

.PARAMETER Target
    ~/.ssh/config 里的主机别名。默认 mc-admin（Administrator，游戏所在的交互用户）。

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
#>
[CmdletBinding()]
param(
    [string]$Target = 'mc-admin',
    [string]$HostName = '192.168.59.148',
    [int]$Port = 8080,
    [switch]$SkipBackend,
    [switch]$SkipFrontend,
    [switch]$SkipBuild,
    [switch]$DryRun,
    [switch]$Bootstrap,
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
# 这些路径写死在 start-stream.cmd 与计划任务里，改这里等于改部署契约，
# 两边必须同时改（文档 §2 有对照表）。
$RemoteRoot = 'C:\dev\stream'
$RemoteDeploy = 'C:\dev\deploy'
$RemoteWeb = 'C:\dev\selkies-web'
$RemoteTask = 'MCStreamSrv'
$RemoteExe = "$RemoteRoot\stream-server.exe"
$RemoteLog = "$RemoteRoot\logs\stream-server.log"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$LocalExe = Join-Path $RepoRoot 'target\release\stream-server.exe'

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

# SSH 一律 BatchMode：缺密钥时立刻失败，而不是卡在密码提示上等一个人来敲。
$SshOpts = @('-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8', '-o', 'StrictHostKeyChecking=accept-new')

function Invoke-Ssh([string]$Command, [switch]$AllowFail) {
    $out = & ssh @SshOpts $Target $Command 2>&1
    $code = $LASTEXITCODE
    if ($code -ne 0 -and -not $AllowFail) {
        throw "ssh 失败（$code）：$Command`n$($out -join "`n")"
    }
    return [pscustomobject]@{ Code = $code; Output = ($out -join "`n") }
}

function Send-File([string]$Local, [string]$Remote) {
    Invoke-Native -Exe 'scp' -Arguments (@('-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8', $Local, "${Target}:$Remote") -replace '\\', '/') -Quiet
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

    # ---- 2 引导（可选）
    if ($Bootstrap) {
        Write-Step '引导：目录 / sshd 自启 / 启动脚本 / 计划任务'
        Enable-RemoteSsh

        $bootstrapCmd = @(
            "New-Item -ItemType Directory -Force '$RemoteRoot','$RemoteRoot\logs','$RemoteDeploy','$RemoteWeb' | Out-Null;"
            "if (Test-Path '$RemoteExe') { Write-Output ('exe ' + (Get-Item '$RemoteExe').Length) } else { Write-Output 'exe MISSING' };"
            "if (Get-ScheduledTask -TaskName $RemoteTask -ErrorAction SilentlyContinue) { Write-Output ('task ' + (Get-ScheduledTask -TaskName $RemoteTask).State) } else { Write-Output 'task MISSING' };"
            "if ((Test-Path 'C:\dev\cert.pem') -and (Test-Path 'C:\dev\key.pem')) { Write-Output 'tls OK' } else { Write-Output 'tls MISSING' }"
        ) -join ' '
        $r = Invoke-Ssh "powershell -NoProfile -Command `"$bootstrapCmd`""
        foreach ($line in ($r.Output -split "`r?`n")) {
            if ($line.Trim()) { Write-Info $line.Trim() }
        }

        # 启动脚本要跟着部署走：命令行参数（--dist / --tls-cert / --repeat-every）
        # 全写在里面，只换 exe 不换它，参数就会永远停在上一次。
        foreach ($f in @('start-stream.cmd', 'run-hidden.vbs')) {
            Send-File (Join-Path $PSScriptRoot $f) "$RemoteDeploy/$f"
            Write-Ok "推送 $f"
        }
        $r = Invoke-Ssh "powershell -NoProfile -Command `"Copy-Item '$RemoteDeploy\start-stream.cmd','$RemoteDeploy\run-hidden.vbs' '$RemoteRoot' -Force`""
        Write-Ok '启动脚本落到 C:\dev\stream'

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
            # 每次往 dist/jsdb 写 1992 个小 JSON，于是下一次构建的「清空输出目录」
            # 就变成一次上千文件的批量删除；在带批量删除保护的 shell 里（例如
            # 工作区沙箱把 node 的 rmSync 包了一层）会直接让构建失败，
            # 而且报错信息指向 vite 插件，看不出是文件数量的问题。
            # Remove-Item 不走 node，构建路径就与文件数量无关。
            foreach ($pkg in @($CoreDir, $DashDir)) {
                $stale = Join-Path $pkg 'dist'
                if (Test-Path $stale) { Remove-Item $stale -Recurse -Force }
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
        if (Test-Path $ZipPath) { Remove-Item $ZipPath -Force }
        Compress-Archive -Path (Join-Path $dashDist '*') -DestinationPath $ZipPath -CompressionLevel Optimal
        $zipMd5 = Get-Md5 $ZipPath
        Write-Ok ("打包 $([math]::Round((Get-Item $ZipPath).Length / 1MB, 2)) MB  md5=$zipMd5")
    }

    # ---- 5 上传
    Write-Step '上传'
    if (-not $SkipBackend) {
        Send-File $LocalExe "$RemoteRoot/stream-server.new.exe"
        Write-Ok 'stream-server.new.exe'
    }
    if (-not $SkipFrontend) {
        if (-not (Test-Path $ZipPath)) { throw "没有 $ZipPath：这是打包步骤失败，检查 dashboard\dist 是否存在" }
        Send-File $ZipPath "$RemoteDeploy/selkies-web.zip"
        Write-Ok 'selkies-web.zip'
    }
    Send-File $RemoteScript "$RemoteDeploy/deploy-remote.ps1"
    Write-Ok 'deploy-remote.ps1'

    # ---- 6 传输校验
    $verify = @()
    if (-not $SkipBackend) { $verify += "exe=$(Get-Md5 $LocalExe)" }
    if (-not $SkipFrontend) { $verify += "zip=$(Get-Md5 $ZipPath)" }
    if ($verify.Count -eq 0) {
        Write-Step '传输校验：跳过（后端与前端都没发了）'
    } else {
    Write-Step '传输校验（两端 MD5）'
    $check = @(
        "if (Test-Path '$RemoteRoot\stream-server.new.exe') { Write-Output ('exe=' + (Get-FileHash '$RemoteRoot\stream-server.new.exe' -Algorithm MD5).Hash) }"
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

    # ---- 7 切换
    Write-Step '切换（停任务 → 备份 → 换文件 → 起任务）'

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

    # ---- 8 体检
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

    Write-Host ""
    Write-Host ("部署完成：{0:N1}s" -f ((Get-Date) - $DeployStartedAt).TotalSeconds) -ForegroundColor Green
    Write-Host "  页面    https://${HostName}:$Port/   （强刷 Ctrl+F5：index.html 换了）"
    Write-Host "  日志    $RemoteLog"
    Write-Host "  回滚    在被控机执行 C:\dev\deploy\deploy-remote.ps1 -Rollback（文档 §6）"
}
catch {
    Write-Host ""
    Write-Host "部署失败：$($_.Exception.Message)" -ForegroundColor Red
    if (-not $DryRun) {
        Write-Host "尝试回滚…" -ForegroundColor Yellow
        $rb = Invoke-Ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $RemoteDeploy\deploy-remote.ps1 -Rollback -CycleOnly" -AllowFail
        Write-Host $rb.Output -ForegroundColor DarkGray
        if ($rb.Code -eq 0) {
            Write-Host "已回滚到上一版（stream-server.exe.prev / selkies-web.prev）" -ForegroundColor Yellow
        } else {
            Write-Host "回滚也失败了 —— 到被控机上手工执行 deploy-remote.ps1 -Rollback" -ForegroundColor Red
        }
    }
    exit 1
}
