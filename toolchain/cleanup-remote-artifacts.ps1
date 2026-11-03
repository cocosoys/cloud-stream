# One-shot: strip the build-side residue that accumulated on the controlled
# machine, leaving only the artifacts that are supposed to run there.
#
# Why a script instead of `ssh mc-admin 'del "C:\dev\deploy\*.ps1"'`: the command
# has to survive bash -> ssh -> cmd, and the wildcard does not (MSYS rewrites the
# path and nothing gets deleted, silently). Every path below is explicit.
#
# Nothing here is a rollback copy: since the deploy chain stopped keeping .prev on
# the target, backups live in the build machine's target\deploy-archive. The only
# file this script removes that could matter is C:\dev\stream\stream-server.exe.prev,
# and it matters precisely because it is a stale duplicate of an older build.
#
# Deliberately NOT touched:
#   <root>\stream				runtime root since the bundle refactor: exe, dll,
#							cert.pem, key.pem, selkies-web\, logs\, 3 launchers
#   cert.pem / key.pem			--tls-cert / --tls-key for start-stream.cmd
#   mc-game-task.xml				scheduled-task definitions for the Minecraft
#   mc-launch.cmd				service that shares this machine
#   mc-stream-task.xml
#   jdk8\  mc\start-mc.ps1  mc\launch-remote.ps1		the game's own runtime
#
# Legacy shape (before the bundle refactor) put the served web root and the
# certificates directly under the base directory. deploy-remote.ps1 moves those
# into the runtime root on its next run; a stray copy left here is safe to drop
# once `selkies-web\` exists under the root.
#
# After this runs, C:\dev\deploy still exists but is empty: deploy.ps1 uploads
# deploy-remote.ps1, the three launcher scripts and selkies-web.zip into it on
# every deploy, and the remote half deletes the zip once it has been extracted.
#
# Re-runnable: missing paths are skipped.

param(
    [string]$Root = 'C:\dev',
    [switch]$WhatIf
)

$script:removed = 0
$script:bytes = 0
$script:missing = 0
$script:failed = 0

function Remove-One([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { $script:missing++; return }
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.PSIsContainer) { return }

    $size = $item.Length
    if ($WhatIf) {
        Write-Output ("WOULD DELETE {0} ({1:N1} KB)" -f $item.FullName, ($size / 1KB))
        $script:bytes += $size
        $script:removed++
        return
    }
    try {
        # Extracted files can carry the read-only attribute; Clear first so the
        # delete does not fail halfway through a directory.
        if ($item.IsReadOnly) { $item.IsReadOnly = $false }
        Remove-Item -LiteralPath $Path -Force -ErrorAction Stop
        $script:bytes += $size
        $script:removed++
        Write-Output ("deleted {0}" -f $item.FullName)
    } catch {
        $script:failed++
        Write-Output ("FAILED  {0}: {1}" -f $item.FullName, $_.Exception.Message)
    }
}

function Remove-Tree([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { $script:missing++; return }
    $item = Get-Item -LiteralPath $Path -Force
    if (-not $item.PSIsContainer) { Remove-One $Path; return }

    $sum = Get-ChildItem -LiteralPath $Path -Recurse -File -ErrorAction SilentlyContinue |
        Measure-Object Length -Sum
    $count = if ($sum.Count) { $sum.Count } else { 0 }
    $size = if ($sum.Sum) { $sum.Sum } else { 0 }
    if ($WhatIf) {
        Write-Output ("WOULD DELETE {0}\ ({1} files, {2:N1} MB)" -f $item.FullName, $count, ($size / 1MB))
        $script:removed += $count
        $script:bytes += $size
        return
    }
    try {
        # A staged dist can be 2000 files with some read-only entries; clearing
        # the attribute up front is cheaper than failing and restarting.
        Get-ChildItem -LiteralPath $Path -Recurse -File -Force -ErrorAction SilentlyContinue |
            ForEach-Object { if ($_.IsReadOnly) { $_.IsReadOnly = $false } }
        Remove-Item -LiteralPath $Path -Recurse -Force -ErrorAction Stop
        $script:removed += $count
        $script:bytes += $size
        Write-Output ("deleted {0}\ ({1} files)" -f $item.FullName, $count)
    } catch {
        $script:failed++
        Write-Output ("FAILED  {0}\: {1}" -f $item.FullName, $_.Exception.Message)
    }
}

# ---- 1. stream\: the old rollback copy and any interrupted upload
Remove-One "$Root\stream\stream-server.exe.prev"
Remove-One "$Root\stream\stream-server.new.exe"
Remove-One "$Root\stream\ViGEmClient.new.dll"

# ---- 2. deploy\: staging for the deploy chain, plus ~28 build-side scripts that
#         were pushed here over the sessions (probes, log pullers, diag runners).
#         The directory itself stays: scp needs it to exist.
$deploy = "$Root\deploy"
if (Test-Path -LiteralPath $deploy) {
    Get-ChildItem -LiteralPath $deploy -Force | ForEach-Object {
        if ($_.PSIsContainer) { Remove-Tree $_.FullName } else { Remove-One $_.FullName }
    }
}

# ---- 3. bench\: bench-capture.exe and its scripts/outputs. Rebuildable from
#         crates/bench-capture, and it has no business on a serving machine.
Remove-Tree "$Root\bench"

# ---- 4. python prototype cache
Remove-Tree "$Root\__pycache__"

# ---- 5. Minecraft debugging dumps (the launcher scripts stay: the MCGame task
#         runs start-mc.ps1, which calls launch-remote.ps1)
foreach ($name in @(
    'err.txt', 'err2.txt', 'java-now.txt', 'libs-count.txt', 'libs.txt',
    'mc-remote.log', 'mc-state.txt', 'mc.log', 'mc.log.err', 'mouse-accel.log',
    'run-verbose.txt', 'state.txt', 'water.txt', 'mc-remote.cmd'
)) {
    Remove-One "$Root\mc\$name"
}

# ---- 6. root-level leftover (14 bytes, contents: 1.12.json)
Remove-One "$Root\mcjson.txt"

$mb = [math]::Round($script:bytes / 1MB, 2)
if ($WhatIf) {
    Write-Output ("`nWHATIF: {0} files, {1} MB, {2} missing" -f $script:removed, $mb, $script:missing)
} else {
    Write-Output ("`nDONE: {0} files, {1} MB removed, {2} missing, {3} failed" -f $script:removed, $mb, $script:missing, $script:failed)
}
