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

if not exist "%VBS%" (
    echo MISSING %VBS%
    echo Unpack the whole bundle, then run this script from its own directory.
    exit /b 1
)

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
