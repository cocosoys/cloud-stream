# One-shot: strip the historic debug residue out of the controlled machine's
# C:\dev root, leaving only what a running deployment actually needs.
#
# Why a script instead of `ssh mc-admin 'del "C:\dev\*.bat"'`: the command has
# to survive bash -> ssh -> cmd, and the wildcard does not. Every path below is
# explicit, so nothing is deleted by pattern.
#
# Deliberately NOT touched:
#   cert.pem / key.pem          referenced by start-stream.cmd's --tls-cert/key
#   mc-game-task.xml            scheduled-task definitions for the Minecraft
#   mc-launch.cmd               service that shares this machine
#   mc-stream-task.xml
#   mcjson.txt                  unknown provenance; kept rather than guessed
#   directories                 stream/ selkies-web/ deploy/ mc/ jdk8/ bench/
#
# Everything else in the list is a probe, a log, a one-off .bat, or an installer
# from a past debugging session. Re-runnable: missing files are skipped.

param(
    [string]$Root = 'C:\dev',
    [switch]$WhatIf
)

$script:removed = 0
$script:bytes = 0
$script:missing = 0

function Remove-One([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { $script:missing++; return }
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.PSIsContainer) { return }
    $script:bytes += $item.Length
    if ($WhatIf) {
        Write-Output ("WOULD DELETE {0} ({1} KB)" -f $item.FullName, [math]::Round($item.Length / 1KB, 1))
    } else {
        Remove-Item -LiteralPath $Path -Force
        Write-Output ("deleted {0}" -f $item.Name)
    }
    $script:removed++
}

# Root-level residue, by name. Extensions are not used: `*.txt` would take
# mcjson.txt with it.
$files = @(
    'api-probe.py', 'bat-chk.bat', 'bench-capture.exe', 'bench.txt',
    'caller-probe.py', 'caller-probe2.py', 'capture-bench.py',
    'check-stream.bat', 'clean-restart.bat', 'clear-old-143.bat', 'clear-old-143.txt',
    'connect-probe.py', 'cpu-sample.csv', 'cpu-sample.ps1',
    'diag-codec.ps1', 'diag-log.ps1', 'diag-run.py', 'diag-tasks.bat', 'diag-winrt.bat',
    'diag143.cmd', 'diag2.bat', 'diag3.bat', 'diag4.bat', 'diag5.bat', 'diag6.bat',
    'diag7.bat', 'diag8.bat', 'diag9.bat',
    'dxcam-check.bat', 'dxcam-list.bat', 'enc-probe.py',
    'fg-err.log', 'fg-out.log', 'fg-probe2.bat', 'foreground-probe.bat',
    'gen-cert.py', 'grep-log.bat',
    'ice-probe.py', 'ice-probe2.py', 'inspect.ps1', 'install-winrt.bat', 'java-probe.txt',
    'jdk8.tar.gz', 'jsdb.tgz',
    'kill-old.ps1', 'kill-py-143.bat', 'kill-py-143.txt', 'lang-check.bat', 'launch-bench.bat',
    'mem-check.bat', 'mouse-probe.out', 'mouse-probe.ps1', 'null',
    'probe-143.bat', 'probe-143.txt', 'probe-cursor.ps1', 'probe-remote.py', 'probe-top.py',
    'probe-winrt.ps1', 'probe2-143.bat', 'probe2-143.txt', 'ps-tree.py', 'pull-diag.bat',
    'py-err.log', 'py-out.log', 'python-3.11.9-amd64.exe',
    'q-after.bat', 'q-boot.bat', 'q-boot2.bat', 'q-mem.bat', 'q-pf.bat', 'q-state.bat',
    'read-cmd.bat', 'rebuild-tasks.bat', 'res-check.bat', 'restart-probe.bat',
    'restart-stream.bat', 'restart-winrt.bat', 'restart2.bat', 'run-probe.bat',
    'sender-probe.py', 'sess.bat', 'setup-bench-task-143.bat', 'setup-bench-task-143.txt',
    'start-stream.60fps.cmd', 'start-stream.cmd', 'start-winrt.ps1',
    'stream-precheck.py', 'stream-precheck2.py', 'stream-precheck3.py',
    'stream-server.log', 'stream-server.py', 'tail-log.ps1',
    'verify.bat', 'winrt-install.log', 'winrt-precheck.bat',
    '_tmp-check.ps1', '_tmp-check96.ps1', '_tmp-cpu.ps1', '_tmp-env.ps1', '_tmp-fw.ps1',
    '_tmp-idle.ps1', '_tmp-log.ps1', '_tmp-log2.ps1', '_tmp-log3.ps1', '_tmp-log4.ps1',
    '_tmp-log96.ps1', '_tmp-mem.ps1', '_tmp-runprobe.ps1', '_tmp-s96.ps1',
    '_tmp-taskinfo.ps1', '_tmp-web96.ps1'
)

foreach ($name in $files) { Remove-One (Join-Path $Root $name) }

# Inside the run directory: the three superseded binaries, the pre-rename copy of
# the launcher, and the one-off bootstrap/diagnostic helpers. stream-server.exe.prev
# stays — deploy-remote.ps1 -Rollback is built on it.
$stream = Join-Path $Root 'stream'
foreach ($name in @(
    'stream-server.exe.bak.cpu',
    'stream-server.exe.bak63',
    'stream-server.exe.bak77',
    'start-stream.cmd.bak',
    'probe-top.py',
    'setup-watchdog.bat'
)) { Remove-One (Join-Path $stream $name) }

$dump = Join-Path $stream 'dump'
if (Test-Path -LiteralPath $dump) {
    $dumpFiles = Get-ChildItem -LiteralPath $dump -Recurse -File
    $dumpBytes = ($dumpFiles | Measure-Object -Property Length -Sum).Sum
    if ($WhatIf) {
        Write-Output ("WOULD DELETE {0} ({1} file(s), {2} MB)" -f $dump, $dumpFiles.Count, [math]::Round($dumpBytes / 1MB, 1))
    } else {
        Remove-Item -LiteralPath $dump -Recurse -Force
        Write-Output ("deleted stream\dump ({0} file(s), {1} MB)" -f $dumpFiles.Count, [math]::Round($dumpBytes / 1MB, 1))
        $script:removed += $dumpFiles.Count
        $script:bytes += $dumpBytes
    }
}

Write-Output ''
Write-Output ("{0}: {1} file(s), {2} MB" -f $(if ($WhatIf) { 'would remove' } else { 'removed' }), $script:removed, [math]::Round($script:bytes / 1MB, 1))
Write-Output ("absent already: {0}" -f $script:missing)
