@echo off
chcp 65001 >nul
setlocal
echo ===== HOST =====
hostname
whoami
echo ===== CPU =====
powershell -NoProfile -Command "(Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors"
echo ===== MEMORY =====
powershell -NoProfile -Command "$o=Get-CimInstance Win32_OperatingSystem; 'total_mb=' + [math]::Round($o.TotalVisibleMemorySize/1024) + ' free_mb=' + [math]::Round($o.FreePhysicalMemory/1024)"
echo ===== DISPLAY =====
powershell -NoProfile -Command "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Screen]::AllScreens | ForEach-Object { $_.Bounds.Width.ToString() + 'x' + $_.Bounds.Height.ToString() + ' primary=' + $_.Primary }"
echo ===== PYTHON PROC =====
tasklist /FI "IMAGENAME eq python.exe"
tasklist /FI "IMAGENAME eq pythonw.exe"
echo ===== JAVA / MC PROC =====
tasklist /FI "IMAGENAME eq java.exe"
echo ===== VC RUNTIME =====
if exist C:\Windows\System32\vcruntime140.dll (echo vcruntime140.dll OK) else (echo vcruntime140.dll MISSING)
if exist C:\Windows\System32\vcruntime140_1.dll (echo vcruntime140_1.dll OK) else (echo vcruntime140_1.dll MISSING)
if exist C:\Windows\System32\msvcp140.dll (echo msvcp140.dll OK) else (echo msvcp140.dll MISSING)
echo ===== STREAM TASK =====
schtasks /query /TN MCStreamSrv /FO LIST 2>nul
echo ===== DONE =====
