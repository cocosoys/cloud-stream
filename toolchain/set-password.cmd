@echo off
rem ---------------------------------------------------------------------------
rem Set the password this machine requires before it streams.
rem
rem Run it, type a password, restart the task. The password lands in
rem auth-token.txt next to this script; start-stream.cmd passes it to the exe
rem as --auth-token, and the server then answers 401 to every /api/ request
rem that does not present it.
rem
rem Why a file and not a flag in start-stream.cmd: that file is ASCII, read byte
rem by byte in the OEM code page. A password with a non-ASCII character would be
rem mangled before the exe saw it. Hence the ASCII-only rule enforced below --
rem it is a cmd boundary, not a security opinion.
rem
rem Why PowerShell writes it: `echo %PW% > file` breaks on & | < > and on a
rem password that is empty, and each of those failure modes is silent.
rem
rem Keep pure ASCII: cmd reads this file byte by byte.
rem ---------------------------------------------------------------------------
setlocal EnableExtensions

set "ROOT=%~dp0"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"
set "TARGET=%ROOT%\auth-token.txt"

echo.
echo -- stream password --
echo.
echo An empty answer REMOVES the password: anyone who can reach port 8080
echo could then drive this machine. Use letters, digits and punctuation only.
echo.
set "PW="
set /p "PW=Password: "

rem The password reaches PowerShell through the environment, not through the
rem command line: a `"` in it would otherwise end the -Command string.
rem
rem One single -Command, not several continuation lines: PowerShell joins
rem multiple -Command arguments with spaces, which breaks the brace blocks.
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -Command "$p=$env:PW; $t='%TARGET%'; if ([string]::IsNullOrWhiteSpace($p)) { Remove-Item -LiteralPath $t -Force -ErrorAction SilentlyContinue; Write-Output 'removed' } elseif ($p -match '[^\x20-\x7E]') { Write-Output 'non-ascii'; exit 2 } else { [System.IO.File]::WriteAllText($t,$p,(New-Object System.Text.UTF8Encoding($false))); Write-Output 'written' }"
set "RESULT=%errorlevel%"

if "%RESULT%"=="2" (
    echo.
    echo FAILED: the password must be printable ASCII. cmd would mangle
    echo anything else before the server ever sees it.
    echo.
    pause
    exit /b 2
)
if not "%RESULT%"=="0" (
    echo.
    echo FAILED to write %TARGET%
    echo.
    pause
    exit /b 1
)

if not exist "%TARGET%" (
    echo.
    echo Password removed. This machine is open to anyone on the LAN again.
    goto :restart
)

echo.
echo Wrote %TARGET%
echo.

:restart
echo Restart the task so the new setting takes effect:
echo    schtasks /End /TN MCStreamSrv
echo    schtasks /Run /TN MCStreamSrv
echo.
echo Then open the page with the token appended (the browser keeps it in a
echo cookie afterwards, so the plain address works from then on):
echo.
echo    https://^<this-ip^>:8080/?token=^<the password^>
echo.
echo A wrong or missing token shows 401 and nothing else -- the server logs
echo one "auth token REQUIRED on /api/*" line at every start.
echo.
pause
exit /b 0
