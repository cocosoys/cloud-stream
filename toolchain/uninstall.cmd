@echo off
rem ---------------------------------------------------------------------------
rem uninstall.cmd -- take this controlled machine back to "nothing installed".
rem
rem What it removes, in this order (the order is not cosmetic: the watchdog
rem task has to die before the process, or it restarts the server mid-uninstall
rem and the exe stays locked while the folder is being deleted):
rem
rem   1. the task MCStreamSrv         -- but only if its action points at this
rem                                      folder. A task that belongs to another
rem                                      copy is reported, not deleted
rem   2. the running server process   -- only copies whose exe lives under
rem                                      this folder; another install elsewhere
rem                                      is left alone
rem   3. the default recording device -- the server takes it over at startup and
rem                                      is always killed, never asked to clean
rem                                      up, so the original is restored here
rem   4. the four inbound rules       -- names are machine-global, so this also
rem                                      clears what a second copy would use;
rem                                      re-run its install.cmd afterwards
rem   5. the folder itself            -- a detached PowerShell does it 3 s later,
rem                                      because a script cannot delete the
rem                                      directory it is running from
rem
rem If step 1 cannot get rid of the task *and* the task is still enabled, the
rem script stops right there: the watchdog would restart the server while the
rem folder is being deleted, so every later step would fail or, worse, half
rem succeed.
rem
rem Usage:
rem
rem   uninstall.cmd          remove everything, confirm once, keep nothing
rem   uninstall.cmd /keep    remove task + rules + process, keep the files
rem                          (that is the "reinstall into a new version" path)
rem   uninstall.cmd /whatif  print the plan, change nothing
rem   uninstall.cmd /y       no confirmation, no pause (scripts)
rem   uninstall.cmd /force   delete the task even if it points elsewhere
rem
rem What it does NOT touch: uploaded files under the desktop folder the server
rem was pointed at (--upload-dir), the virtual audio cable driver, and the VC++
rem redistributable. Those are shared with the rest of the machine.
rem
rem Keep this file pure ASCII and CRLF.
rem ---------------------------------------------------------------------------
setlocal EnableExtensions

set "ROOT=%~dp0"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"
set "EXE=%ROOT%\stream-server.exe"
set "TASK=MCStreamSrv"
set "PSS=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
set "TMPF=%TEMP%\cloud-stream-uninstall.tmp"
rem Exported for the PowerShell snippets below: they filter processes by the
rem exe path so that a second install elsewhere is never touched.
set "CS_ROOT=%ROOT%"
rem Self path, exported for the elevation call: Start-Process then needs no
rem quoting at all. A "\"" inside -Command has to survive cmd's parser and then
rem PowerShell's, and every escape character in that chain is a way to open the
rem wrong file.
set "CS_SELF=%~f0"

set "MODE=uninstall"
set "NOPAUSE=0"
set "FORCE=0"
rem /resumed is internal: the elevated copy of this script must not ask the same
rem confirmation a second time.
set "RESUMED=0"

:args
if "%~1"=="" goto :args_done
if /i "%~1"=="/keep" set "MODE=keep"
if /i "%~1"=="/whatif" set "MODE=whatif"
if /i "%~1"=="/y" set "NOPAUSE=1"
if /i "%~1"=="/yes" set "NOPAUSE=1"
if /i "%~1"=="/force" set "FORCE=1"
if /i "%~1"=="/resumed" set "RESUMED=1"
if /i "%~1"=="/?" goto :usage
if /i "%~1"=="/h" goto :usage
shift
goto :args

:usage
echo Usage:
echo   uninstall.cmd          remove task, rules, process and files
echo   uninstall.cmd /keep    remove task, rules, process; keep the files
echo   uninstall.cmd /whatif  print the plan, change nothing
echo   uninstall.cmd /y       no confirmation, no pause
echo   uninstall.cmd /force   delete the task even if it points elsewhere
exit /b 0

:args_done

echo ======================================================================
echo  cloud-stream  -  one-click uninstall
echo ======================================================================
echo   bundle : %ROOT%
echo   task   : %TASK%
echo   mode   : %MODE%
echo ======================================================================
echo.

