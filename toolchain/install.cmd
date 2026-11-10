@echo off
rem ---------------------------------------------------------------------------
rem install.cmd -- turn this directory into a working controlled machine.
rem
rem Five steps, in the only order that makes sense:
rem
rem   0. verify the bundle           -- a half-unpacked zip fails here, loudly
rem   1. open the inbound rules      -- otherwise every packet is dropped and the
rem                                     machine looks dead while serving fine
rem   2. register the MCStreamSrv task -- interactive token, no password stored
rem   3. start it and wait up to 60 s for 127.0.0.1:8080
rem   4. print the page addresses and the two follow-ups worth knowing
rem
rem Usage:
rem
rem   install.cmd            double-click. Self-elevates through UAC, then
rem                          pauses so the verdict stays on screen.
rem   install.cmd /check     read-only preflight. Changes nothing, needs no
rem                          admin. This is what to ask for in a bug report.
rem   install.cmd /y         no pause at the end (scripts, remote shells)
rem
rem Why this exists on top of setup-watchdog.bat: that script assumes a human
rem already knows the account story, the firewall story and how to read
rem "[fail]". Installing on a fresh machine used to be "read deploy-new-host.md,
rem then run two scripts in the right order as the right user, then figure out
rem which of the four usual causes you hit". This file is that knowledge, run
rem once. It calls exactly the same helpers -- setup-task.ps1 and
rem open-inbound.cmd -- so there is no second implementation of anything.
rem
rem Idempotent on purpose: running it again is the repair path. The task is
rem replaced (Register-ScheduledTask -Force), the rules are deleted and re-added
rem by name, and an already-running server is left alone -- the probes below
rem answer "is it serving" either way.
rem
rem Keep this file pure ASCII and CRLF: cmd reads it byte by byte in the OEM
rem code page.
rem ---------------------------------------------------------------------------
setlocal EnableExtensions

set "ROOT=%~dp0"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"
set "EXE=%ROOT%\stream-server.exe"
set "TASK=MCStreamSrv"
set "PSS=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
set "TIMEOUT=%SystemRoot%\System32\timeout.exe"
set "CURL=%SystemRoot%\System32\curl.exe"
set "FIND=%SystemRoot%\System32\find.exe"
set "FINDSTR=%SystemRoot%\System32\findstr.exe"
set "TMPF=%TEMP%\cloud-stream-install.tmp"
rem Self path, exported for the elevation call: Start-Process then needs no
rem quoting at all. A "\"" inside -Command has to survive cmd's parser and then
rem PowerShell's, and every escape character in that chain is a way to open the
rem wrong file.
set "CS_SELF=%~f0"

rem ---- arguments ------------------------------------------------------------
set "MODE=install"
set "NOPAUSE=0"

:args
if "%~1"=="" goto :args_done
if /i "%~1"=="/check" set "MODE=check"
if /i "%~1"=="/y" set "NOPAUSE=1"
if /i "%~1"=="/yes" set "NOPAUSE=1"
if /i "%~1"=="/?" goto :usage
if /i "%~1"=="/h" goto :usage
if /i "%~1"=="/help" goto :usage
shift
goto :args

:usage
echo Usage:
echo   install.cmd            install / repair, self-elevates
echo   install.cmd /check     read-only preflight, changes nothing
echo   install.cmd /y         no pause at the end
exit /b 0

:args_done

echo ======================================================================
echo  cloud-stream  -  one-click install
echo ======================================================================
echo   bundle : %ROOT%
echo   task   : %TASK%
echo   port   : 8080
echo ======================================================================
echo.

rem ---------------------------------------------------------------------------
rem A UNC path cannot be elevated: the UAC prompt runs the script through
rem CreateProcess with a working directory Windows refuses to set to \\host\...,
rem so cmd exits with "CMD does not support UNC paths" the moment the elevated
rem window opens. Happens to everyone who unzips onto a shared folder.
rem ---------------------------------------------------------------------------
if "%ROOT:~0,2%"=="\\" (
    echo   [fail] this bundle is on a network share:
    echo          %ROOT%
    echo          Windows cannot install from a UNC path. Copy the folder to a
    echo          local disk ^(C:\ or D:\^) and run install.cmd there.
    goto :die
)

