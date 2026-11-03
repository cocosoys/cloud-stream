@echo off
rem ---------------------------------------------------------------------------
rem Create the MCStreamSrv watchdog task for THIS directory.
rem
rem Portable: the task action is "wscript.exe <this dir>\run-hidden.vbs", built
rem from %~dp0, so the bundle can be unpacked anywhere. The account and password
rem are arguments (or prompts) instead of literals -- a password committed to the
rem repo is both a leak and a lie, because every machine has a different one.
rem
rem Usage, from the directory the bundle was unpacked into:
rem
rem   setup-watchdog.bat                     <- prompts for account and password
rem   setup-watchdog.bat Administrator S3cr3t
rem
rem #81: turn the task from a one-shot launcher into a watchdog.
rem
rem Before: /SC ONCE, "repeat disabled" -- the server ran until it died and then
rem stayed dead until somebody typed `schtasks /Run`.
rem
rem After: every minute. Task Scheduler's default policy is "do not start a new
rem instance", so while the server is alive the trigger does nothing; when it
rem dies the task completes and the next minute brings it back.
rem
rem wscript + run-hidden.vbs instead of cmd.exe directly: the trigger fires 1440
rem times a day, and a console flashing on the game's desktop would steal focus.
rem ---------------------------------------------------------------------------
setlocal EnableExtensions

set "ROOT=%~dp0"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"
set "VBS=%ROOT%\run-hidden.vbs"
set "TASK=MCStreamSrv"
set "WLOG=%ROOT%\logs\watchdog.log"
if not exist "%ROOT%\logs" mkdir "%ROOT%\logs" >nul 2>&1

if not exist "%VBS%" (
    echo MISSING %VBS%
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

set "ACCOUNT=%~1"
if not defined ACCOUNT (
    rem The task must run in the session that has the desktop (/IT), so the
    rem default is whoever is running this script, not a hardcoded Administrator.
    set "ACCOUNT=%USERNAME%"
)
set "PASSWORD=%~2"
if not defined PASSWORD set /p "PASSWORD=Password for %ACCOUNT%: "
if not defined PASSWORD (
    rem A blank password is not "no password needed" -- it is the single most
    rem common reason this script looks dead on a fresh machine. Windows blocks
    rem blank-password accounts from batch logon (LimitBlankPasswordUse=1), so
    rem schtasks would fail with "the password is invalid" even if it got that
    rem far, and the task it created (if any) would never start. Say so out
    rem loud instead of exiting with six characters nobody can read.
    echo NO_PASSWORD
    echo.
    echo A scheduled task cannot run for an account that has no password:
    echo Windows bars blank-password accounts from batch logon.
    echo Two ways forward:
    echo   a^) give %ACCOUNT% a password, then run this script again:
    echo         net user %ACCOUNT% *
    echo   b^) keep the blank password and skip the task: put a shortcut to
    echo      run-hidden.vbs into  shell:startup  instead ^(needs auto logon^).
    echo.
    echo docs\deploy-new-host.md section 3.0 covers both paths.
    exit /b 1
)

rem \" is the documented escape for a quote passed *through* cmd to schtasks: the
rem action string has to stay quoted inside /TR so an unpack path with spaces
rem still works. Writing ""%VBS%"" instead hands schtasks a broken argument.
schtasks /Create /TN %TASK% /TR "wscript.exe \"%VBS%\"" /SC MINUTE /MO 1 /RU "%ACCOUNT%" /RP "%PASSWORD%" /IT /F
if errorlevel 1 (
    echo CREATE_FAILED
    exit /b 1
)
rem Read the action back. If the quoting above ever regresses, the task still
rem gets created -- pointing at a truncated path -- and the symptom is "the task
rem says it ran, but nothing is listening". Catch it here instead of a week later.
schtasks /Query /TN %TASK% /FO LIST /V | "%SystemRoot%\System32\find.exe" /I "run-hidden.vbs" >nul
if errorlevel 1 (
    echo WARNING task action does not mention run-hidden.vbs. Inspect with:
    echo   schtasks /Query /TN %TASK% /FO LIST /V
)

schtasks /Query /TN %TASK% /FO LIST

rem ---------------------------------------------------------------------------
rem Start it now and report, instead of leaving "task created" as the last
rem thing the operator sees. The previous version ended here, the window with
rem it, and the next thing anybody noticed was a page that would not load --
rem a minute later or a day later, with no way to tell which of the four usual
rem causes it was. The probe is loopback only: it answers "is the server up",
rem which is the part this script is responsible for.
rem ---------------------------------------------------------------------------
echo.
echo -- starting the task and waiting up to 60 s --
schtasks /Run /TN %TASK% >nul 2>&1
if errorlevel 1 (
    echo   [fail] schtasks /Run was refused. Try by hand:
    echo          schtasks /Run /TN %TASK%
    goto :summary
)

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
echo   * the task fires every minute; a crash heals within 60 s
echo   * resolution / fps / bitrate live in start-stream.cmd, not in the task
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
"%SystemRoot%\System32\curl.exe" -sk -m 3 -f --noproxy "*" -o NUL https://127.0.0.1:8080/api/v1/health >nul 2>&1
if not errorlevel 1 (
    set "CODE=200"
    exit /b 0
)
"%SystemRoot%\System32\curl.exe" -sk -m 3 -f --noproxy "*" -o NUL http://127.0.0.1:8080/api/v1/health >nul 2>&1
if not errorlevel 1 (
    set "CODE=200"
    exit /b 0
)
set "CODE=000"
exit /b 0