rem ---------------------------------------------------------------------------
rem Guards. This script deletes a whole directory, and a directory is exactly
rem the kind of thing that is one typo away from something that must not go.
rem Every check below is cheap and none of them can be talked out of.
rem ---------------------------------------------------------------------------
if "%ROOT:~3%"=="" goto :bad_root
if /i "%ROOT%"=="%SystemRoot%" goto :bad_root
if /i "%ROOT%"=="%USERPROFILE%" goto :bad_root
if /i "%ROOT%"=="%SystemDrive%" goto :bad_root
if exist "%EXE%" goto :root_ok
if exist "%ROOT%\manifest.json" goto :root_ok
goto :bad_root

:root_ok

rem ---- inventory ------------------------------------------------------------
rem goto instead of `if ... ( ) else ( )` wherever an expanded value can contain
rem a closing paren: a bundle unpacked into "D:\stream (2)" would otherwise
rem close the block early and cmd would run the rest of the line as a command.
echo -- what was found --
call :task_action
if not defined OLDACT goto :inv_no_task
echo   [x] task %TASK%            %OLDACT%
goto :inv_procs
:inv_no_task
echo   [ ] task %TASK%            not present
:inv_procs

call :count_processes
if "%PROCS%"=="0" goto :inv_no_proc
echo   [x] stream-server.exe      %PROCS% process^(es^) from this folder
goto :inv_rules
:inv_no_proc
echo   [ ] stream-server.exe      not running from this folder
:inv_rules
set "FWRULES=0"
netsh advfirewall firewall show rule name="cloud-stream 8080" >nul 2>&1
if not errorlevel 1 set /a FWRULES+=1
netsh advfirewall firewall show rule name="cloud-stream ICMPv4" >nul 2>&1
if not errorlevel 1 set /a FWRULES+=1
netsh advfirewall firewall show rule name="cloud-stream webrtc UDP" >nul 2>&1
if not errorlevel 1 set /a FWRULES+=1
netsh advfirewall firewall show rule name="cloud-stream app" >nul 2>&1
if not errorlevel 1 set /a FWRULES+=1
if "%FWRULES%"=="0" goto :inv_no_rules
echo   [x] inbound rules          %FWRULES% of 4 present
goto :inv_files
:inv_no_rules
echo   [ ] inbound rules          none of the four present
:inv_files

if exist "%ROOT%\auth-token.txt" echo   [x] auth-token.txt         a password is configured
if exist "%ROOT%\logs\mic-default.json" echo   [x] logs\mic-default.json   the default recorder was taken over
if "%MODE%"=="keep" goto :inv_files_kept
echo   [x] files                  %ROOT%
goto :inv_done
:inv_files_kept
echo   [ ] files                  KEPT ^(/keep^)
:inv_done

echo.
if "%MODE%"=="whatif" goto :whatif_done

rem ---- confirmation ---------------------------------------------------------
if "%NOPAUSE%"=="1" goto :go
if "%RESUMED%"=="1" goto :go
echo   This stops the server and removes the items marked [x] above.
choice /C YN /N /M "Continue? [Y/N] "
if errorlevel 2 goto :abort

:go

rem ---- elevation ------------------------------------------------------------
rem Needed for the task and the firewall rules. Checked after confirmation so
rem that a declined UAC prompt does not look like a refused uninstall.
net session >nul 2>&1
if not errorlevel 1 goto :elevated
echo.
echo -- [admin] this needs administrator rights; opening a UAC prompt --
rem The elevated copy has to inherit the mode: without /keep it would delete the
rem files the operator asked to keep, and without /resumed it would ask the same
rem question again in the new window.
set "REARGS=/resumed"
if "%NOPAUSE%"=="1" set "REARGS=%REARGS% /y"
if "%MODE%"=="keep" set "REARGS=%REARGS% /keep"
if "%FORCE%"=="1" set "REARGS=%REARGS% /force"
"%PSS%" -NoProfile -Command "Start-Process -FilePath $env:CS_SELF -ArgumentList '%REARGS%' -Verb RunAs" >nul 2>&1
if errorlevel 1 (
    echo   [fail] the elevation prompt was declined or blocked.
    echo          Right-click uninstall.cmd and choose "Run as administrator".
    goto :die
)
echo   the elevated window does the work; this one can be closed.
exit /b 0

