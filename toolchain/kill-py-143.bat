@echo off
chcp 65001 >nul
setlocal
echo ===== BEFORE PID 7532 =====
tasklist /FI "PID eq 7532"
echo ===== STOP-PROCESS =====
powershell -NoProfile -Command "try { Stop-Process -Id 7532 -Force -ErrorAction Stop; 'stop_process_ok' } catch { 'stop_process_fail: ' + $_.Exception.Message }"
echo ===== AFTER PID 7532 =====
tasklist /FI "PID eq 7532"
echo ===== ANY PYTHON NOW =====
tasklist /FI "IMAGENAME eq python.exe"
echo ===== DONE =====