rem ---- step 0: is this really a complete bundle? ----------------------------
echo -- [0/4] bundle check --
set "BAD=0"

call :need "%EXE%"                        stream-server.exe
call :need "%ROOT%\start-stream.cmd"      start-stream.cmd
call :need "%ROOT%\stream-config.toml"    stream-config.toml
call :need "%ROOT%\run-hidden.vbs"        run-hidden.vbs
call :need "%ROOT%\setup-task.ps1"        setup-task.ps1
call :need "%ROOT%\open-inbound.cmd"      open-inbound.cmd
call :need "%ROOT%\selkies-web\index.html" selkies-web\index.html

if "%BAD%"=="1" goto :incomplete

set "VCMISS=0"
if exist "%SystemRoot%\System32\vcruntime140.dll" goto :vc_ok
set "VCMISS=1"
echo   [warn] vcruntime140.dll is not in System32 -- install the
echo          "Microsoft Visual C++ 2015-2022 Redistributable (x64)" first,
echo          or the exe will exit before it can write a single log line.
:vc_ok

rem The exe's import table names VCRUNTIME140.dll, so a machine without the
rem redistributable cannot even reach main(). Warning instead of failing: the
rem redistributable may be installed per-app or the check may be looking at the
rem wrong bitness; the probe at the end is the real verdict.

rem Two things that explain a "it installed but the page is blank/timeout":
rem another copy of the server, and another program on 8080 entirely.
call :task_action
if not defined OLDACT goto :port_check
echo   [i] a task named %TASK% already exists:
echo        %OLDACT%
echo "%OLDACT%" | findstr /I /C:"%ROOT%" >nul
if not errorlevel 1 goto :port_check
echo   [warn] it points somewhere else -- this run replaces it with %ROOT%
echo          ^(run uninstall.cmd in the old folder to clean that one up^)

:port_check
set "OWNSRV=0"
tasklist /FI "IMAGENAME eq stream-server.exe" 2>nul | "%FIND%" /I "stream-server.exe" >nul
if errorlevel 1 goto :port_listen
set "OWNSRV=1"
echo   [i] a stream-server.exe is already running -- repair path: the task is
echo        re-registered and the running process is not interrupted.
goto :check_done

:port_listen
rem Only when nothing of ours is up: otherwise the "8080 is taken" warning is
rem just this server, and a reinstall would look like a port conflict.
netstat -ano | "%FINDSTR%" /C:":8080 " | "%FINDSTR%" /C:"LISTENING" >nul 2>&1
if errorlevel 1 goto :check_done
echo   [warn] port 8080 already has a listener that is not stream-server.exe.
echo          The probe below will report [fail] until it is gone -- find it:
echo            netstat -ano ^| findstr :8080

:check_done

if "%MODE%"=="check" goto :check_summary

rem ---------------------------------------------------------------------------
rem Elevation. Rules and task registration both need it. Rather than teaching
rem the operator "right-click, run as administrator", re-launch ourselves: the
rem elevated copy runs in its own window with the same arguments and the parent
rem exits, so there is exactly one window to read.
rem ---------------------------------------------------------------------------
net session >nul 2>&1
if not errorlevel 1 goto :elevated

echo.
echo -- [admin] this needs administrator rights; opening a UAC prompt --
if "%NOPAUSE%"=="1" (
    "%PSS%" -NoProfile -Command "Start-Process -FilePath $env:CS_SELF -ArgumentList '/y' -Verb RunAs" >nul 2>&1
) else (
    "%PSS%" -NoProfile -Command "Start-Process -FilePath $env:CS_SELF -Verb RunAs" >nul 2>&1
)
if errorlevel 1 (
    echo   [fail] the elevation prompt was declined or blocked.
    echo          Right-click install.cmd and choose "Run as administrator".
    goto :die
)
echo   the elevated window does the work; this one can be closed.
exit /b 0

