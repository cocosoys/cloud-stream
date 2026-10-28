@echo off
rem ---------------------------------------------------------------------------
rem Launcher for stream-server.exe, run by the scheduled task MCStreamSrv.
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
setlocal
cd /d C:\dev\stream

tasklist /FI "IMAGENAME eq stream-server.exe" 2>nul | find /I "stream-server.exe" >nul
if not errorlevel 1 (
    echo [%date% %time%] already running, nothing to do>> C:\dev\stream\logs\watchdog.log
    exit /b 0
)

echo [%date% %time%] starting stream-server.exe>> C:\dev\stream\logs\watchdog.log

rem #65/#66: 60fps target; capture runs on its own thread; a client resolution
rem larger than the native display is clamped (no upscale); --repeat-every 1
rem holds the full cadence on a static screen. --dump-frame stays off (BMP every
rem 150 frames was pure diagnostic I/O and gets scanned by Defender).
C:\dev\stream\stream-server.exe --host 0.0.0.0 --port 8080 --width 1920 --height 1080 --fps 60 --bitrate 2500 --dist C:\dev\selkies-web --app webrtc --tls-cert C:\dev\cert.pem --tls-key C:\dev\key.pem --log C:\dev\stream\logs\stream-server.log --no-hdr --repeat-every 1 --upload-dir C:\Users\Administrator\Desktop

echo [%date% %time%] stream-server.exe exited with %errorlevel%>> C:\dev\stream\logs\watchdog.log
