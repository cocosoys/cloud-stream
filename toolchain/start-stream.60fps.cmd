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

set "ARGS=--host 0.0.0.0 --port 8080 --width 1920 --height 1080 --fps 60 --bitrate 2500"
set "ARGS=%ARGS% --app webrtc --dist "%ROOT%\selkies-web" --log "%ROOT%\logs\stream-server.log""
set "ARGS=%ARGS% --no-hdr --repeat-every 1"
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
echo  [preflight] inbound 8080/ICMP still blocked -- run setup-inbound.cmd as administrator once
:pf_done

if exist "%ROOT%\probe-listen.cmd" start "cloud-stream probe" /b "%ROOT%\probe-listen.cmd"
"%ROOT%\stream-server.exe" %ARGS%
