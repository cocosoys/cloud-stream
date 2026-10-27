@echo off
echo === HOST ===
hostname
echo === TIME ===
echo %DATE% %TIME%
echo === PORT_8080 ===
netstat -ano | findstr :8080
echo === PROCESSES ===
tasklist | findstr /i "python.exe java.exe stream-server.exe"
echo === CDEV_LISTING ===
dir C:\dev /b
echo === CERT_FILES ===
dir C:\dev\*.pem /b 2>nul
echo === DIST_LISTING ===
dir C:\dev\selkies-web /b 2>nul
echo === TASK_MCSTREAMSRV ===
schtasks /query /tn MCStreamSrv 2>nul
echo === TASK_BENCHCAPTURE ===
schtasks /query /tn BenchCapture 2>nul
echo === TASK_MCCLOUDSTART ===
schtasks /query /tn MCCloudStart 2>nul
echo === SSHD_STATE ===
sc query sshd | findstr STATE
echo === MEMORY_MB ===
powershell -NoProfile -Command "$os=Get-CimInstance Win32_OperatingSystem; '{0} MB free / {1} MB total' -f [math]::Round($os.FreePhysicalMemory/1024),[math]::Round($os.TotalVisibleMemorySize/1024)"
echo === DONE ===