:elevated

rem ---- 1. the task, FIRST ---------------------------------------------------
rem Order matters. The watchdog fires every minute: killing the process before
rem unregistering the task leaves a window in which it is restarted, and then
rem the exe is held open while the folder is being deleted. Stop the task (which
rem also ends the process tree it started), then deal with what is left.
echo.
echo -- [1/5] watchdog task %TASK% --
if not defined OLDACT goto :task_absent
echo "%OLDACT%" | findstr /I /C:"%ROOT%" >nul
if errorlevel 1 goto :task_foreign
call :remove_task
call :task_action
if not defined OLDACT goto :task_removed
rem Still registered. A disabled task cannot start anything, so it is not a
rem reason to refuse the uninstall; an enabled one is, because the watchdog
rem would restart the server in the middle of the deletion.
call :task_state
if /i "%TSTATE%"=="Disabled" goto :task_disabled

echo   [fail] task %TASK% could not be removed and is still enabled.
echo          Its watchdog restarts the server every minute, so the folder
echo          cannot be deleted. The server process was stopped; nothing else
echo          was touched.
echo.
echo          Delete the task in Task Scheduler (taskschd.msc) and run this
echo          script again.
set "RC=1"
goto :finish

:task_disabled
echo   [warn] task %TASK% is still registered but disabled, so the uninstall
echo          continues. Delete it in Task Scheduler when convenient.
goto :stop_process

:task_removed
echo   [ok] task %TASK% removed
goto :stop_process

:task_absent
echo   [ok] no task %TASK% to remove
goto :stop_process

:task_foreign
echo   [skip] task %TASK% points at a different folder:
echo          %OLDACT%
if "%FORCE%"=="1" goto :task_force
echo          left alone. Run uninstall.cmd in that folder to remove it.
goto :stop_process

:task_force
echo   [i] /force: removing %TASK% even though it points elsewhere
call :remove_task
call :task_action
if defined OLDACT goto :task_force_failed
echo   [ok] task %TASK% removed ^(/force^)
goto :stop_process

:task_force_failed
echo   [warn] task %TASK% is still registered ^(/force^)
goto :stop_process

rem ---- 2. the process -------------------------------------------------------
:stop_process
echo.
echo -- [2/5] stopping stream-server.exe --
"%PSS%" -NoProfile -Command "Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'stream-server.exe' -and $_.ExecutablePath -and $_.ExecutablePath.StartsWith($env:CS_ROOT, [System.StringComparison]::OrdinalIgnoreCase) } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }" >nul 2>&1
call :count_processes
if "%PROCS%"=="0" (
    echo   [ok] nothing running from this folder
) else (
    echo   [warn] %PROCS% process^(es^) survived -- close them by hand, then run
    echo          this script again to remove the files
)

