# MC 1.12.2-Forge 14.23.5.2864 命令行直启（免启动器）
#
# 定位（2026-11-13 更新）：**不属于 cloud-stream 的构建或安装链**。它是游戏主机上
# 用来把 MC 拉起来的那条指令 —— 云游戏链路的第一环（先有游戏进程，才有画面可串）。
# 手动排障、以及将来 Rust 侧接手之前，都靠它。
#
# 两台机器的差异全部收进参数默认值，脚本本身不分叉：
#   .148（CLIENT001）: jdk8 在 C:\dev\jdk8\jdk8u504-b01，mc 在 Administrator 桌面
#   .150（DESKTOP-0L2MMH3）: jre8 在 C:\mc\jre8，mc 在 Admin 桌面
# 探测顺序见 Resolve-Java / Resolve-Mc，找不到就报错退出，不猜。
#
# 从 ssh 调用时要注意的事（这条最容易白查一小时）：
#   sshd 落在 **session 0**，在那儿起来的 java 窗口不属于任何人看得见的桌面 ——
#   进程活着、日志一路写到主菜单、CPU 也在跑，就是屏幕上没有。加 -Deliver 让脚本
#   用 schtasks /IT 把自己重新投进 console 会话，再在那边启动。
#
#        ssh mc-150 'powershell -NoProfile -ExecutionPolicy Bypass -File C:\mc\start-mc.ps1 -Deliver'
#
# 历史坑（前三条在 .148 上踩的，第四条在 .150 上踩的，别再踩一遍）：
#   1. classpath 必须从 version json 的 downloads.artifact.path 拼，不能按 name
#      猜；artifact.path 是相对 **libraries 根** 的。首版少 55 个 jar 直接失败。
#   2. Start-Process -ArgumentList 传数组会把含空格路径拆断 → 手拼单字符串 + 引号。
#   3. 不要 -RedirectStandardOutput：会和脚本自己的 Out-File 争日志句柄，也会让
#      启动它的 PowerShell 一直挂着等管道（桌面上留一个黑窗口）。MC 自己写
#      logs\latest.log，够排障。
#   4. **不要 -WindowStyle Hidden**：SW_HIDE 会被进程创建的第一个窗口继承，而 MC
#      只有一个 LWJGL 窗口 —— 结果是游戏在跑、窗口不可见（vis=False、1x1）。用
#      javaw.exe（本身无控制台）就够了，别再加 Hidden。
param(
    [string]$McDir,
    [string]$JavaHome,
    [string]$Version = '1.12.2-Forge_14.23.5.2864',
    [string]$Player  = 'CloudPlayer',
    [string]$HeapMB  = '2048',
    [string]$TaskName = 'MCGame',
    [switch]$Deliver,
    [switch]$Delivered
)

$ErrorActionPreference = 'Stop'

# ---- 日志：走 %TEMP%，不落在 .minecraft 里（那是用户数据） ----------------
$log = Join-Path $env:TEMP 'mc-launch.log'
function Say([string]$msg) {
    $line = ('[{0}] {1}' -f (Get-Date -Format 'HH:mm:ss'), $msg)
    Add-Content -Path $log -Value $line -Encoding UTF8
    Write-Output $line
}

# ---- console 会话的 id：0 表示"没人登在控制台上" --------------------------
function Get-ConsoleSessionId {
    foreach ($line in (& query.exe session 2>$null)) {
        if ($line -match '^\s*console\s+\S+\s+(\d+)\s') { return [int]$Matches[1] }
    }
    return -1
}

# ---- 把自己投进交互会话 ---------------------------------------------------
# 判据是"当前会话 != console 会话"。投递用的任务名与 .148 上的老任务同名（MCGame），
# 这样两条路径落到同一个东西上，不会出现两个名字各有各的状态。
if ($Deliver -and -not $Delivered) {
    $mine = (Get-Process -Id $PID).SessionId
    $console = Get-ConsoleSessionId
    Say ("deliver: my session=$mine console session=$console")
    if ($console -lt 0) {
        Say 'deliver: nobody is logged on at the console; nothing to deliver into'
        exit 2
    }
    if ($mine -eq $console) {
        Say 'deliver: already in the console session, starting here'
    } else {
        $self = $PSCommandPath
        $act = 'powershell -NoProfile -ExecutionPolicy Bypass -File "' + $self + '" -Delivered'
        if ($act.Length -gt 261) {
            Say ('deliver: command line is ' + $act.Length + ' chars, schtasks caps at 261')
            exit 2
        }
        & schtasks.exe '/Create' '/TN' $TaskName '/TR' $act '/SC' 'ONCE' '/ST' '23:59' '/IT' '/F' 2>&1 |
            ForEach-Object { Say ('  ' + $_) }
        if ($LASTEXITCODE -ne 0) { Say ('deliver: schtasks /Create exit=' + $LASTEXITCODE); exit 1 }
        & schtasks.exe '/Run' '/TN' $TaskName 2>&1 | ForEach-Object { Say ('  ' + $_) }
        if ($LASTEXITCODE -ne 0) { Say ('deliver: schtasks /Run exit=' + $LASTEXITCODE); exit 1 }
        Say ('deliver: handed off to task ' + $TaskName + ' in session ' + $console)
        exit 0
    }
}

# ---- .minecraft -----------------------------------------------------------
function Resolve-Mc {
    param([string]$Explicit)
    $cands = New-Object System.Collections.Generic.List[string]
    if ($Explicit) { $cands.Add($Explicit) }
    $cands.Add((Join-Path $env:USERPROFILE 'Desktop\.minecraft'))
    $cands.Add('C:\Users\Administrator\Desktop\.minecraft')
    $cands.Add('C:\Users\Admin\Desktop\.minecraft')
    foreach ($c in $cands) {
        if (Test-Path (Join-Path $c 'versions')) { return $c }
    }
    throw ('no .minecraft found; tried: ' + ($cands -join ', '))
}

