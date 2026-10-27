@echo off
chcp 65001 >nul
setlocal
if exist C:\dev\bench\bench-cpu.txt del C:\dev\bench\bench-cpu.txt
if exist C:\dev\bench\bench-result.txt del C:\dev\bench\bench-result.txt
if exist C:\dev\bench\bench-result.err.txt del C:\dev\bench\bench-result.err.txt
echo ===== RUN BENCH TASK =====
schtasks /run /tn BenchCapture
echo run_exit=%errorlevel%
powershell -NoProfile -Command "'triggered_at=" + (Get-Date -Format 'HH:mm:ss.fff')'
echo ===== DONE =====