rem ---- 3. the default recording device --------------------------------------
rem Before anything is deleted: the original value lives in
rem logs\mic-default.json, which is about to go away with the folder.
echo.
echo -- [3/5] restoring the default recording device --
if not exist "%EXE%" goto :mic_skip
rem Capability probe, before anything is executed. An exe built before the
rem one-click uninstaller does not know --restore-mic; Config::parse ignores
rem unknown switches, so the old binary would quietly start a FULL server --
rem which never returns, grabs port 8080 and takes the recorder over again.
rem That is precisely the "hangs at step 3" that was reported.
rem
rem The needle is MICRESTORE, not "--restore-mic": rustc folds short switch
rem literals into immediate comparisons, so the switch name is not in the exe
rem at all (checked: 0 occurrences, while "--no-hdr" survives only because it
rem is also printed). The output text always is, and it only exists in a build
rem that has the restore path.
rem
rem If the probe is wrong the failure direction is safe: the exe is simply not
rem run, and the operator is told to put the device back by hand.
findstr /M /C:"MICRESTORE" "%EXE%" >nul 2>&1
if errorlevel 1 goto :mic_old_exe
rem The exe bounds itself to 5 s (see main.rs). Before that it printed its
rem first line, so "no output at all" can only mean the process never reached
rem main -- a different problem, and one the operator should not sit through
rem silently.
echo   [i] running stream-server.exe --restore-mic (bounded to 5 s)
pushd "%ROOT%"
"%EXE%" --restore-mic --log "%ROOT%\logs\stream-server.log" > "%TMPF%" 2>&1
popd
if not exist "%TMPF%" goto :mic_skip
for /f "usebackq delims=" %%L in ("%TMPF%") do echo   [i] %%L
rem Success must be positively observed. "Not timeout and not not-restored"
rem would also be true for a helper that printed its first line and then died,
rem and the machine would be left on the virtual cable with a clean-looking log.
findstr /C:"MICRESTORE nothing-to-do" "%TMPF%" >nul 2>&1
if not errorlevel 1 goto :mic_ok
findstr /C:"MICRESTORE restored" "%TMPF%" >nul 2>&1
if errorlevel 1 goto :mic_kept
findstr /C:"MICRESTORE not-restored" "%TMPF%" >nul 2>&1
if not errorlevel 1 goto :mic_kept

:mic_ok
del /f /q "%TMPF%" >nul 2>&1
goto :mic_done

:mic_old_exe
echo   [skip] this stream-server.exe predates --restore-mic ^(it was built
echo          before the one-click uninstaller^). It would not restore anything
echo          -- it would start a full server and never return -- so it is not
echo          run at all.
goto :mic_kept

:mic_kept
rem The folder and the record file are about to go; the record is the only
rem evidence of what the default used to be, so keep a copy outside.
del /f /q "%TMPF%" >nul 2>&1
set "MICBAK=%TEMP%\cloud-stream-mic-default.json"
copy /y "%ROOT%\logs\mic-default.json" "%MICBAK%" >nul 2>&1
echo   [warn] the default recorder was NOT put back.
echo          Set it by hand in Settings - System - Sound ^(or the Recording
echo          tab of mmsys.cpl^) before the next call.
if exist "%MICBAK%" echo          what it used to be: %MICBAK%
goto :mic_done

:mic_skip
echo   [skip] stream-server.exe is gone; nothing to restore ^(or nothing was
echo          ever taken over^)

:mic_done

rem ---- 4. the inbound rules -------------------------------------------------
echo.
echo -- [4/5] inbound firewall rules --
netsh advfirewall firewall delete rule name="cloud-stream 8080" >nul 2>&1
netsh advfirewall firewall delete rule name="cloud-stream ICMPv4" >nul 2>&1
netsh advfirewall firewall delete rule name="cloud-stream webrtc UDP" >nul 2>&1
netsh advfirewall firewall delete rule name="cloud-stream app" >nul 2>&1
rem Also the automatic "Windows blocked some features of this app" rules: a
rem BLOCK rule beats any allow and no `show rule name=` output distinguishes
rem them from an allow, so leaving one behind is a rule that looks present and
rem forwards nothing.
netsh advfirewall firewall delete rule name=all dir=in program="%EXE%" >nul 2>&1

set "LEFT=0"
netsh advfirewall firewall show rule name="cloud-stream 8080" >nul 2>&1
if not errorlevel 1 set /a LEFT+=1
netsh advfirewall firewall show rule name="cloud-stream ICMPv4" >nul 2>&1
if not errorlevel 1 set /a LEFT+=1
netsh advfirewall firewall show rule name="cloud-stream webrtc UDP" >nul 2>&1
if not errorlevel 1 set /a LEFT+=1
netsh advfirewall firewall show rule name="cloud-stream app" >nul 2>&1
if not errorlevel 1 set /a LEFT+=1
if "%LEFT%"=="0" (
    echo   [ok] inbound rules removed
) else (
    echo   [warn] %LEFT% rule^(s^) survived -- a domain policy or a third-party
    echo          firewall owns them. Remove them by name in Windows Firewall.
)

