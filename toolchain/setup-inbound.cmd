@echo off
rem ---------------------------------------------------------------------------
rem Open the two inbound paths a controlled machine needs, from its own console.
rem
rem Run it as administrator (right-click -> Run as administrator). The rules go
rem into the machine-wide store, so a non-elevated run fails with "access is
rem denied" and changes nothing -- which is why this checks first instead of
rem printing a wall of errors.
rem
rem Why this file exists: on a fresh Windows every inbound packet is dropped on
rem the Public profile, ICMP included. The controlled machine then looks dead
rem from the build machine -- ping fails, 8080 times out, every port TIMEOUT --
rem while the server is running perfectly. The Windows popup shown on first
rem start only helps if somebody sits at that machine and clicks "allow".
rem
rem   8080 TCP   HTTPS page, WS signalling, file transfer
rem   ICMP echo  so "ping <ip>" answers, which is what everyone tries first
rem
rem Rules are added with profile=any on purpose: a VMware NAT / host-only
rem adapter is often classified as "public", and a rule limited to Private will
rem silently do nothing there.
rem ---------------------------------------------------------------------------
setlocal EnableExtensions

net session >nul 2>&1
if errorlevel 1 (
    echo.
    echo This script must run elevated.
    echo Right-click setup-inbound.cmd and choose "Run as administrator".
    echo.
    pause
    exit /b 1
)

echo -- current firewall profile --
netsh advfirewall show currentprofile state

echo.
echo -- dropping old rules of the same name ^(silently, they may not exist^) --
netsh advfirewall firewall delete rule name="cloud-stream 8080" >nul 2>&1
netsh advfirewall firewall delete rule name="cloud-stream ICMPv4" >nul 2>&1

echo -- adding rules for ALL profiles --
netsh advfirewall firewall add rule name="cloud-stream 8080" dir=in action=allow protocol=TCP localport=8080 profile=any
netsh advfirewall firewall add rule name="cloud-stream ICMPv4" dir=in action=allow protocol=icmpv4:8,any profile=any

echo.
echo -- rules now in place --
netsh advfirewall firewall show rule name="cloud-stream 8080"
netsh advfirewall firewall show rule name="cloud-stream ICMPv4"

echo.
echo -- local self check ^(the firewall does not affect loopback^) --
rem Full path, same reason as find.exe: a PATH with MSYS or a portable curl first
rem would run a different binary with different flags.
"%SystemRoot%\System32\curl.exe" -sk -m 4 -o NUL -w "https 8080 = HTTP %%{http_code}\n" https://127.0.0.1:8080/api/v1/health
if errorlevel 1 (
    echo   nothing is listening on 8080 yet -- start the server, then run this
    echo   check again:  start-stream.cmd
)

echo.
echo Done. From another machine:
echo    ping ^<this-ip^>
echo    https://^<this-ip^>:8080/
echo.
echo If a third-party firewall ^(360, Huorong, McAfee^) is installed it may
echo still block 8080 -- allow stream-server.exe there as well.
echo.
pause
