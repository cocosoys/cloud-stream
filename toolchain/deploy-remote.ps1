<#
    deploy-remote.ps1 - the controlled-machine half of #34.

    Runs ON the target (Windows PowerShell 5.1). Never run it from the repo
    checkout: deploy.ps1 uploads it to <base>\deploy and invokes it by path.

    Why a separate file instead of an inline `ssh powershell -Command "..."`:
    an inline command has to survive bash -> ssh -> cmd -> powershell quoting.
    This session lost a whole deploy cycle to `$p=C:\dev\x` being eaten by one
    of those layers. A script file has exactly one layer.

    Everything here is ASCII on purpose: PowerShell 5.1 reads a BOM-less .ps1
    as ANSI, so non-ASCII comments would turn to mojibake.

    NO HARDCODED PATHS. -Root and -DeployDir default to positions derived from
    this script's own directory (deploy.ps1 uploads it to <base>\deploy, so the
    runtime root is <base>\stream), and everything else - web dir, launchers,
    zip, log - hangs off those two. That is what makes a bundle unpacked at
    D:\games\stream behave exactly like C:\dev\stream: the launcher starts the
    exe next to itself, serves <root>\selkies-web, and logs under <root>\logs.

    Runtime layout - identical to the build machine's target\output\cloud-stream:

        <root>\stream-server.exe  ViGEmClient.dll
        <root>\cert.pem  key.pem
        <root>\selkies-web\  logs\
        <root>\start-stream.cmd  run-hidden.vbs  setup-watchdog.bat

    Steps: install launchers -> stop task (and disable its 1-minute trigger) ->
    kill -> swap -> re-enable -> start -> wait for the process.

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
    # Do not touch the launcher scripts. Used when the operator deliberately
    # hand-edited start-stream.cmd on this box (resolution, upload root).
    [switch]$SkipLauncher,
    # Bootstrap mode: create the directories, install the launchers and stop.
    # No stop/start, no swap - this is the "first contact with a fresh machine"
    # entry point, and it must be safe to run against a live one.
    [switch]$OnlyLaunchers,

    # Defaults are resolved in the body below, not here: PowerShell 5.1 leaves
    # $PSScriptRoot empty while it evaluates parameter defaults, so
    # `[string]$DeployDir = $PSScriptRoot` binds "" and the first Join-Path dies
    # with "cannot bind argument to parameter 'Path' because it is an empty
    # string" -- before anything is touched, which is how this was caught.
    [string]$Root,
    [string]$DeployDir,
    [string]$TaskName = 'MCStreamSrv'
)

$ErrorActionPreference = 'Stop'

