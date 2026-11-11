@echo off
rem ---------------------------------------------------------------------------
rem Create the MCStreamSrv watchdog task for THIS directory.
rem
rem Portable: the task action is "<this dir>\stream-server.exe --run", built
rem from %%~dp0, so the bundle can be unpacked anywhere.
rem
rem Usage, from the directory the bundle was unpacked into:
rem
rem   setup-watchdog.bat                     <- uses the console user, no password
rem   setup-watchdog.bat <account>           <- override the account, rarely needed
rem
rem Nothing on that command line is a credential. The task is registered as
rem "run only when the user is logged on" (InteractiveToken), which reuses the
rem session's existing token instead of performing a logon, so this works on an
rem account with a blank password and leaves the account untouched. The real
rem work is in setup-task.ps1 -- see the header there for why the old
rem schtasks /RP line had to go.
rem
rem #81: turn the task from a one-shot launcher into a watchdog.
rem
rem Before: /SC ONCE, "repeat disabled" -- the server ran until it died and then
rem stayed dead until somebody typed `schtasks /Run`.
rem
rem After: at logon, and every minute after that. Task Scheduler's "do not start
rem a new instance" policy means that while the server is alive the trigger does
rem nothing; when it dies the task completes and the next minute brings it back.
rem
rem The task action is the server exe itself, not an interpreter wrapping a
rem script. The reason the old launcher went through the script host at all was
rem that a console-subsystem exe gets a console window from Task Scheduler --
rem every trigger, 1440 times a day, each one stealing focus from the game. The
rem server is a GUI-subsystem binary now, so no window is created and the layer
rem is gone. See docs/operations-run.md.
rem ---------------------------------------------------------------------------
setlocal EnableExtensions

set "ROOT=%~dp0"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"
set "SVEXE=%ROOT%\stream-server.exe"
set "TASK=MCStreamSrv"
set "WLOG=%ROOT%\logs\watchdog.log"
if not exist "%ROOT%\logs" mkdir "%ROOT%\logs" >nul 2>&1

if not exist "%SVEXE%" (
    echo MISSING %SVEXE%
    echo Unpack the whole bundle, then run this script from its own directory.
    exit /b 1
)

rem ---------------------------------------------------------------------------
rem Elevation. Creating a task for another account needs it, and so do the
rem inbound rules below. Without it the task may be created but every launch
rem stays invisible from other machines -- the exact combination that reads as
rem "I did everything and nothing works".
rem ---------------------------------------------------------------------------
set "ELEVATED=1"
net session >nul 2>&1
if errorlevel 1 set "ELEVATED=0"

if "%ELEVATED%"=="0" (
    echo.
    echo WARNING not running as administrator.
    echo   The task may still be created for %USERNAME%, but the inbound
    echo   firewall rules cannot be added from here, so a browser on another
    echo   machine will time out until somebody runs, in an *elevated* window:
    echo.
    echo       setup-inbound.cmd
    echo.
    echo   Right-click setup-watchdog.bat -^> "Run as administrator" to do both.
    echo.
)

rem Rules first: they are independent of the task, and a start that is
rem unreachable from the network has no other visible symptom.
rem Kept silent here -- the verdict is echoed below, in one line, in a window
rem someone is actually looking at.
if not exist "%ROOT%\open-inbound.cmd" goto :inbound_done
if "%ELEVATED%"=="0" goto :inbound_done
call "%ROOT%\open-inbound.cmd" >nul 2>&1
if errorlevel 2 (
    echo [inbound] netsh refused the rules -- a third-party firewall or a policy
    echo           owns the profile; allow stream-server.exe there as well.
    goto :inbound_done
)
if errorlevel 1 (
    echo [inbound] rules still missing after the attempt; see the netsh output
    echo           by running setup-inbound.cmd directly.
    goto :inbound_done
)
echo [inbound] 8080/TCP and ICMP echo allowed on all profiles.
:inbound_done

rem ---------------------------------------------------------------------------
rem The task runs in the session that owns the desktop -- capture and SendInput
rem are per-session, and a process in session 0 gets an empty desktop -- so the
rem principal is the *console* user, not necessarily whoever runs this script.
rem
rem NO PASSWORD. None is asked for, none is stored, and nothing in the SAM
rem database is touched. The task is registered with
rem LogonType=Interactive ("run only when user is logged on"), which reuses the
rem token that the console session already holds instead of logging anybody in.
rem
rem That is the same session behaviour the old `schtasks /RU ... /RP ... /IT`
rem line had, minus the one thing that hurt: /RP made every trigger do a batch
rem logon, so Windows stored the password and, by default
rem (LimitBlankPasswordUse=1), refused the whole thing on an account without
rem one -- which is why this script used to end in "give the account a
rem password, then run me again".
rem
rem Optional override, rarely needed: setup-watchdog.bat <account>
rem ---------------------------------------------------------------------------
set "ACCOUNT=%~1"

