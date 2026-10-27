@echo off
chcp 65001 >nul
setlocal
echo ===== BEFORE =====
tasklist /FI "IMAGENAME eq python.exe"
tasklist /FI "IMAGENAME eq pythonw.exe"
echo ===== END TASK MCStreamSrv =====
schtasks /end /tn MCStreamSrv
echo end_task_exit=%errorlevel%
echo ===== KILL python 7532 =====
taskkill /PID 7532 /F
echo kill_exit=%errorlevel%
echo ===== AFTER =====
tasklist /FI "IMAGENAME eq python.exe"
tasklist /FI "IMAGENAME eq pythonw.exe"
echo ===== JAVA STILL INTACT =====
tasklist /FI "IMAGENAME eq java.exe"
echo ===== DONE =====
