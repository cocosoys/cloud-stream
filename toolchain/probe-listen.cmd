@echo off
rem ---------------------------------------------------------------------------
rem Post-start probe: did the exe actually end up listening on 8080?
rem
rem Started in the background by start-stream.cmd, because that script blocks on
rem stream-server.exe (that is what keeps the watchdog task in the Running state
rem and makes the per-minute trigger a no-op). It writes exactly one PREFLIGHT
rem line into logs\watchdog.log -- the file the operator already opens -- so a
rem start that "does nothing" leaves a timestamped verdict instead of silence.
rem
rem Loopback only. This probe cannot tell you whether *another machine* can
rem reach the port; it answers the narrower and more useful question "is the
rem server up at all". When the answer is yes and the page still does not load
rem elsewhere, the problem is the firewall profile, not the build.
rem
rem Keep pure ASCII: cmd reads this file byte by byte in the OEM code page.
rem ---------------------------------------------------------------------------
setlocal EnableExtensions EnableDelayedExpansion

set "ROOT=%~dp0"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"
if not exist "%ROOT%\logs" mkdir "%ROOT%\logs" >nul 2>&1
set "WLOG=%ROOT%\logs\watchdog.log"
set "CURL=%SystemRoot%\System32\curl.exe"
set "FIND=%SystemRoot%\System32\find.exe"
set "LOG=%ROOT%\logs\stream-server.log"

rem 6 probes x 5 s = 30 s. A cold start on a slow VM spends a few seconds in
rem frontend indexing before the listener is up; 30 s covers that without
rem leaving a process behind for long.
for /l %%I in (1,1,6) do (
    "%SystemRoot%\System32\timeout.exe" /t 5 /nobreak >nul 2>&1
    call :probe
    if "!CODE!"=="200" (
        rem !time! not %time%: cmd expands %time% once when it parses the whole
        rem for block, so the verdict would carry the timestamp from before the
        rem first 5 s wait. Delayed expansion is evaluated per iteration.
        echo [!date! !time!] PREFLIGHT listening on 8080 after %%I probe^(s^): local status 200 >> "%WLOG%"
        call :hint
        exit /b 0
    )
)

echo [%date% %time%] PREFLIGHT NOT listening on 8080 after 6 probes -- the page will not load >> "%WLOG%"
if exist "%LOG%" (
    rem The three causes worth naming, in the order they are likely.
    "%FIND%" /I "Address already in use" "%LOG%" >nul 2>&1
    if not errorlevel 1 echo [%date% %time%] PREFLIGHT   8080 is taken by another process -- netstat -ano ^| find ":8080" >> "%WLOG%"
    "%FIND%" /I "panicked" "%LOG%" >nul 2>&1
    if not errorlevel 1 echo [%date% %time%] PREFLIGHT   the exe panicked -- see the tail of logs\stream-server.log >> "%WLOG%"
) else (
    echo [%date% %time%] PREFLIGHT   logs\stream-server.log was never created -- the exe died in the loader; install VC++ 2015-2022 x64 >> "%WLOG%"
)
echo [%date% %time%] PREFLIGHT   next step: run collect-launch-facts.cmd in this folder and send cloud-stream-facts.txt >> "%WLOG%"
exit /b 1

:probe
rem Loopback bypasses the firewall, so both schemes have to be tried: the
rem bundle ships cert.pem, but a bundle built without one serves plain HTTP.
rem
rem -f and the exit code, not -w "%%{http_code}": a percent-brace format string
rem inside a parenthesised cmd block gets eaten by variable expansion, and curl
rem then reports "no URL specified". The exit code has no such problem: 0 means
rem 2xx, 22 means 4xx/5xx, 7 means nothing is listening.
rem
rem --noproxy "*": an http_proxy in the environment -- standard on company
rem images, and present on anything that also runs a dev proxy -- sends the
rem loopback request to that proxy, which answers 502 for a port it cannot
rem reach. The probe would then announce a dead server on a perfectly healthy
rem machine.
rem
rem /api/status, not /api/v1/health: #101 put the health report behind the
rem password (it names every connected client), while status stays open because
rem the deploy wrapper probes it too. The probe's question is "is anything
rem answering on this port", not "is the pipeline healthy" -- so the open route
rem is both the right question and the one that keeps working after somebody
rem sets a password. Using health here would turn every password-protected
rem machine into a "PREFLIGHT NOT listening" false alarm.
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

:hint
rem The success case still has a trap: loopback bypasses the firewall, so a
rem remote browser can fail on a machine that passes this probe. Say it once,
rem in the same line, instead of letting it become a second support round.
echo [%date% %time%] PREFLIGHT   loopback works. If another machine still cannot open the page, the inbound rule or the network profile is the cause: run setup-inbound.cmd as administrator >> "%WLOG%"
exit /b 0