if (-not $DeployDir) {
    # $PSScriptRoot is populated once the body runs.
    $DeployDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
}
$DeployDir = $DeployDir.TrimEnd('\')

if (-not $Root) {
    # Split-Path on a trailing separator returns the parent of the wrong level,
    # so normalise first: <base>\deploy -> <base>\stream.
    $base = Split-Path -Parent $DeployDir
    $Root = Join-Path $base 'stream'
}
if (-not $Root) {
    # Fail() is defined below, so this one cannot call it yet.
    Write-Output 'FAIL cannot derive -Root; pass it explicitly (e.g. -Root C:\dev\stream)'
    exit 1
}
$Root = $Root.TrimEnd('\')

$WebDir = Join-Path $Root 'selkies-web'
$WebZip = Join-Path $DeployDir 'selkies-web.zip'
$LogDir = Join-Path $Root 'logs'

# #17: the virtual-gamepad client DLL. It is optional: without it the server
# runs and simply has no gamepad, so a missing upload is a note, never a failure.
$LiveExe = Join-Path $Root 'stream-server.exe'
$NewExe = Join-Path $Root 'stream-server.new.exe'
$LiveDll = Join-Path $Root 'ViGEmClient.dll'
$NewDll = Join-Path $Root 'ViGEmClient.new.dll'

# Launchers travel with every deploy: the command line (--dist / --tls-cert /
# --repeat-every) lives in start-stream.cmd, so replacing only the exe leaves the
# flags frozen at the previous release. Installed by hash comparison, because
# rewriting a .cmd that the watchdog is currently executing is a sharing
# violation on a frontend-only deploy (nothing stops the batch in that path).
$Launchers = @('start-stream.cmd', 'run-hidden.vbs', 'setup-watchdog.bat')

function Say([string]$Text) { Write-Output $Text }
function Fail([string]$Text) { Write-Output "FAIL $Text"; exit 1 }

function Get-Sha([string]$Path) { (Get-FileHash -Path $Path -Algorithm MD5).Hash }

# ------------------------------------------------------------- legacy layout
<#
    Until 2026-11 the web root and the certificates lived next to the runtime
    directory instead of inside it (<base>\selkies-web, <base>\cert.pem), because
    those paths were literals in the launcher. They are now relative, so a bundle
    unpacked anywhere is self-contained - but a machine deployed before that
    change still has the old shape. Migrate it once, in place, instead of
    forcing a hand-run migration on every box.

    The web directory is *renamed*, not copied: same volume, metadata only, and
    it guarantees the old tree cannot linger as a second copy nobody rotates.
#>
function Repair-LegacyLayout {
    $parent = Split-Path -Parent $Root
    if ($parent -and $parent -ne $Root) {
        if (-not (Test-Path $WebDir)) {
            $legacyWeb = Join-Path $parent 'selkies-web'
            if (Test-Path $legacyWeb) {
                Move-Item -LiteralPath $legacyWeb -Destination $WebDir
                Say "OK  migrated legacy web root $legacyWeb -> $WebDir"
            }
        }
        foreach ($name in @('cert.pem', 'key.pem')) {
            $legacy = Join-Path $parent $name
            $live = Join-Path $Root $name
            if ((Test-Path $legacy) -and -not (Test-Path $live)) {
                Copy-Item -LiteralPath $legacy -Destination $live -Force
                Say "OK  copied $name into the runtime root"
            }
        }
    }

    # Certificates uploaded by deploy.ps1 are only installed when the machine
    # has none: a certificate carries this host's IP in its SAN, so overwriting
    # one that was issued here with the build machine's copy would trade a
    # working HTTPS endpoint for a browser warning.
    foreach ($name in @('cert.pem', 'key.pem')) {
        $staged = Join-Path $DeployDir $name
        $live = Join-Path $Root $name
        if (-not (Test-Path $staged)) { continue }
        if (-not (Test-Path $live)) {
            Copy-Item -LiteralPath $staged -Destination $live -Force
            Say "OK  installed $name from the deploy bundle"
        }
        # The uploaded copy is transfer staging, and one of these two files is a
        # private key: leave exactly one copy on the disk. deploy.ps1 re-uploads
        # them on every run, so nothing is lost by deleting this one.
        Remove-Item -LiteralPath $staged -Force
    }
}

function Install-Launchers {
    if ($SkipLauncher) { Say 'SKIP launchers (-SkipLauncher)'; return }
    if (-not (Test-Path $LogDir)) { New-Item -ItemType Directory -Force -Path $LogDir | Out-Null }

    foreach ($name in $Launchers) {
        $src = Join-Path $DeployDir $name
        if (-not (Test-Path $src)) { Say "NOTE  $name not uploaded; keeping the one on disk"; continue }
        $dst = Join-Path $Root $name
        if ((Test-Path $dst) -and (Get-Sha $src) -eq (Get-Sha $dst)) {
            Say "OK  $name unchanged"
            continue
        }
        # cmd.exe closes start-stream.cmd a moment after the exe it launched
        # exits, so retry briefly instead of declaring the file unusable.
        $copied = $false
        $err = ''
        for ($i = 0; $i -lt 10; $i++) {
            try {
                Copy-Item -LiteralPath $src -Destination $dst -Force
                $copied = $true
                break
            } catch {
                Start-Sleep -Milliseconds 200
                $err = $_.Exception.Message
            }
        }
        if ($copied) {
            Say "OK  $name updated"
        } else {
            # On a frontend-only deploy this is expected, not an error: the file
            # is in use until the next restart.
            Say "NOTE  $name busy ($err); new copy applies at next restart"
        }
    }
}

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
    Fail "task ran but stream-server.exe never appeared (see $(Join-Path $LogDir 'watchdog.log'))"
}

# ---------------------------------------------------------------- preflight
Say 'STEP preflight'
# Create rather than require: a fresh machine gets the bundle unzipped into the
# directory and deploy.ps1 uploads into deploy\ first. Both are idempotent.
foreach ($dir in @($Root, $LogDir, $DeployDir)) {
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
}
Say "OK  root $Root"

# Launchers and certificates first: the certificates decide whether the server
# comes up with TLS, and the launchers are installed later - see the note there.
Repair-LegacyLayout

if ($OnlyLaunchers) {
    # Bootstrap path: install what we can and stop. The watchdog may be running
    # start-stream.cmd right now, so a busy file is reported, never fatal.
    Install-Launchers
    Say 'DONE launchers only'
    exit 0
}

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
    # The launchers go in HERE, not in preflight: while the watchdog runs, its
    # cmd.exe holds start-stream.cmd open and the copy is a sharing violation.
    # Stop-Server has just released it, so this is the only reliable window.
    # Order matters for a layout change too - the new exe must be started by the
    # new launcher (it is the launcher that knows --dist moved into the root).
    Install-Launchers
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

# Frontend-only deploy never stopped the process, so the watchdog still has
# start-stream.cmd open. Update what is free and say so; the rest takes effect
# at the next restart, which is exactly the semantics of this switch.
if ($SkipBackend) { Install-Launchers }

# ---------------------------------------------------------------- start
Say 'STEP start'
Start-Server
Say 'DONE swap'
exit 0
