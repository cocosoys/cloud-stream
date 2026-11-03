@echo off
rem ---------------------------------------------------------------------------
rem Launcher for stream-server.exe, run by the scheduled task MCStreamSrv.
rem
rem PORTABLE ON PURPOSE. Every path is derived from %~dp0, so the whole runtime
rem directory can be unpacked anywhere (D:\stream, C:\dev\stream, a USB disk)
rem and this script keeps working. Earlier revisions hardcoded C:\dev\stream and
rem C:\dev\selkies-web; unpacking the bundle one level off produced a task that
rem started, served 404s and logged nothing wrong.
rem
rem Layout expected next to this file (that is exactly what the deploy bundle
rem target\output\cloud-stream carries):
rem
rem   stream-server.exe  ViGEmClient.dll  cert.pem  key.pem
rem   selkies-web\  logs\  start-stream.cmd  run-hidden.vbs  setup-watchdog.bat
rem
rem #81 (service self-recovery): the task fires every minute and relies on
rem Task Scheduler's default "do not start a new instance" policy. While the
rem server runs this script blocks, so the task stays Running and later triggers
rem are ignored; when the server dies the script returns, the task completes, and
rem the next minute's trigger restarts it. A crash that used to need a human at
rem the keyboard now heals itself within 60 s.
rem
rem The guard below is for the case the scheduler cannot cover: a server started
rem by hand (or left over from before the task was recreated) while the task is
rem not Running. Without it, the next trigger would try to bind 8080 again and
rem fail with a port error in the log every minute.
rem ---------------------------------------------------------------------------
setlocal EnableExtensions

rem Trailing backslash off: "%ROOT%logs" reads better than "%ROOT%\logs" and the
rem quoted-argument building below then has exactly one pair of quotes per path.
set "ROOT=%~dp0"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"
if not exist "%ROOT%\logs" mkdir "%ROOT%\logs" >nul 2>&1
cd /d "%ROOT%"
if errorlevel 1 (
    echo [%date% %time%] cannot cd to %ROOT%>> "%ROOT%\logs\watchdog.log"
    exit /b 1
)

set "EXE=%ROOT%\stream-server.exe"
set "WLOG=%ROOT%\logs\watchdog.log"

rem find.exe is spelled out with its full path: the watchdog may inherit a PATH
rem where an MSYS/Git find.exe comes first, and that one dies on "/I" -- the guard
rem then reads "not running" every minute and the launcher starts a second copy.
tasklist /FI "IMAGENAME eq stream-server.exe" 2>nul | "%SystemRoot%\System32\find.exe" /I "stream-server.exe" >nul
if not errorlevel 1 (
    echo [%date% %time%] already running, nothing to do>> "%WLOG%"
    exit /b 0
)

if not exist "%EXE%" (
    echo [%date% %time%] MISSING %EXE% -- bundle not unpacked completely>> "%WLOG%"
    exit /b 1
)

echo [%date% %time%] starting stream-server.exe from %ROOT%>> "%WLOG%"

rem #65/#66: 60fps target; capture runs on its own thread; a client resolution
rem larger than the native display is clamped (no upscale); --repeat-every 1
rem holds the full cadence on a static screen. --dump-frame stays off (BMP every
rem 150 frames was pure diagnostic I/O and gets scanned by Defender).
rem
rem TLS is conditional: a bundle built without certificates still has to run
rem (plain HTTP). Clipboard sync needs a secure context, so the absence is
rem recorded in the log instead of silently degrading.
set "ARGS=--host 0.0.0.0 --port 8080 --width 1920 --height 1080 --fps 60 --bitrate 2500"
set "ARGS=%ARGS% --app webrtc --dist "%ROOT%\selkies-web" --log "%ROOT%\logs\stream-server.log""
set "ARGS=%ARGS% --no-hdr --repeat-every 1"

rem Labelled instead of an if/else block: cmd expands %ARGS% once per *block*,
rem so a set inside parentheses would append to the value from before the block.
rem A bundle built without certificates still has to run (plain HTTP); clipboard
rem sync needs a secure context, so the absence is recorded rather than hidden.
if not exist "%ROOT%\cert.pem" goto :no_tls
if not exist "%ROOT%\key.pem" goto :no_tls
set "ARGS=%ARGS% --tls-cert "%ROOT%\cert.pem" --tls-key "%ROOT%\key.pem""
goto :upload_dir

:no_tls
echo [%date% %time%] no cert.pem/key.pem in %ROOT% -- plain HTTP, clipboard off>> "%WLOG%"

:upload_dir

rem Upload root follows the account that runs the game, not a hardcoded
rem C:\Users\Administrator: a new machine has a different interactive user.
set "UPLOAD=%USERPROFILE%\Desktop"
if not exist "%UPLOAD%" set "UPLOAD=%USERPROFILE%"
set "ARGS=%ARGS% --upload-dir "%UPLOAD%""

"%EXE%" %ARGS%
set "CODE=%errorlevel%"
echo [%date% %time%] stream-server.exe exited with %CODE%>> "%WLOG%"
exit /b %CODE%