rem ---- 5. the files ---------------------------------------------------------
if "%MODE%"=="keep" goto :keep_done

echo.
echo -- [5/5] removing the folder --
echo   %ROOT%
if "%NOPAUSE%"=="0" (
    echo   It disappears a few seconds after this window closes.
    echo Press any key to close this window.
    pause >nul
)
call :schedule_delete
exit /b 0

:keep_done
echo.
echo -- done, files kept --
echo   %ROOT%
echo   The task, the rules and the process are gone; the folder is intact.
echo   Run install.cmd in it again to put the machine back into service.
goto :finish

:whatif_done
echo.
echo -- /whatif: nothing was changed --
goto :finish

:abort
echo.
echo Cancelled. Nothing was changed.
goto :finish

:bad_root
echo   [fail] refusing to uninstall from this path:
echo          %ROOT%
echo.
echo   Expected a cloud-stream folder ^(stream-server.exe or manifest.json
echo   next to this script^), and not a drive root, %SystemRoot% or %USERPROFILE%.
echo   Delete it by hand once you are sure.
set "RC=1"
goto :finish

:finish
echo.
del /f /q "%TMPF%" >nul 2>&1
if "%NOPAUSE%"=="1" goto :finish_code
echo Press any key to close this window.
pause >nul
:finish_code
rem RC is only set by the refusal branch: a refused uninstall must not report
rem success to a script that called it.
if defined RC exit /b 1
exit /b 0

:die
echo.
if "%NOPAUSE%"=="1" exit /b 1
echo Press any key to close this window.
pause >nul
exit /b 1

rem ---------------------------------------------------------------------------
rem :remove_task -- stop then unregister %TASK%.
rem
rem Why this is not a one-liner. On the first real uninstall run the task
rem survived with nothing printed, for two different reasons:
rem
rem   * `Unregister-ScheduledTask` refuses while an instance of the task is
rem     still winding down ("an instance of this task is already running",
rem     0x8004131F). `Stop-ScheduledTask` only *asks*; the state stays Running
rem     for a moment after it returns. So: stop, poll State until it is not
rem     Running (max 6 s), then unregister.
rem   * for policy reasons the module can fail on a machine where the older
rem     API still works, so `schtasks /Delete /F` is tried as well. It also
rem     removes a task that is still running.
rem
rem Anything the module says is printed as [i] -- a silent failure is what made
rem this take a second attempt to notice in the first place.
rem ---------------------------------------------------------------------------
:remove_task
del /f /q "%TMPF%" >nul 2>&1
"%PSS%" -NoProfile -Command "$n='%TASK%'; Stop-ScheduledTask -TaskName $n -ErrorAction SilentlyContinue; for ($i=0; $i -lt 40; $i++) { $t = Get-ScheduledTask -TaskName $n -ErrorAction SilentlyContinue; if ($null -eq $t -or [string]$t.State -ne 'Running') { break }; Start-Sleep -Milliseconds 150 }; if ($null -ne (Get-ScheduledTask -TaskName $n -ErrorAction SilentlyContinue)) { try { Unregister-ScheduledTask -TaskName $n -Confirm:$false -ErrorAction Stop } catch { $_.Exception.Message } }" > "%TMPF%" 2>&1
if exist "%TMPF%" for /f "usebackq delims=" %%A in ("%TMPF%") do echo   [i] %%A
schtasks /End /TN "%TASK%" >nul 2>&1
schtasks /Delete /TN "%TASK%" /F >nul 2>&1
del /f /q "%TMPF%" >nul 2>&1
exit /b 0

