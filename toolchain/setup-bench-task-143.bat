@echo off
chcp 65001 >nul
setlocal
echo ===== DISABLE OLD STREAM TASK =====
schtasks /change /tn MCStreamSrv /disable
echo disable_old_exit=%errorlevel%
echo ===== CREATE BENCH INTERACTIVE TASK =====
schtasks /create /tn BenchCapture /tr "C:\dev\bench\run-bench.cmd" /sc once /st 00:00 /it /ru Administrator /rp Admin@2026 /f
echo create_exit=%errorlevel%
echo ===== QUERY BENCH TASK =====
schtasks /query /tn BenchCapture /V /FO LIST
echo ===== MEMORY NOW =====
powershell -NoProfile -Command "$o=Get-CimInstance Win32_OperatingSystem; 'total_mb=' + [math]::Round($o.TotalVisibleMemorySize/1024) + ' free_mb=' + [math]::Round($o.FreePhysicalMemory/1024)"
echo ===== PROCESSES NOW =====
tasklist /FI "IMAGENAME eq python.exe"
tasklist /FI "IMAGENAME eq java.exe"
echo ===== DONE =====
