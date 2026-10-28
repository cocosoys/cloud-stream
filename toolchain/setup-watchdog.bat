@echo off
rem #81: turn MCStreamSrv from a one-shot launcher into a watchdog.
rem
rem Before: /SC ONCE, "repeat disabled" — the server ran until it died and then
rem stayed dead until somebody typed `schtasks /Run`.
rem
rem After: every minute. Task Scheduler's default policy is "do not start a new
rem instance", so while the server is alive the trigger does nothing; when it
rem dies the task completes and the next minute brings it back.
rem
rem wscript + run-hidden.vbs instead of cmd.exe directly: the trigger fires 1440
rem times a day, and a console flashing on the game's desktop would steal focus.
schtasks /Create /TN MCStreamSrv /TR "wscript.exe C:\dev\stream\run-hidden.vbs" /SC MINUTE /MO 1 /RU Administrator /RP Admin@2026 /IT /F
if errorlevel 1 (
    echo CREATE_FAILED
    exit /b 1
)
schtasks /Query /TN MCStreamSrv /FO LIST