:elevated

rem ---- step 1: firewall -----------------------------------------------------
echo.
echo -- [1/4] inbound firewall rules --
call "%ROOT%\open-inbound.cmd" >nul 2>&1
if errorlevel 2 goto :fw_refused
if errorlevel 1 goto :fw_denied
echo   [ok] 8080/TCP, ICMP echo, UDP 40000-40049 and the program rule are in
echo        place. Without the UDP range the page reports "WebRTC cannot
echo        connect / the media path fails" while WebSockets looks perfect.
goto :task_step

:fw_refused
echo   [warn] netsh refused the rules: a third-party firewall or a domain
echo          policy owns the profile. Allow stream-server.exe there as well,
echo          otherwise only this machine can reach the page.
goto :task_step

:fw_denied
echo   [warn] the rules are still missing -- this process is not elevated
echo          after all. Run setup-inbound.cmd as administrator once.
goto :task_step

rem ---- step 2: the watchdog task -------------------------------------------
:task_step
echo.
echo -- [2/4] watchdog task %TASK% --
rem Stop the old instance first. Re-running install.cmd on top of a live install
rem is the documented repair path, and re-registering a task whose instance is
rem still running is refused on some Windows builds ("the task is currently
rem running"). Task Scheduler restarts it from -Start a few lines below.
"%PSS%" -NoProfile -Command "Stop-ScheduledTask -TaskName '%TASK%' -ErrorAction SilentlyContinue" >nul 2>&1
"%PSS%" -NoProfile -ExecutionPolicy Bypass -File "%ROOT%\setup-task.ps1" -Root "%ROOT%" -TaskName "%TASK%" -Start
set "RC=%errorlevel%"
if "%RC%"=="0" goto :task_ok
if "%RC%"=="5" (
    echo   [warn] the task exists but did not start. The next minute's trigger
    echo          will retry; to start it now: Task Scheduler -^> %TASK% -^> Run
    goto :probe
)
echo   [fail] task registration failed (exit %RC%). Nothing below will work.
goto :die

:task_ok
echo   [ok] task created: fires at logon and every minute, no password stored.

rem ---- step 3: does it actually serve? -------------------------------------
:probe
echo.
echo -- [3/4] waiting up to 60 s for 127.0.0.1:8080 --

set /a TRY=0
:wait
set /a TRY+=1
"%TIMEOUT%" /t 5 /nobreak >nul 2>&1
call :probe_port
if "%CODE%"=="200" goto :up
if %TRY% lss 12 goto :wait
goto :down

:up
echo   [ok] stream-server answered after %TRY% probe^(s^).

echo.
echo -- [4/4] done --
echo.
echo   Open from another machine ^(any of these^):
del /f /q "%TMPF%" >nul 2>&1
"%PSS%" -NoProfile -Command "Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.IPAddress -notlike '127.*' -and $_.IPAddress -notlike '169.254.*' } | Sort-Object IPAddress | ForEach-Object { $_.IPAddress }" > "%TMPF%" 2>nul
set "HADIP=0"
if not exist "%TMPF%" goto :ip_done
for /f "usebackq delims=" %%A in ("%TMPF%") do call :show_ip "%%A"
:ip_done
if "%HADIP%"=="0" echo        https://^<this-machine-ip^>:8080/    ^(run ipconfig^)
del /f /q "%TMPF%" >nul 2>&1

echo.
echo   Worth doing now ^(both are one double-click in this folder^):
echo     set-password.cmd   require a token: by default anyone on the LAN who
echo                        reaches the port can drive this machine.
echo     collect-launch-facts.cmd  if anything looks wrong, this writes a
echo                        single facts file instead of guessing.
echo.
echo   Removing it later:  uninstall.cmd
goto :finish

:down
echo   [fail] nothing listening on 127.0.0.1:8080 after 12 probes ^(~60 s^).
echo.
echo   In this order:
if "%VCMISS%"=="1" echo     0^) vcruntime140.dll is missing -- install the VC++ 2015-2022
if "%VCMISS%"=="1" echo        x64 redistributable, then run install.cmd again.
echo     1^) read the two logs:
echo          type "%ROOT%\logs\watchdog.log"
echo          type "%ROOT%\logs\stream-server.log"
echo        An empty watchdog.log means the task never ran: the account that
echo        owns the desktop is not the one the task was registered for.
echo     2^) collect-launch-facts.cmd  -- writes cloud-stream-facts.txt, read-only
echo     3^) is another program on 8080?   netstat -ano ^| findstr :8080
goto :finish

