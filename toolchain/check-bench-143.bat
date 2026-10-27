@echo off
chcp 65001 >nul
setlocal
echo ===== TASK STATUS =====
schtasks /query /tn BenchCapture /V /FO LIST
echo ===== FILES =====
dir C:\dev\bench
echo ===== RESULT CPU =====
if exist C:\dev\bench\bench-cpu.txt (type C:\dev\bench\bench-cpu.txt) else (echo bench-cpu.txt NOT YET)
echo ===== DONE =====
