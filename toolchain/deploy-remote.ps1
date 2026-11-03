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

    Steps: stop task (and disable its 1-minute trigger) -> kill -> swap ->
    re-enable -> start -> wait for the process.

    NO BACKUPS LIVE HERE. The controlled machine carries only the artifact
    that is supposed to run: one exe, one DLL, one web directory. Rollback
    material is kept on the build machine (target\deploy-archive), because that
    is where the person doing the rollback sits. The old scheme left
    stream-server.exe.prev (18.6 MB) and selkies-web.prev (2000+ files) on every
    machine, and they accumulated faster than anyone rotated them.

    The frontend is expanded into a staging directory and swapped by rename, so
    a truncated zip fails while the live directory is still untouched.
#>
[CmdletBinding()]
param(
    [switch]$SkipBackend,
    [switch]$SkipFrontend,

    [string]$Root = 'C:\dev\stream',
    [string]$WebDir = 'C:\dev\selkies-web',
    [string]$WebZip = 'C:\dev\deploy\selkies-web.zip',
    [string]$TaskName = 'MCStreamSrv',
    [string]$LogPath = 'C:\dev\stream\logs\stream-server.log'
)

$ErrorActionPreference = 'Stop'

$LiveExe = Join-Path $Root 'stream-server.exe'
$NewExe = Join-Path $Root 'stream-server.new.exe'
# #17: the virtual-gamepad client DLL. It is optional: without it the server
# runs and simply has no gamepad, so a missing upload is a note, never a failure.
$LiveDll = Join-Path $Root 'ViGEmClient.dll'
$NewDll = Join-Path $Root 'ViGEmClient.new.dll'

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
    # The DLL is swapped after the process stops, for the same reason the exe is:
    # a loaded DLL is held open and cannot be overwritten in place.
    if (Test-Path $NewDll) { Say "OK  new dll $((Get-Item $NewDll).Length) B md5 $(Get-Sha $NewDll)" }
    else { Say 'WARN no ViGEmClient.new.dll: gamepad stays unavailable' }
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
    # Overwrite in place. No .prev: the build machine holds the rollback copy,
    # and a copy here is the thing nobody ever rotates.
    Move-Item $NewExe $LiveExe -Force
    Say "OK  swapped in $((Get-Item $LiveExe).Length) B md5 $(Get-Sha $LiveExe)"
    # Same window as the exe: Stop-Server has already released the image.
    if (Test-Path $NewDll) {
        Copy-Item $NewDll $LiveDll -Force
        Remove-Item $NewDll -Force
        Say "OK  dll in place $((Get-Item $LiveDll).Length) B md5 $(Get-Sha $LiveDll)"
    }
}

# ---------------------------------------------------------------- swap frontend
if (-not $SkipFrontend) {
    Say 'STEP frontend'
    # Expand into staging and validate *there*. The live directory is only
    # touched once a complete tree is on disk, so a short or wrong-shaped zip
    # leaves the running site exactly as it was.
    $Stage = "$WebDir.staging"
    if (Test-Path $Stage) { Remove-Item $Stage -Recurse -Force }
    New-Item -ItemType Directory -Path $Stage | Out-Null
    Expand-Archive -Path $WebZip -DestinationPath $Stage -Force

    # The archive must have been built from inside dist/. If it carries a
    # top-level folder, index.html ends up one level down and the server's
    # root serves a directory listing / 404 -- silently, because the files
    # are all there.
    $index = Join-Path $Stage 'index.html'
    if (-not (Test-Path $index)) {
        Remove-Item $Stage -Recurse -Force
        Fail 'index.html not at web root: zip has a top-level folder (live site untouched)'
    }
    $assets = Get-ChildItem (Join-Path $Stage 'assets') -Filter '*.js' -ErrorAction SilentlyContinue
    if (-not $assets) {
        Remove-Item $Stage -Recurse -Force
        Fail 'no js under assets\: wrong build, core instead of dashboard (live site untouched)'
    }
    $indexLen = (Get-Item $index).Length

    # Rename twice instead of delete-then-create: both are metadata operations,
    # so the window where the web root does not exist is microseconds.
    $Old = "$WebDir.old"
    if (Test-Path $Old) { Remove-Item $Old -Recurse -Force }
    if (Test-Path $WebDir) { Rename-Item $WebDir $Old }
    Rename-Item $Stage $WebDir
    Say "OK  web dir $indexLen B index, $($assets.Count) js chunks"
    if (Test-Path $Old) { Remove-Item $Old -Recurse -Force }
    # The zip is transfer staging, not an artifact. Leaving it behind is exactly
    # the pile-up this script exists to avoid.
    Remove-Item $WebZip -Force
    Say 'OK  staging zip removed'
}

# ---------------------------------------------------------------- start
Say 'STEP start'
Start-Server
Say 'DONE swap'
exit 0
