<#
    deploy-remote.ps1 - the controlled-machine half of #34.

    Runs ON the target (Windows PowerShell 5.1). Never run it from the repo
    checkout: deploy.ps1 uploads it to C:\dev\deploy and invokes it by path.

    Why a separate file instead of an inline `ssh powershell -Command "..."`:
    an inline command has to survive bash -> ssh -> cmd -> powershell quoting.
    This session lost a whole deploy cycle to `$p=C:\dev\x` being eaten by one
    of those layers. A script file has exactly one layer.

    Everything here is ASCII on purpose: PowerShell 5.1 reads a BOM-less .ps1
    as ANSI, so non-ASCII comments would turn to mojibake.

    Steps: stop task (and disable its 1-minute trigger) -> kill -> back up ->
    swap -> re-enable -> start -> wait for the process. Any failure after the
    swap is left to the caller, which calls -Rollback.
#>
[CmdletBinding()]
param(
    [switch]$SkipBackend,
    [switch]$SkipFrontend,
    [switch]$Rollback,

    [string]$Root = 'C:\dev\stream',
    [string]$WebDir = 'C:\dev\selkies-web',
    [string]$WebZip = 'C:\dev\deploy\selkies-web.zip',
    [string]$TaskName = 'MCStreamSrv',
    [string]$LogPath = 'C:\dev\stream\logs\stream-server.log'
)

$ErrorActionPreference = 'Stop'

$LiveExe = Join-Path $Root 'stream-server.exe'
$NewExe = Join-Path $Root 'stream-server.new.exe'
$PrevExe = Join-Path $Root 'stream-server.exe.prev'
$PrevWeb = "$WebDir.prev"

function Say([string]$Text) { Write-Output $Text }
function Fail([string]$Text) { Write-Output "FAIL $Text"; exit 1 }

function Stop-Server {
    # The task fires every minute. Stop it first, then disable it: between the
    # stop and the swap a trigger would otherwise restart the exe we are about
    # to overwrite (Move-Item fails on a running image).
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if (-not $task) { Fail "scheduled task $TaskName not found; run setup-watchdog.bat once" }

    if ($task.State -eq 'Running') {
        schtasks /End /TN $TaskName | Out-Null
        Say 'OK  task ended'
    }
    Disable-ScheduledTask -TaskName $TaskName | Out-Null
    Say 'OK  task disabled'

    # /End only ends wscript.exe. stream-server.exe is a grandchild, so kill it
    # explicitly and wait for the image to be released before touching the file.
    for ($i = 0; $i -lt 20; $i++) {
        $p = Get-Process stream-server -ErrorAction SilentlyContinue
        if (-not $p) { break }
        foreach ($proc in $p) {
            Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
            Say "OK  killed pid $($proc.Id)"
        }
        Start-Sleep -Milliseconds 250
    }
    if (Get-Process stream-server -ErrorAction SilentlyContinue) { Fail 'stream-server.exe still running' }
    Say 'OK  no stream-server process'
}

function Start-Server {
    Enable-ScheduledTask -TaskName $TaskName | Out-Null
    Say 'OK  task enabled'
    schtasks /Run /TN $TaskName | Out-Null

    # The launcher goes through wscript -> cmd -> exe; give it a moment and
    # report the pid so the caller can check StartTime.
    for ($i = 0; $i -lt 60; $i++) {
        Start-Sleep -Milliseconds 500
        $p = Get-Process stream-server -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($p) {
            Say "OK  started pid $($p.Id) session $($p.SessionId)"
            return
        }
    }
    Fail 'task ran but stream-server.exe never appeared (see logs\watchdog.log)'
}

function Get-Sha([string]$Path) { (Get-FileHash -Path $Path -Algorithm MD5).Hash }

# ---------------------------------------------------------------- rollback
if ($Rollback) {
    Say 'STEP rollback'
    Stop-Server
    if (Test-Path $PrevExe) {
        Copy-Item $PrevExe $LiveExe -Force
        Say "OK  exe restored $((Get-Item $LiveExe).Length) B"
    } else { Say 'WARN no stream-server.exe.prev' }
    if (Test-Path $PrevWeb) {
        if (Test-Path $WebDir) { Remove-Item $WebDir -Recurse -Force }
        Rename-Item $PrevWeb $WebDir
        Say 'OK  web dir restored'
    } else { Say 'WARN no selkies-web.prev' }
    Start-Server
    Say 'DONE rollback'
    exit 0
}

# ---------------------------------------------------------------- preflight
Say 'STEP preflight'
if (-not (Test-Path $Root)) { Fail "missing $Root" }
if (-not $SkipBackend -and -not (Test-Path $NewExe)) { Fail "missing $NewExe (upload did not land)" }
if (-not $SkipFrontend -and -not (Test-Path $WebZip)) { Fail "missing $WebZip" }
if (-not $SkipBackend) {
    $len = (Get-Item $NewExe).Length
    # A truncated upload is the classic silent failure: the file exists, the
    # hash matched nothing because nobody compared, and the exe will not start.
    if ($len -lt 5MB) { Fail "new exe is only $len bytes" }
    Say "OK  new exe $len B md5 $(Get-Sha $NewExe)"
}
if (-not $SkipFrontend) {
    $zipLen = (Get-Item $WebZip).Length
    if ($zipLen -lt 100KB) { Fail "zip is only $zipLen bytes" }
    Say "OK  zip $zipLen B md5 $(Get-Sha $WebZip)"
}

# ---------------------------------------------------------------- swap backend
if (-not $SkipBackend) {
    Say 'STEP backend'
    Stop-Server
    # Copy, not Move: if the process fails to start we still have a live image
    # on disk, and .prev doubles as the rollback source.
    Copy-Item $LiveExe $PrevExe -Force
    Say 'OK  backed up to stream-server.exe.prev'
    Move-Item $NewExe $LiveExe -Force
    Say "OK  swapped in $((Get-Item $LiveExe).Length) B md5 $(Get-Sha $LiveExe)"
}

# ---------------------------------------------------------------- swap frontend
if (-not $SkipFrontend) {
    Say 'STEP frontend'
    if (Test-Path $PrevWeb) { Remove-Item $PrevWeb -Recurse -Force }
    if (Test-Path $WebDir) { Rename-Item $WebDir $PrevWeb }
    New-Item -ItemType Directory -Path $WebDir | Out-Null
    Expand-Archive -Path $WebZip -DestinationPath $WebDir -Force

    # The archive must have been built from inside dist/. If it carries a
    # top-level folder, index.html ends up one level down and the server's
    # root serves a directory listing / 404 -- silently, because the files
    # are all there.
    $index = Join-Path $WebDir 'index.html'
    if (-not (Test-Path $index)) { Fail 'index.html not at web root: zip has a top-level folder' }
    $assets = Get-ChildItem (Join-Path $WebDir 'assets') -Filter '*.js' -ErrorAction SilentlyContinue
    if (-not $assets) { Fail 'no js under assets\: wrong build (core instead of dashboard?)' }
    Say "OK  web dir $((Get-Item $index).Length) B index, $($assets.Count) js chunks"
}

# ---------------------------------------------------------------- start
Say 'STEP start'
Start-Server
Say 'DONE swap'
exit 0
