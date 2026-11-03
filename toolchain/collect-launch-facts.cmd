@echo off
rem ---------------------------------------------------------------------------
rem Collect the facts needed when the bundle will not start on a new machine.
rem
rem Double-click it. It writes cloud-stream-facts.txt next to this script and a
rem copy on the Desktop. Read-only: no service is started, nothing installed,
rem nothing deleted. It deliberately does not run stream-server.exe -- a second
rem copy would grab the screen and fight an instance that already works.
rem
rem Why a script: on a fresh Windows install the three usual causes all fail
rem *silently*, and each one looks exactly like "the program will not start".
rem   * blank password     -> schtasks refuses /RP "" (blank-password accounts
rem                           are barred from batch logon), so setup-watchdog
rem                           exits without creating anything, and the window
rem                           closes before the message can be read.
rem   * missing VC++       -> the exe dies in the loader, before it can write a
rem                           single line to its own log.
rem   * zip not extracted  -> it ran from Explorer's temp folder, so there is no
rem                           selkies-web next to it and no logs folder either.
rem The output below separates those three in one pass.
rem
rem Keep this file pure ASCII: cmd reads it byte by byte in the OEM code page,
rem and a UTF-8 em dash swallows the next quote (that bug cost an evening).
rem ---------------------------------------------------------------------------
setlocal EnableExtensions
set "ROOT=%~dp0"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"
set "OUT=%~dp0cloud-stream-facts.txt"
rem Full path on purpose, same lesson as find.exe: on a machine where PATH has
rem a pwsh 7 or a conda "powershell" first, the bare name runs the wrong binary
rem (and can stop on a module-trust prompt nobody is watching).
set "PS=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
rem A machine that also has PowerShell 7 installed often has PSModulePath
rem pointing at pwsh's module folder. 5.1 then loads those .ps1xml files, finds
rem them signed by an untrusted publisher, and stops on an interactive prompt --
rem which on a headless controlled box looks like "the script hangs". Scoped to
rem this script (setlocal), so nothing outside is affected.
set "PSModulePath=%SystemRoot%\System32\WindowsPowerShell\v1.0\Modules"

rem 65001 so the file reads the same on whatever machine inspects it later.
chcp 65001 >nul 2>&1

> "%OUT%" echo cloud-stream launch facts
>> "%OUT%" echo script dir   %ROOT%
>> "%OUT%" echo collected    %date% %time%
>> "%OUT%" echo.

>> "%OUT%" echo ==== 1. windows and session ====
>> "%OUT%" echo arch         %PROCESSOR_ARCHITECTURE%
>> "%OUT%" ver
>> "%OUT%" echo.
>> "%OUT%" echo -- quser (the desktop session must be console/1, not 0) --
>> "%OUT%" quser 2>&1
>> "%OUT%" echo.
>> "%OUT%" echo -- whoami --
>> "%OUT%" whoami /user 2>&1
>> "%OUT%" echo.

>> "%OUT%" echo ==== 2. account policy ====
>> "%OUT%" echo -- LimitBlankPasswordUse (1 means a blank password cannot run a task) --
>> "%OUT%" "%PS%" -NoProfile -Command "try { $v = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Lsa' -Name LimitBlankPasswordUse -ErrorAction Stop).LimitBlankPasswordUse; if ($v -eq 1) { 'LimitBlankPasswordUse = 1  (blank password blocked for batch logon)' } else { 'LimitBlankPasswordUse = ' + $v + '  (blank password allowed)' } } catch { 'LimitBlankPasswordUse not set (default 1: blank password blocked for batch logon)' }" 2>&1
>> "%OUT%" echo.
>> "%OUT%" echo -- net user for this account --
>> "%OUT%" net user "%USERNAME%" 2>&1
>> "%OUT%" echo.

>> "%OUT%" echo ==== 3. bundle content ====
>> "%OUT%" dir /a "%ROOT%" 2>&1
>> "%OUT%" echo.
>> "%OUT%" echo -- files that must exist --
for %%F in (stream-server.exe ViGEmClient.dll start-stream.cmd run-hidden.vbs setup-watchdog.bat) do (
    if exist "%ROOT%\%%F" (
        >> "%OUT%" echo OK      %%F
    ) else (
        >> "%OUT%" echo MISSING %%F
    )
)
if exist "%ROOT%\selkies-web\index.html" (
    >> "%OUT%" echo OK      selkies-web\index.html
) else (
    >> "%OUT%" echo MISSING selkies-web\index.html
)
if exist "%ROOT%\cert.pem" (
    >> "%OUT%" echo OK      cert.pem
) else (
    >> "%OUT%" echo MISSING cert.pem -- plain HTTP, clipboard will not work
)
>> "%OUT%" echo.
>> "%OUT%" echo -- exe size (a truncated copy is the classic bad unzip) --
for %%F in ("%ROOT%\stream-server.exe") do >> "%OUT%" echo %%~zF bytes
>> "%OUT%" echo.

