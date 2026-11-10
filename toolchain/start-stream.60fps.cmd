@echo off
rem Manual 60 fps launcher, for debugging without the watchdog task.
rem
rem Same portability rule as start-stream.cmd: every path comes from %~dp0, so
rem the bundle works wherever it was unpacked. Keep the two files in sync -- they
rem carry the same command line, and a hand-run test that passes different flags
rem than the watchdog is not a test of the watchdog.
setlocal EnableExtensions

set "ROOT=%~dp0"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"
if not exist "%ROOT%\logs" mkdir "%ROOT%\logs" >nul 2>&1

rem Same split as start-stream.cmd: tunables live in stream-config.toml, and only
rem what follows the deployment location is passed here. Keep the two in sync --
rem a hand-run test that passes different flags than the watchdog is not a test
rem of the watchdog.
if not exist "%ROOT%\stream-config.toml" (
    echo [error] MISSING %ROOT%\stream-config.toml -- refusing to run on silent defaults
    exit /b 1
)
set "ARGS=--config "%ROOT%\stream-config.toml""
set "ARGS=%ARGS% --dist "%ROOT%\selkies-web" --log "%ROOT%\logs\stream-server.log""
if not exist "%ROOT%\cert.pem" goto :no_tls
if not exist "%ROOT%\key.pem" goto :no_tls
set "ARGS=%ARGS% --tls-cert "%ROOT%\cert.pem" --tls-key "%ROOT%\key.pem""
goto :run

:no_tls
echo [watchdog] no cert.pem/key.pem in %ROOT% -- plain HTTP, clipboard off

:run

rem Same preflight as start-stream.cmd. A hand-run test that skips the firewall
rem check reproduces the symptom it was called in to explain: server up,
rem loopback fine, browser on another machine times out.
if not exist "%ROOT%\open-inbound.cmd" goto :pf_done
call "%ROOT%\open-inbound.cmd"
if errorlevel 2 goto :pf_refused
if errorlevel 1 goto :pf_blocked
goto :pf_done
:pf_refused
echo  [preflight] netsh refused the inbound rules -- a third-party firewall or a policy owns the profile
goto :pf_done
:pf_blocked
echo  [preflight] inbound 8080/ICMP/WebRTC still blocked -- run setup-inbound.cmd as administrator once
:pf_done

rem #101: same file, same rule as start-stream.cmd -- the long explanation is
rem there. Kept identical on purpose: a hand-run test that passes different
rem flags than the watchdog is not a test of the watchdog.
if not exist "%ROOT%\auth-token.txt" goto :no_auth
set "AUTH="
for /f "usebackq delims=" %%T in ("%ROOT%\auth-token.txt") do if not defined AUTH set "AUTH=%%T"
if not defined AUTH goto :no_auth
set "ARGS=%ARGS% --auth-token "%AUTH%""
echo  [auth] token enabled from auth-token.txt -- browsers must append it to the page URL
goto :probe

:no_auth
echo  [auth] no auth-token.txt -- /api/* is open to anyone who can reach the port

:probe

if exist "%ROOT%\probe-listen.cmd" start "cloud-stream probe" /b "%ROOT%\probe-listen.cmd"
"%ROOT%\stream-server.exe" %ARGS%
