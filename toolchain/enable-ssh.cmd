@echo off
rem Double-click entry for enable-ssh.ps1.
rem
rem Two reasons this file exists instead of just handing out the .ps1:
rem   1. Windows opens .ps1 in Notepad on double-click, and the operator of a
rem      machine with no ssh is by definition not going to run it from a shell.
rem   2. The execution policy on a fresh host is Restricted, so -Bypass has to
rem      be on the command line.
rem
rem Arguments pass straight through, e.g.:
rem     enable-ssh.cmd -Check
rem     enable-ssh.cmd -ClientAddress 192.168.59.10 -KeepPassword
setlocal
set "PS=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
"%PS%" -NoProfile -ExecutionPolicy Bypass -File "%~dp0enable-ssh.ps1" %*

rem When this window was not elevated the script has already handed the work to
rem an elevated window and exited; pausing here would leave a second window
rem saying nothing. Only stop when this window actually did the work -- which
rem includes -Check, whose whole value is the output.
echo %* | findstr /I /C:"-Check" >nul
if not errorlevel 1 goto :stop
net session >nul 2>&1
if errorlevel 1 goto :done

:stop
echo.
echo Press any key to close this window.
pause >nul
:done
endlocal
