@echo off
chcp 65001 >nul
setlocal
echo ===== SESSIONS =====
query session
echo ===== VIDEO CONTROLLER =====
wmic path Win32_VideoController get Name,CurrentHorizontalResolution,CurrentVerticalResolution,VideoModeDescription /format:list
echo ===== TASK DETAIL =====
schtasks /query /TN MCStreamSrv /V /FO LIST
echo ===== PROCESSES OF INTEREST =====
tasklist /FI "IMAGENAME eq python.exe" /V
tasklist /FI "IMAGENAME eq java.exe" /V
echo ===== DONE =====
