# setup-task.ps1 -- create the MCStreamSrv watchdog task WITHOUT a password.
#
# Called by setup-watchdog.bat. Kept in its own .ps1 rather than an inline
# -Command because every argument here is a path that may contain spaces and
# quote characters; nesting that through cmd -> powershell -Command is exactly
# where the old one-liner kept breaking.
#
# WHY THERE IS NO PASSWORD
#
#   The task must run inside the session that owns the desktop: screen capture
#   and SendInput are per-session, and a process in session 0 gets an empty
#   desktop. The old script expressed that as `schtasks /RU user /RP pass /IT`.
#   /IT is right; /RP is not, and it was the only reason this bundle ever asked
#   for the controlled machine's account password.
#
#   schtasks' /RP makes the task do a *batch logon* at every trigger, so Windows
#   stores the password and, by default (LimitBlankPasswordUse=1), refuses the
#   whole thing on an account that has none.
#
#   LogonType=Interactive is the other way to say the same thing: reuse the
#   token the console session already holds. No logon happens, no credential is
#   collected or stored, and a blank-password account works. Nothing in the SAM
#   database is read or written -- the account is left exactly as it was.
#
#   Verified on a blank-password account (Password required: No):
#   registers, runs, LastTaskResult 0x0.
#
# Kept pure ASCII: written without a BOM, so Windows PowerShell 5.1 would read
# anything else in the ANSI code page.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Root,
    [string]$TaskName = 'MCStreamSrv',
    [string]$Account,
    [switch]$Start
)

$ErrorActionPreference = 'Stop'

function Fail([int]$code, [string]$text) {
    Write-Output "  [fail] $text"
    exit $code
}

# ---- 1 the console user, not whoever happens to be running this script ------
# An elevated copy may be running as Administrator while the desktop belongs to
# someone else. The task has to land on the desktop's owner.
if (-not $Account) {
    try {
        $cs = Get-CimInstance -ClassName Win32_ComputerSystem -ErrorAction Stop
        $Account = $cs.UserName
    } catch {
        $Account = $null
    }
    if (-not $Account) { $Account = "$env:USERDOMAIN\$env:USERNAME" }
}

# Absolute from here on: a relative -Root would be baked into the task action
# and then resolved against whatever directory Task Scheduler happens to use.
try {
    $Root = (Resolve-Path -LiteralPath $Root -ErrorAction Stop).Path
} catch {
    Fail 1 "root directory not found: $Root"
}

$vbs = Join-Path $Root 'run-hidden.vbs'
if (-not (Test-Path -LiteralPath $vbs)) {
    Fail 1 "missing $vbs -- unpack the whole bundle, then run this from its own directory"
}

# ---- 2 definition ----------------------------------------------------------
# The action keeps exactly the shape the old /TR had: wscript.exe "<vbs>".
# Quoting is done once, here, instead of being escaped through three layers.
$action = New-ScheduledTaskAction -Execute 'wscript.exe' -Argument ('"{0}"' -f $vbs)

# Two triggers on purpose:
#   * at logon  -- no dead window after a reboot/logon; the old minute-only
#                  trigger left up to 60 s with nothing listening
#   * every 1 min -- the watchdog itself: Task Scheduler's "do not start a new
#                  instance" turns a blocked copy into "while alive, nothing
#                  happens; when it dies, the next minute brings it back"
$triggerLogon = New-ScheduledTaskTrigger -AtLogOn
$triggerMinute = New-ScheduledTaskTrigger -Once -At ((Get-Date).AddMinutes(1)) `
    -RepetitionInterval (New-TimeSpan -Minutes 1)

# Interactive = "run only when the user is logged on", the same session
# behaviour as /IT and the same limited token the account normally gets. No
# password field exists in this principal, by construction.
$principal = New-ScheduledTaskPrincipal -UserId $Account -LogonType Interactive -RunLevel Limited

# MultipleInstances IgnoreNew is the watchdog's whole mechanism -- see above.
# ExecutionTimeLimit Zero = unlimited: schtasks' default for a repeated task is
# 72 hours, which silently killed a server that had been up for three days.
$settings = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -StartWhenAvailable `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries

# ---- 3 register ------------------------------------------------------------
try {
    Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger @($triggerLogon, $triggerMinute) `
        -Principal $principal -Settings $settings -Force -ErrorAction Stop | Out-Null
} catch {
    $msg = $_.Exception.Message
    Write-Output "  [fail] could not create task $TaskName : $msg"
    if ($msg -match 'denied|0x80070005') {
        Write-Output '         Run this script as administrator (right-click -> Run as administrator).'
    } elseif ($msg -match '0x80070534|no mapping') {
        Write-Output "         Account '$Account' is not a local account on this machine."
    }
    exit 2
}

# ---- 4 read it back --------------------------------------------------------
# A task can be created and still be wrong -- pointing at a truncated path, or
# carrying a logon type nobody intended. Both symptoms look like "the task ran
# but nothing is listening", a week later.
$task = Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop
$act = $task.Actions | Select-Object -First 1
$actLine = ('{0} {1}' -f $act.Execute, $act.Arguments).Trim()
$logon = $task.Principal.LogonType
$trgNames = @($task.Triggers | ForEach-Object { $_.CimClass.CimClassName -replace '^MSFT_Task', '' -replace 'Trigger$', '' })

Write-Output "  account  = $($task.Principal.UserId)"
Write-Output "  logon    = $logon (no password stored)"
Write-Output "  action   = $actLine"
Write-Output "  triggers = $($trgNames -join ', ')"

if ($actLine -notmatch 'run-hidden\.vbs') {
    Write-Output '  [warn] the task action does not mention run-hidden.vbs. Inspect with:'
    Write-Output "         Get-ScheduledTask -TaskName $TaskName | Select-Object -Expand Actions"
    exit 3
}
if ($logon -ne 'Interactive') {
    Write-Output "  [warn] logon type is '$logon', expected Interactive -- the process would land outside the desktop session"
    exit 3
}

# ---- 5 optional immediate start -------------------------------------------
if ($Start) {
    try {
        Start-ScheduledTask -TaskName $TaskName -ErrorAction Stop
    } catch {
        Write-Output "  [fail] task created but not started: $($_.Exception.Message)"
        exit 5
    }
}

exit 0
