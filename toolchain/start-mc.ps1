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
# 全屏：加 -Fullscreen（回到窗口用 -Windowed）。它在启动前把 options.txt 的
#   fullscreen 改成 true/false —— 1.12.2 没有 --fullscreen 启动参数。已经在跑
#   的实例会挡住这次修改（它退出时会把内存里的旧值写回去），脚本遇到就 exit 3。
#
#   **这是排障用的手工入口，不是正式路径。** 正式路径是 stream-server 托管
#   （docs/game-hosting.md），全屏走游戏内的 F11 —— 实测 F11 的真全屏能被 DXGI
#   桌面复制采到，所以不需要改窗口样式的无边框方案。全屏真正的敌人是**失焦**：
#   独占全屏的 MC 一失去焦点就自己最小化到 -32000,-32000，而重试 SetWindowPos
#   反复恢复会打崩 GL 上下文（No OpenGL context found in the current thread）。
#   这件事由 kiosk（docs/kiosk.md）负责，不是这个脚本能解决的。
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
    [switch]$Delivered,
    [switch]$Fullscreen,
    [switch]$Windowed
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
        # 投递是"把这条指令重新念一遍给另一个会话听"，所以开关必须跟着走。漏掉
        # 一个就会出现"命令里写了 -Fullscreen、桌上还是小窗"这种最难查的差别 ——
        # 两条路径看起来一样，只是参数在转手时掉了。
        $act = 'powershell -NoProfile -ExecutionPolicy Bypass -File "' + $self + '" -Delivered'
        if ($Fullscreen) { $act += ' -Fullscreen' }
        if ($Windowed)   { $act += ' -Windowed' }
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

# ---- 全屏：只能改 options.txt --------------------------------------------
# 1.12.2 的 Main 没有 --fullscreen 这个参数（那是 1.13 之后才有的），窗口模式由
# GameSettings 从 options.txt 的 `fullscreen:true` 读出来。所以只能改文件。
#
# 两个前提条件，缺一个这次改就白改：
#   * 游戏不能正在跑 —— MC 只在启动时读 options.txt，退出时把**内存里的状态**
#     整份写回去。跑着的时候改，退出那一刻就被旧值盖掉，看起来像"改了没用"。
#   * 得在**投递之后**改（也就是这个文件在被 -Deliver 拉起的那个进程里执行），
#     因为真正开窗口的是它；在这里改也一样，因为启动动作就在下面几十行。
if ($Fullscreen -or $Windowed) {
    $want = if ($Fullscreen) { 'true' } else { 'false' }
    # 用命令行匹配而不是映像名：同一台机器上可能有别人的 javaw（比如另一个版本），
    # 或者上一次没退干净的实例。只有命令行里带着这个 .minecraft 的才算。
    $alive = Get-CimInstance Win32_Process -Filter "Name='javaw.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -and $_.CommandLine.Contains($McDir) }
    if ($alive) {
        $ids = ($alive | ForEach-Object { $_.ProcessId }) -join ', '
        Say ('fullscreen: a game under this .minecraft is already running (pid ' + $ids + ')')
        Say 'fullscreen: stop it first -- it rewrites options.txt on exit and would undo this'
        exit 3
    }

    $opt = Join-Path $McDir 'options.txt'
    if (Test-Path $opt) {
        # 按行改，保留顺序与其余设置（options.txt 有 100 多行，重排等于清掉用户的
        # 键位、音量、资源包）。用 ReadAllLines 而不是 Get-Content：后者会给每行
        # 套上 PSObject，写回去时字符串比较与拼接都容易出岔子。
        $lines = [System.IO.File]::ReadAllLines($opt)
        $hit = $false
        for ($i = 0; $i -lt $lines.Count; $i++) {
            if ($lines[$i] -match '^fullscreen:') { $lines[$i] = 'fullscreen:' + $want; $hit = $true }
        }
        if (-not $hit) {
            $lines = @($lines) + @('fullscreen:' + $want)
        }
        [System.IO.File]::WriteAllLines($opt, $lines, (New-Object System.Text.UTF8Encoding $false))
        Say ('fullscreen=' + $want + ' -> ' + $opt + ' (' + $lines.Count + ' lines, ' +
             $(if ($hit) { 'replaced' } else { 'appended' }) + ')')
    } else {
        # 没有 options.txt 说明这台机器上的 MC 一次都没正常退出过。不替它造一份：
        # 缺文件的 MC 会用全套默认值，凭空写一行进去反而是最坏的那种"帮倒忙"。
        Say ('fullscreen: no options.txt at ' + $opt + '; nothing changed')
        Say 'fullscreen: run the game once so it writes one, then retry'
        exit 4
    }
}

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