# ---- java：先看显式参数，再看两台机器的固定位置，最后看 PATH ---------------
# 1.12.2 只能是 Java 8；抓到 java 17 之类的会在一堆 ClassNotFound 里淹死，
# 所以这里把版本也读出来，不是 1.8 就在日志里说一句。
function Resolve-Java {
    param([string]$Explicit)
    $cands = New-Object System.Collections.Generic.List[string]
    if ($Explicit) { $cands.Add((Join-Path $Explicit 'bin\javaw.exe')) }
    $cands.Add('C:\mc\jre8\bin\javaw.exe')
    Get-ChildItem 'C:\dev\jdk8' -Directory -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending |
        ForEach-Object { $cands.Add((Join-Path $_.FullName 'bin\javaw.exe')) }
    foreach ($c in $cands) {
        if (Test-Path $c) { return $c }
    }
    $onPath = Get-Command javaw.exe -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }
    throw ('no javaw.exe found; tried: ' + ($cands -join ', '))
}

$McDir = Resolve-Mc $McDir
$java  = Resolve-Java $JavaHome
Say ('mc       = ' + $McDir)
Say ('javaw    = ' + $java)
Say ('version  = ' + $Version)
Say ('heap     = ' + $HeapMB + 'M')
Say ('session  = ' + (Get-Process -Id $PID).SessionId)

# ---- classpath：json 的 downloads.artifact.path 优先 ----------------------
$vdir  = Join-Path $McDir ('versions\' + $Version)
$vjson = Join-Path $vdir ($Version + '.json')
if (-not (Test-Path $vjson)) { throw ('no version json: ' + $vjson) }
$json = Get-Content -Raw $vjson | ConvertFrom-Json

$libRoot = Join-Path $McDir 'libraries'
$cp = New-Object System.Collections.Generic.List[string]
$missing = New-Object System.Collections.Generic.List[string]

foreach ($lib in $json.libraries) {
    # rules：没有 rules 就是进；有 rules 时最后一条匹配的说了算，没匹配就是不进。
    # 1.12.2 只用得上 os.name / os.arch。
    $keep = $true
    if ($lib.rules) {
        $keep = $false
        foreach ($r in $lib.rules) {
            $hit = $true
            if ($r.os) {
                if ($r.os.name -and $r.os.name -ne 'windows') { $hit = $false }
                if ($r.os.arch -and $r.os.arch -ne 'x86_64') { $hit = $false }
            }
            if ($hit) { $keep = ($r.action -eq 'allow') }
        }
    }
    if (-not $keep) { continue }

    $path = $null
    if ($lib.downloads -and $lib.downloads.artifact -and $lib.downloads.artifact.path) {
        $path = $lib.downloads.artifact.path
    } elseif ($lib.name -match '^([^:]+):([^:]+):([^:]+)$') {
        $path = ($Matches[1] -replace '\.', '\') + '\' + $Matches[2] + '\' + $Matches[3] + '\' + $Matches[2] + '-' + $Matches[3] + '.jar'
    }
    if ($path) {
        $full = Join-Path $libRoot $path
        if (Test-Path $full) { $cp.Add($full) } else { $missing.Add($path) }
    }
}

$clientJar = Join-Path $vdir ($Version + '.jar')
if (Test-Path $clientJar) { $cp.Add($clientJar) } else { $missing.Add($Version + '.jar') }

Say ('classpath=' + $cp.Count + ' items, missing=' + $missing.Count)
foreach ($m in $missing) { Say ('  MISSING ' + $m) }

# natives 不进球；它们解压在 natives-tmp 里，靠 -Djava.library.path 找到
$natives = Join-Path $McDir 'natives-tmp'
if (-not (Test-Path $natives)) {
    Say ('natives-tmp missing, falling back to the version dir')
    $natives = $vdir
}
$assets = Join-Path $McDir 'assets'

# ---- 参数拼成一个字符串 ---------------------------------------------------
# 数组会被 Start-Process 按分隔符重排，含空格的路径（比如 "C:\Program Files\..."）
# 就此断成两半。全部加引号，整串交过去。
$classpath = ($cp -join ';')
$argStr = '-Xmx' + $HeapMB + 'M -Xms512M -XX:+UseG1GC ' +
          '"-Djava.library.path=' + $natives + '" ' +
          '-Dfml.ignoreInvalidMinecraftCertificates=true ' +
          '-Dfml.ignorePatchDiscrepancies=true ' +
          '-cp "' + $classpath + '" ' +
          $json.mainClass + ' ' +
          '--username ' + $Player +
          ' --version ' + $Version +
          ' --gameDir "' + $McDir + '"' +
          ' --assetsDir "' + $assets + '"' +
          ' --assetIndex ' + $json.assetIndex.id +
          ' --uuid 00000000-0000-0000-0000-000000000000' +
          ' --accessToken 0 --userType legacy --versionType ' + $json.type +
          ' --tweakClass net.minecraftforge.fml.common.launcher.FMLTweaker'

Say ('cmdline=' + $argStr.Length + ' chars')

# javaw，不是 java：游戏窗口是桌面上唯一该出现的东西。也不加 WindowStyle。
$p = Start-Process -FilePath $java -ArgumentList $argStr -WorkingDirectory $McDir `
     -WindowStyle Normal -PassThru

Say ('pid=' + $p.Id + ' session=' + $p.SessionId)
Say ('log: ' + (Join-Path $McDir 'logs\latest.log'))
