@echo off
cd /d C:\dev\stream
rem #62 diag: target 60fps. --dump-frame dropped (BMP every 150 frames was pure
rem diagnostic I/O and gets scanned by Defender). Log path stays fixed.
C:\dev\stream\stream-server.exe --host 0.0.0.0 --port 8080 --width 1920 --height 1080 --fps 60 --bitrate 2500 --dist C:\dev\selkies-web --app webrtc --tls-cert C:\dev\cert.pem --tls-key C:\dev\key.pem --log C:\dev\stream\logs\stream-server.log --no-hdr
