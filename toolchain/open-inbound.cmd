@echo off
rem ---------------------------------------------------------------------------
rem Internal helper: make sure the two inbound rules exist. Idempotent.
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
rem Keep pure ASCII: cmd reads this file byte by byte in the OEM code page.
rem ---------------------------------------------------------------------------
setlocal EnableExtensions

set "R8080=cloud-stream 8080"
set "RICMP=cloud-stream ICMPv4"

rem A rule that already exists returns 0; a missing one returns 1. The query
rem needs no elevation, so this guard is safe to run from the per-minute task.
netsh advfirewall firewall show rule name="%R8080%" >nul 2>&1
if not errorlevel 1 (
    netsh advfirewall firewall show rule name="%RICMP%" >nul 2>&1
    if not errorlevel 1 exit /b 0
)

rem net session succeeds only with an elevated token. The watchdog task runs as
rem an interactive user (/IT) and is normally *not* elevated even when that
rem account is an administrator -- UAC hands it a filtered token -- so this
rem branch is the common one in the field, not an edge case.
net session >nul 2>&1
if errorlevel 1 exit /b 1

netsh advfirewall firewall add rule name="%R8080%" dir=in action=allow protocol=TCP localport=8080 profile=any >nul 2>&1
if errorlevel 1 exit /b 2
netsh advfirewall firewall add rule name="%RICMP%" dir=in action=allow protocol=icmpv4:8,any profile=any >nul 2>&1

exit /b 0