>> "%OUT%" echo ==== 4. loader prerequisites ====
if exist "%SystemRoot%\System32\vcruntime140.dll" (
    >> "%OUT%" echo OK      %SystemRoot%\System32\vcruntime140.dll
) else (
    >> "%OUT%" echo MISSING vcruntime140.dll -- install VC++ 2015-2022 x64
)
>> "%OUT%" echo.
>> "%OUT%" echo -- the exe is NOT started from here, on purpose --
>> "%OUT%" echo It would grab the screen and port 8080, and a second copy would
>> "%OUT%" echo fight an instance that is already working. To see the real error,
>> "%OUT%" echo open a cmd window in this folder and run:  stream-server.exe
>> "%OUT%" echo A loader failure says "VCRUNTIME140.dll was not found" and returns
>> "%OUT%" echo 0xC0000135 -- which appears in logs below as "exited with
>> "%OUT%" echo -1073741515".
>> "%OUT%" echo.

>> "%OUT%" echo ==== 5. runtime state ====
>> "%OUT%" echo -- process --
>> "%OUT%" tasklist /FI "IMAGENAME eq stream-server.exe" 2>&1
>> "%OUT%" echo.
>> "%OUT%" echo -- port 8080 (empty here = nothing is listening) --
rem Redirection goes on the LAST command of the pipe. Writing
rem ">> file netstat | find" sends all of netstat to the file and leaves the
rem filter with an empty stream -- which looks like "no filter at all".
netstat -ano 2>nul | "%SystemRoot%\System32\find.exe" ":8080" >> "%OUT%"
>> "%OUT%" echo.
>> "%OUT%" echo -- scheduled task MCStreamSrv --
>> "%OUT%" schtasks /Query /TN MCStreamSrv /FO LIST /V 2>&1
>> "%OUT%" echo.
>> "%OUT%" echo -- last run result (0x41301 = still running, 0xFFFFFFFF = never) --
>> "%OUT%" "%PS%" -NoProfile -Command "try { $i = Get-ScheduledTaskInfo -TaskName MCStreamSrv -ErrorAction Stop; 'LastRunTime    ' + $i.LastRunTime; 'LastTaskResult 0x{0:X8}' -f $i.LastTaskResult } catch { 'no task MCStreamSrv: ' + $_.Exception.Message }" 2>&1
>> "%OUT%" echo.

>> "%OUT%" echo ==== 6. logs (tail 40 each) ====
if exist "%ROOT%\logs\watchdog.log" (
    >> "%OUT%" echo -- logs\watchdog.log --
    >> "%OUT%" "%PS%" -NoProfile -Command "Get-Content -LiteralPath '%ROOT%\logs\watchdog.log' -Tail 40 -Encoding UTF8" 2>&1
) else (
    >> "%OUT%" echo no watchdog.log -- start-stream.cmd never ran from this folder
)
>> "%OUT%" echo.
if exist "%ROOT%\logs\stream-server.log" (
    >> "%OUT%" echo -- logs\stream-server.log --
    >> "%OUT%" "%PS%" -NoProfile -Command "Get-Content -LiteralPath '%ROOT%\logs\stream-server.log' -Tail 40 -Encoding UTF8" 2>&1
) else (
    >> "%OUT%" echo no stream-server.log -- the exe never got far enough to open it
)
>> "%OUT%" echo.

>> "%OUT%" echo ==== 7. task scheduler events (last 8) ====
>> "%OUT%" "%PS%" -NoProfile -Command "try { Get-WinEvent -LogName 'Microsoft-Windows-TaskScheduler/Operational' -MaxEvents 8 -ErrorAction Stop | ForEach-Object { $_.TimeCreated.ToString('yyyy-MM-dd HH:mm:ss') + '  ' + $_.Id + '  ' + ($_.Message -replace '\s+', ' ') } } catch { 'task scheduler log: ' + $_.Exception.Message }" 2>&1
>> "%OUT%" echo.

>> "%OUT%" echo ==== 8. defender ====
>> "%OUT%" "%PS%" -NoProfile -Command "try { $d = Get-MpComputerStatus -ErrorAction Stop; 'RealTimeProtection ' + $d.RealTimeProtectionEnabled; Get-MpThreatDetection -ErrorAction SilentlyContinue | Select-Object -Last 3 | ForEach-Object { 'threat ' + $_.InitialDetectionTime.ToString() + ' id ' + $_.ThreatID } } catch { 'defender query failed: ' + $_.Exception.Message }" 2>&1
>> "%OUT%" echo.

>> "%OUT%" echo ==== done ====

echo.
echo Wrote %OUT%
if exist "%USERPROFILE%\Desktop" copy /y "%OUT%" "%USERPROFILE%\Desktop\cloud-stream-facts.txt" >nul 2>&1
echo A copy is on the Desktop as cloud-stream-facts.txt
echo Send that file back; it answers "why did it not start" without guessing.
echo.
pause