rem ---------------------------------------------------------------------------
rem :task_state -- the task's state into TSTATE, empty when absent.
rem Only used to tell "still there but harmless" (Disabled) from "still there
rem and about to restart the server" (Ready/Running).
rem ---------------------------------------------------------------------------
:task_state
set "TSTATE="
del /f /q "%TMPF%" >nul 2>&1
"%PSS%" -NoProfile -Command "(Get-ScheduledTask -TaskName '%TASK%' -ErrorAction SilentlyContinue).State" > "%TMPF%" 2>nul
if not exist "%TMPF%" exit /b 0
for /f "usebackq delims=" %%A in ("%TMPF%") do if not defined TSTATE set "TSTATE=%%A"
del /f /q "%TMPF%" >nul 2>&1
if not defined TSTATE set "TSTATE=unknown"
exit /b 0

rem ---------------------------------------------------------------------------
rem :task_action -- the task's action line into OLDACT, empty when absent.
rem Through a temp file rather than `for /f ... in (`...`)`: the PowerShell
rem pipeline has pipes and quotes, and cmd's parser mangles them inside
rem backquotes differently across versions.
rem ---------------------------------------------------------------------------
:task_action
set "OLDACT="
del /f /q "%TMPF%" >nul 2>&1
"%PSS%" -NoProfile -Command "(Get-ScheduledTask -TaskName '%TASK%' -ErrorAction SilentlyContinue).Actions | ForEach-Object { $_.Execute + ' ' + $_.Arguments }" > "%TMPF%" 2>nul
if not exist "%TMPF%" exit /b 0
for /f "usebackq delims=" %%A in ("%TMPF%") do if not defined OLDACT set "OLDACT=%%A"
del /f /q "%TMPF%" >nul 2>&1
exit /b 0

rem ---------------------------------------------------------------------------
rem :count_processes -- PROCS = number of stream-server.exe running from this
rem folder. Filtering by path is the whole point: a second install elsewhere
rem must not be killed by this script.
rem ---------------------------------------------------------------------------
:count_processes
set "PROCS=0"
del /f /q "%TMPF%" >nul 2>&1
"%PSS%" -NoProfile -Command "@(Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'stream-server.exe' -and $_.ExecutablePath -and $_.ExecutablePath.StartsWith($env:CS_ROOT, [System.StringComparison]::OrdinalIgnoreCase) }).Count" > "%TMPF%" 2>nul
if not exist "%TMPF%" exit /b 0
for /f "usebackq delims=" %%A in ("%TMPF%") do set "PROCS=%%A"
del /f /q "%TMPF%" >nul 2>&1
if not defined PROCS set "PROCS=0"
exit /b 0

rem ---------------------------------------------------------------------------
rem :schedule_delete -- hand the removal to a detached PowerShell that waits a
rem few seconds.
rem
rem A batch file cannot delete the directory it is running from: cmd.exe has it
rem as its working directory and as the file it is executing, so rd fails with
rem "being used by another process". Waiting 3 s gives this console time to
rem close; Remove-Item then works on a folder nobody holds open.
rem
rem Three details that are load-bearing:
rem   * cd /d %%TEMP%% first -- the child inherits the working directory, and a
rem     process whose CWD is the target folder makes it undeletable
rem   * the path goes in single quotes, so no quote character ever has to
rem     survive cmd's parser (a "\"" here is read differently by each layer)
rem   * a path containing ' would end that string early, so it is refused
rem     up front and the operator deletes the folder by hand
rem ---------------------------------------------------------------------------
:schedule_delete
cd /d "%TEMP%"
echo "%ROOT%" | findstr /C:"'" >nul 2>&1
if not errorlevel 1 goto :sd_manual
start "" /min "%PSS%" -NoProfile -WindowStyle Hidden -Command "Start-Sleep -Seconds 3; Remove-Item -LiteralPath '%ROOT%' -Recurse -Force -ErrorAction SilentlyContinue"
if errorlevel 1 goto :sd_manual
echo   [ok] scheduled; the folder will be gone in a few seconds.
exit /b 0

:sd_manual
echo   [warn] this folder cannot be removed automatically -- its name contains
echo          a quote character, or the helper could not be started.
echo          Close any window inside it and delete it by hand:
echo            %ROOT%
exit /b 0