set "PSS=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
if not exist "%ROOT%\setup-task.ps1" (
    echo MISSING %ROOT%\setup-task.ps1
    echo Unpack the whole bundle, then run this script from its own directory.
    exit /b 1
)

echo.
echo -- creating task %TASK% (no password needed) --
if defined ACCOUNT (
    "%PSS%" -NoProfile -ExecutionPolicy Bypass -File "%ROOT%\setup-task.ps1" -Root "%ROOT%" -TaskName "%TASK%" -Account "%ACCOUNT%" -Start
) else (
    "%PSS%" -NoProfile -ExecutionPolicy Bypass -File "%ROOT%\setup-task.ps1" -Root "%ROOT%" -TaskName "%TASK%" -Start
)
rem Exit codes from setup-task.ps1: 1 missing bundle, 2 registration refused,
rem 3 definition read back wrong, 5 created but not started.
if errorlevel 5 (
    echo START_FAILED
    echo   The task was created but could not be started. Try by hand:
    echo     schtasks /Run /TN %TASK%
    goto :summary
)
if errorlevel 1 (
    echo CREATE_FAILED
    exit /b 1
)

rem ---------------------------------------------------------------------------
rem The task has already been started by setup-task.ps1 (-Start). All that is
rem left is to answer the question the operator actually has: is it serving?
rem The previous version ended at "task created", the window closed with it, and
rem the next thing anybody noticed was a page that would not load -- a minute
rem later or a day later, with no way to tell which of the four usual causes it
rem was. The probe is loopback only: it answers "is the server up", which is the
rem part this script is responsible for.
rem ---------------------------------------------------------------------------
echo.
echo -- waiting up to 60 s for 127.0.0.1:8080 --

set /a TRY=0
:wait_loop
set /a TRY+=1
"%SystemRoot%\System32\timeout.exe" /t 5 /nobreak >nul 2>&1
call :probe
if "%CODE%"=="200" goto :wait_ok
if %TRY% lss 12 goto :wait_loop
goto :wait_fail

:wait_ok
echo   [ok] stream-server answered on 127.0.0.1:8080 after %TRY% probe^(s^).
echo        Page: https://^<this-machine-ip^>:8080/
if "%ELEVATED%"=="0" (
    echo        Inbound rules were NOT added -- not elevated. Another machine
    echo        will time out until setup-inbound.cmd runs as administrator.
)
goto :summary

:wait_fail
echo   [fail] nothing listening on 127.0.0.1:8080 after 12 probes (about 60 s).
echo          Read the two logs for the real reason:
echo            type "%WLOG%"
echo            type "%ROOT%\logs\stream-server.log"
echo          Or run collect-launch-facts.cmd and send cloud-stream-facts.txt.
goto :summary

:summary
echo.
echo -- next steps --
echo   * the task fires at logon and every minute; a crash heals within 60 s
echo   * it runs as the console user, with no password stored anywhere
echo   * resolution / fps / bitrate live in stream-config.toml, not in the task
echo   * hand-run one copy: stream-server.exe --run  [--fps N to override for this run]
echo   * firewall, port and session facts: collect-launch-facts.cmd
echo   * re-open the port by hand:            setup-inbound.cmd
echo.
rem Without this the verdict scrolls away with the window: the script is mostly
rem started by double-click, and "task created" used to be the last thing anyone
rem could read. pause returns immediately when stdin is not a console, so an
rem automated call (ssh, script) does not block here.
echo Press any key to close this window.
pause >nul
exit /b 0

:probe
rem Loopback bypasses the firewall, so both schemes have to be tried: the
rem bundle ships cert.pem, but a bundle built without one serves plain HTTP.
rem
rem -f and the exit code, not -w "%%{http_code}": a percent-brace format string
rem inside a cmd block gets eaten by variable expansion and curl then reports
rem "no URL specified" -- the probe would read as "still not listening".
rem
rem --noproxy "*": an http_proxy in the environment (common on company images)
rem routes the loopback request to that proxy, which answers 502 for a port it
rem cannot reach, and a healthy server gets reported as dead.
rem
rem /api/status, not /api/v1/health: #101 put the health report behind the
rem password, so probing it here would print "[fail] nothing listening" on a
rem machine that set a password and is running perfectly. status stays open on
rem purpose (the deploy wrapper uses it) and answers the only question this
rem check asks: is anything on that port.
"%SystemRoot%\System32\curl.exe" -sk -m 3 -f --noproxy "*" -o NUL https://127.0.0.1:8080/api/status >nul 2>&1
if not errorlevel 1 (
    set "CODE=200"
    exit /b 0
)
"%SystemRoot%\System32\curl.exe" -sk -m 3 -f --noproxy "*" -o NUL http://127.0.0.1:8080/api/status >nul 2>&1
if not errorlevel 1 (
    set "CODE=200"
    exit /b 0
)
set "CODE=000"
exit /b 0
