@echo off
cd /d C:\dev\stream
rem #65/#66: 60fps target; capture runs on its own thread; a client resolution
rem larger than the native display is clamped (no upscale); --repeat-every 1
rem holds the full cadence on a static screen. --dump-frame stays off (BMP every
rem 150 frames was pure diagnostic I/O and gets scanned by Defender).
C:\dev\stream\stream-server.exe --host 0.0.0.0 --port 8080 --width 1920 --height 1080 --fps 60 --bitrate 2500 --dist C:\dev\selkies-web --app webrtc --tls-cert C:\dev\cert.pem --tls-key C:\dev\key.pem --log C:\dev\stream\logs\stream-server.log --no-hdr --repeat-every 1