:check_summary
echo.
echo -- /check: nothing was changed --
echo   Bundle looks complete. To install, run this file again without /check.
if "%VCMISS%"=="1" echo   [warn] VC++ runtime missing -- fix that first.
goto :finish

:incomplete
echo.
echo   [fail] this directory is not a complete bundle.
echo          Unpack cloud-stream.zip in full, then run install.cmd from the
echo          folder that contains stream-server.exe.
set "RC=1"
goto :finish

:finish
echo.
del /f /q "%TMPF%" >nul 2>&1
if not "%RC%"=="0" if not "%RC%"=="" goto :die
if "%NOPAUSE%"=="1" exit /b 0
echo Press any key to close this window.
pause >nul
exit /b 0

:die
echo.
if "%NOPAUSE%"=="1" exit /b 1
echo Press any key to close this window.
pause >nul
exit /b 1

rem ---------------------------------------------------------------------------
rem :need <absolute path> <name shown>
rem One missing file is enough to stop: a bundle without the web root serves a
rem directory listing, and one without run-hidden.vbs registers a task that
rem fires every minute and does nothing.
rem ---------------------------------------------------------------------------
:need
if exist "%~1" goto :need_ok
echo   [fail] missing %~2
set "BAD=1"
exit /b 1
:need_ok
echo   [ok]   %~2
exit /b 0

rem ---------------------------------------------------------------------------
rem :show_ip <address> -- one line per usable IPv4, plus the "we printed
rem something" flag that suppresses the generic fallback.
rem ---------------------------------------------------------------------------
:show_ip
set "HADIP=1"
echo        https://%~1:8080/
exit /b 0

rem ---------------------------------------------------------------------------
rem :task_action -- the current task's action line into OLDACT (empty if none).
rem Through a temp file rather than `for /f ... in (`...`)`: the PowerShell
rem pipeline contains pipes and quotes, and cmd's parser mangles them inside
rem backquotes in ways that differ between versions.
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
rem :probe_port -- CODE=200 if something answers /api/status on loopback.
rem
rem -f and the exit code, not -w "%%{http_code}": the format string gets eaten
rem by cmd's variable expansion inside a block and curl then reports "no URL
rem specified", which reads as "still not listening".
rem
rem --noproxy "*": a company-image http_proxy routes the loopback request to
rem that proxy, which answers 502 -- a healthy server reported as dead.
rem
rem Both schemes: the bundle ships cert.pem, but a copy without certificates
rem serves plain HTTP, and /api/status stays open on purpose (#101 put the
rem health API behind the password).
rem ---------------------------------------------------------------------------
:probe_port
"%CURL%" -sk -m 3 -f --noproxy "*" -o NUL https://127.0.0.1:8080/api/status >nul 2>&1
if not errorlevel 1 (
    set "CODE=200"
    exit /b 0
)
"%CURL%" -sk -m 3 -f --noproxy "*" -o NUL http://127.0.0.1:8080/api/status >nul 2>&1
if not errorlevel 1 (
    set "CODE=200"
    exit /b 0
)
set "CODE=000"
exit /b 0
