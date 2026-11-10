@echo off
rem ---------------------------------------------------------------------------
rem Internal helper: make sure the four inbound rules exist. Idempotent.
rem
rem Call it, do not double-click it -- setup-inbound.cmd is the human-facing
rem wrapper (it shows the rules, self-tests 8080 and pauses).
rem
rem Called from three places, all of which used to leave this to the operator:
rem   start-stream.cmd   on every launch (the watchdog fires every minute)
rem   setup-watchdog.bat after the task is created
rem   setup-inbound.cmd  when somebody runs it by hand
rem
rem Exit codes:
rem   0  rules are in place (already were, or just added)
rem   1  rules missing and this process is not elevated -- needs a human
rem   2  elevated but netsh refused (third-party firewall, policy, GPO)
rem
rem Why the rules matter: on a fresh Windows every inbound packet is dropped on
rem the Public profile, ICMP included. The controlled machine then looks dead
rem from the build machine -- ping fails, 8080 times out -- while the server is
rem running perfectly. profile=any on purpose: a VMware NAT / host-only adapter
rem is usually classified as "public", and a Private-only rule does nothing
rem there without saying so.
rem
rem The four rules, and what each one is for:
rem
rem   cloud-stream 8080        TCP  page + WS signalling + file transfer
rem   cloud-stream ICMPv4      echo, so "ping <ip>" answers -- the first thing
rem                                 everyone tries, and the thing that makes a
rem                                 healthy host look dead when it is missing
rem   cloud-stream webrtc UDP  40000-40049, ICE host candidates
rem   cloud-stream app         stream-server.exe, every port and protocol
rem
rem Why WebRTC needs its own rule: ICE binds one UDP socket per local address,
rem and the library defaults that socket to ":0" -- a different random port
rem every session, which no inbound rule can follow. #122 pins the range
rem (--webrtc-udp-port-min/max, default 40000-40049) so a port rule can
rem describe it. Without this the page says "WebRTC cannot connect / the media
rem path fails from this network" on a stock Windows while WebSockets works:
rem signalling is TCP 8080, media is UDP, and only one of them was open.
rem
rem 40000-40049 must stay in lockstep with the config default -- the rule and
rem the range describe the same thing to two different readers. The program
rem rule is the belt-and-braces for a deployment that changes the range, or
rem unpacks the bundle somewhere else: it matches by path and covers every port.
rem
rem Keep pure ASCII: cmd reads this file byte by byte in the OEM code page.
rem ---------------------------------------------------------------------------
setlocal EnableExtensions

set "R8080=cloud-stream 8080"
set "RICMP=cloud-stream ICMPv4"
set "RUDP=cloud-stream webrtc UDP"
set "RAPP=cloud-stream app"

rem %~dp0 ends with a backslash; strip it so the path matches what netsh stored.
set "ROOT=%~dp0"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"
set "EXE=%ROOT%\stream-server.exe"

rem A rule that already exists returns 0; a missing one returns 1. The query
rem needs no elevation, so this guard is safe to run from the per-minute task.
netsh advfirewall firewall show rule name="%R8080%" >nul 2>&1
if errorlevel 1 goto :install
netsh advfirewall firewall show rule name="%RICMP%" >nul 2>&1
if errorlevel 1 goto :install
netsh advfirewall firewall show rule name="%RUDP%" >nul 2>&1
if errorlevel 1 goto :install

rem The program rule is bound to an absolute path, and netsh does not print that
rem path in `show rule` (a program rule has no port and no "program:" line), so
rem this guard can only check the name -- the path is refreshed by the delete +
rem add below whenever the install branch runs. A moved bundle therefore keeps a
rem stale rule until somebody runs setup-inbound.cmd again, which is harmless:
rem the 8080 and UDP rules above are what actually carry the traffic.
netsh advfirewall firewall show rule name="%RAPP%" >nul 2>&1
if errorlevel 1 goto :install
exit /b 0

:install
rem net session succeeds only with an elevated token. The watchdog task runs as
rem an interactive user (/IT) and is normally *not* elevated even when that
rem account is an administrator -- UAC hands it a filtered token -- so this
rem branch is the common one in the field, not an edge case.
net session >nul 2>&1
if errorlevel 1 exit /b 1

rem Delete then add, never add alone: netsh happily creates a *second* rule
rem with the same display name, so repairing one missing rule would otherwise
rem leave duplicates of the other three behind on every run. The app rule is
rem deleted by name too, because an existing one may still name a previous
rem unpack directory.
rem
rem `delete rule name=all dir=in program=` also drops a dismissed "Windows has
rem blocked some features of this app" BLOCK rule. Block beats allow, and no
rem `show rule name=` output distinguishes the two -- leaving one behind is a
rem rule that looks present and forwards nothing. No match is not an error
rem worth failing over, hence the ignored exit codes.
netsh advfirewall firewall delete rule name="%R8080%" >nul 2>&1
netsh advfirewall firewall delete rule name="%RICMP%" >nul 2>&1
netsh advfirewall firewall delete rule name="%RUDP%" >nul 2>&1
netsh advfirewall firewall delete rule name="%RAPP%" >nul 2>&1
netsh advfirewall firewall delete rule name=all dir=in program="%EXE%" >nul 2>&1

netsh advfirewall firewall add rule name="%R8080%" dir=in action=allow protocol=TCP localport=8080 profile=any >nul 2>&1
if errorlevel 1 exit /b 2
netsh advfirewall firewall add rule name="%RICMP%" dir=in action=allow protocol=icmpv4:8,any profile=any >nul 2>&1
netsh advfirewall firewall add rule name="%RUDP%" dir=in action=allow protocol=UDP localport=40000-40049 profile=any >nul 2>&1
netsh advfirewall firewall add rule name="%RAPP%" dir=in action=allow program="%EXE%" enable=yes profile=any >nul 2>&1

exit /b 0
