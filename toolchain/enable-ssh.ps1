#Requires -Version 5.1
<#
.SYNOPSIS
    One-shot bootstrap: turn this Windows host into an SSH target that only
    the build machine can reach, key-only, without touching any account
    password.

.DESCRIPTION
    Written for a host that has never run sshd (no port 22, no firewall rule),
    i.e. the chicken-and-egg case that deploy.ps1 cannot handle because it gets
    to the host *through* ssh.

    The order is deliberate. A wrong public key plus "PasswordAuthentication no"
    is a host nobody can log into, so the script proves key auth works first:

      1. install the OpenSSH Server capability (skip when present)
      2. sshd -> Automatic, start it, make sure host keys exist
      3. append the public key to administrators_authorized_keys and/or the
         console user's authorized_keys, with the ACL sshd insists on
      4. firewall: allow TCP 22 from <ClientAddress> and loopback only, and
         disable the stock "allow from anywhere" rules that would undo that
      5. restart sshd, then SELF-TEST by logging in over 127.0.0.1 with the key
      6. only if the self-test passed: turn password auth off, restart, test
         again. On any failure the sshd_config backup is restored.

    Output is ASCII English on purpose: this runs in a console whose code page
    is GBK, and mixed-script output is unreadable exactly when it matters.

.PARAMETER ClientAddress
    Source address allowed to reach port 22. Default 192.168.59.146 (the build
    machine). Loopback is always added so the self-test can run.

.PARAMETER PublicKey
    A single public key as text. Defaults to the embedded build-machine key.

.PARAMETER PublicKeyFile
    Read the key from a .pub file instead. Wins over -PublicKey.

.PARAMETER KeepPassword
    Leave PasswordAuthentication as it was. Use when the key might be wrong and
    you want a second way in (the account must actually have a password).

.PARAMETER Check
    Read-only. Prints what is installed / running / allowed and changes nothing.
    Needs no administrator rights.

.PARAMETER NoSelfTest
    Skip step 5. Not recommended: without it there is no proof the key works.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File .\enable-ssh.ps1

.EXAMPLE
    .\enable-ssh.ps1 -Check

.EXAMPLE
    .\enable-ssh.ps1 -KeepPassword -ClientAddress 192.168.59.10

.NOTES
    Re-running is safe and is the intended repair: the key line is not added
    twice, and leftovers from an interrupted run (a cs-sshprobe line in
    administrators_authorized_keys, a private key under %TEMP%) are removed
    before anything else happens.

    Run it from the host's own console. Steps 5 and 6 restart sshd, which
    terminates every existing connection -- including an ssh session that is
    hosting this script, which could strand the host between "password off" and
    "re-tested".
#>
[CmdletBinding()]
param(
    [string]$ClientAddress = '192.168.59.146',
    [string]$PublicKey = '',
    [string]$PublicKeyFile = '',
    [switch]$KeepPassword,
    [switch]$Check,
    [switch]$NoSelfTest,
    # internal: set by the elevated re-launch
    [switch]$Elevated
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

# The build machine's key. Public keys are not secrets; embedding it means the
# operator only has to copy one file to the host.
$DefaultKey = 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPfORDY6B0iyB++h0O4dne+GGnCYDaFJO4ikfV4JDBiC mc-main@server001'

$SshdConfig = Join-Path $env:ProgramData 'ssh\sshd_config'
$SshDir     = Join-Path $env:ProgramData 'ssh'
$AdminKeys  = Join-Path $SshDir 'administrators_authorized_keys'
$LogDir     = Join-Path $SshDir 'logs'
$AdminsSid  = 'S-1-5-32-544'

$script:ExitCode = 0
$script:Backup = $null

# [Console]::Out rather than Write-Host: an empty string has to produce an
# empty LINE, and Write-Host '' loses it when the host stream is redirected --
# which is exactly what happens when the operator pipes this into a file to
# send back.
function Say  ($m) { [Console]::Out.WriteLine([string]$m) }
# Every status line goes through Say as well. Mixing Write-Host with
# Console::Out interleaves two buffers, and the first casualty is the blank
# separator line -- the log then reads as "...password off)-- 1/6 --", which is
# exactly the kind of thing that makes a pasted report useless.
function Ok   ($m) { Say "  [ok]   $m" }
function Note ($m) { Say "  [i]    $m" }
function Warn ($m) { Say "  [warn] $m" }
function Fail ($m) { Say "  [fail] $m"; $script:ExitCode = 1 }
function Head ($m) { Say ''; Say "-- $m --" }

# ---- elevation -------------------------------------------------------------
# Same trick as install.cmd: parameters travel through the environment, never
# through a reconstructed command line, so a path with spaces or quotes cannot
# reopen the wrong file.
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin = (New-Object Security.Principal.WindowsPrincipal($identity)).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)

if (-not $isAdmin -and -not $Check -and -not $Elevated) {
    $env:CS_SSH_CLIENT  = $ClientAddress
    $env:CS_SSH_KEY     = $PublicKey
    $env:CS_SSH_KEYFILE = $PublicKeyFile
    $env:CS_SSH_KEEP    = if ($KeepPassword) { '1' } else { '0' }
    $env:CS_SSH_NOTEST  = if ($NoSelfTest) { '1' } else { '0' }
    Say 'this needs administrator rights; opening a UAC prompt'
    try {
        # -NoExit on purpose: -File + a finished script closes its own console,
        # and the whole point of this run is the report at the end. The window
        # stays, with the report still in it, until the operator closes it.
        # Because of that this process cannot wait for an exit code -- it says
        # where the work went and leaves.
        Start-Process -FilePath 'powershell.exe' `
            -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass',
                            '-File', "`"$PSCommandPath`"", '-Elevated', '-NoExit') `
            -Verb RunAs -ErrorAction Stop | Out-Null
    } catch {
        Fail 'the elevation prompt was declined or blocked'
        Say  'right-click the script and choose "Run with PowerShell"'
        Say  'as administrator, or run it from an administrator terminal'
        exit 1
    }
    Say 'the elevated window does the work and keeps the report; this one can be closed'
    exit 0
}

if ($Elevated) {
    if ($env:CS_SSH_CLIENT)  { $ClientAddress  = $env:CS_SSH_CLIENT }
    if ($env:CS_SSH_KEY)     { $PublicKey      = $env:CS_SSH_KEY }
    if ($env:CS_SSH_KEYFILE) { $PublicKeyFile  = $env:CS_SSH_KEYFILE }
    if ($env:CS_SSH_KEEP -eq '1')   { $KeepPassword = $true }
    if ($env:CS_SSH_NOTEST -eq '1') { $NoSelfTest   = $true }
}

# ---- helpers ---------------------------------------------------------------

function Get-SshdExe {
    $c = @(
        (Join-Path $env:ProgramFiles 'OpenSSH\sshd.exe'),
        (Join-Path $env:SystemRoot 'System32\OpenSSH\sshd.exe')
    )
    foreach ($p in $c) { if (Test-Path $p) { return $p } }
    return $null
}

function Get-SshClientExe {
    $c = @(
        (Join-Path $env:SystemRoot 'System32\OpenSSH\ssh.exe'),
        (Join-Path $env:ProgramFiles 'OpenSSH\ssh.exe')
    )
    foreach ($p in $c) { if (Test-Path $p) { return $p } }
    $cmd = Get-Command ssh.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    return $null
}

# An OpenSSH private key names its cipher right after the openssh-key-v1 magic:
# "none" for an unprotected key. Reading that is the only way to tell without an
# interactive passphrase prompt -- and getting it wrong is invisible until ssh
# silently refuses to sign.
function Test-KeyIsUnprotected ($path) {
    try {
        $body = @(Get-Content -LiteralPath $path |
                  Where-Object { $_ -notmatch '-----(BEGIN|END)' -and $_ -match '\S' })
        if ($body.Count -eq 0) { return $false }
        $raw = [Convert]::FromBase64String(($body -join ''))
        $text = [Text.Encoding]::ASCII.GetString($raw)
        return ($text.Contains('openssh-key-v1') -and $text.Contains('none'))
    } catch {
        return $false
    }
}

# ---- running a native exe without losing the script -------------------------
# `& $exe ... 2>&1` does NOT mean "capture stderr" on Windows PowerShell 5.1.
# Every stderr LINE is turned into a System.Management.Automation.ErrorRecord of
# type NativeCommandError, and with $ErrorActionPreference = 'Stop' (set above)
# the FIRST one terminates the whole script.
#
# That is exactly how host .150 ended up half-configured. ssh printed
#   Warning: Permanently added '127.0.0.1' (ED25519) to the list of known hosts.
# on stderr -- a warning, emitted after a SUCCESSFUL connection -- the script died
# in step 5 before it could read the exit code, so step 6 never ran (password
# auth stayed ON) and the throwaway key stayed in
# administrators_authorized_keys with its private half in %TEMP%.
#
# Start-Process with files has no pipeline and therefore no ErrorRecord, so a
# chatty child process cannot kill the script, and the behaviour is the same on
# 5.1 and on 7 (7.2+ additionally honours $PSNativeCommandUseErrorActionPreference,
# which makes `2>&1` even more eager to throw).
function Invoke-Native ($exe, [string[]]$arguments) {
    $so = [IO.Path]::GetTempFileName()
    $se = [IO.Path]::GetTempFileName()
    $code = -1
    try {
        $p = Start-Process -FilePath $exe -ArgumentList $arguments `
                -NoNewWindow -Wait -PassThru `
                -RedirectStandardOutput $so -RedirectStandardError $se
        $code = $p.ExitCode
    } catch {
        return [pscustomobject]@{
            ExitCode = -1; Out = @(); Err = @()
            Combined = @('could not start ' + [IO.Path]::GetFileName($exe) + ': ' + $_.Exception.Message)
        }
    }
    $o = @(); $e = @()
    try {
        $o = @(Get-Content -LiteralPath $so -ErrorAction SilentlyContinue)
        $e = @(Get-Content -LiteralPath $se -ErrorAction SilentlyContinue)
    } finally {
        foreach ($f in @($so, $se)) { try { [IO.File]::Delete($f) } catch { } }
    }
    return [pscustomobject]@{
        ExitCode = $code
        Out      = $o
        Err      = $e
        Combined = @($o + $e)
    }
}

# The self-test key is tagged cs-sshprobe so its line can be found again exactly.
# Purging before appending makes this script idempotent on a host whose previous
# run was interrupted: the stale line is removed instead of accumulating, and a
# leftover private half in %TEMP% is deleted as well.
function Remove-ProbeKeyLines {
    if (-not (Test-Path $AdminKeys)) { return 0 }
    $lines = @(Get-Content -LiteralPath $AdminKeys)
    # Count what is actually a probe line, not "every line that disappeared":
    # the rewrite also drops blank lines, and reporting "removed 3" when one key
    # line went away reads like the file lost a real key.
    $probes = @($lines | Where-Object { $_ -match 'cs-sshprobe' })
    if ($probes.Count -eq 0) { return 0 }
    $keep = @($lines | Where-Object { $_ -match '\S' -and $_ -notmatch 'cs-sshprobe' })
    [IO.File]::WriteAllText($AdminKeys, (($keep -join "`r`n") + "`r`n"),
                            (New-Object System.Text.ASCIIEncoding))
    return $probes.Count
}

function Remove-StaleProbeDirs {
    $n = 0
    foreach ($d in @(Get-ChildItem $env:TEMP -Filter 'cs-sshprobe-*' -Directory -ErrorAction SilentlyContinue)) {
        try { [IO.Directory]::Delete($d.FullName, $true); $n++ } catch { }
    }
    return $n
}

# One place that undoes the self-test, callable from normal flow AND from the
# trap. Get-Variable instead of $script:tempKey: under Set-StrictMode an
# unassigned name throws, and the trap can fire before section 5 ever runs --
# a trap that throws is a script that exits with no message at all.
function Clear-ProbeArtifacts {
    $tk = $null; $td = $null
    $v = Get-Variable -Name 'tempKey' -Scope Script -ErrorAction SilentlyContinue
    if ($v) { $tk = $v.Value }
    $v = Get-Variable -Name 'tempDir' -Scope Script -ErrorAction SilentlyContinue
    if ($v) { $td = $v.Value }
    [void](Remove-ProbeKeyLines)
    if ($tk) {
        foreach ($f in @($tk, "$tk.pub")) {
            try { if (Test-Path $f) { [IO.File]::Delete($f) } } catch { }
        }
    }
    if ($td) { try { if (Test-Path $td) { [IO.Directory]::Delete($td, $true) } } catch { } }
    $script:tempKey = $null
    $script:tempDir = $null
}

function Get-ServiceState ($name) {
    $svc = Get-CimInstance Win32_Service -Filter "Name='$name'" -ErrorAction SilentlyContinue
    if (-not $svc) { return $null }
    return [pscustomobject]@{ State = $svc.State; StartMode = $svc.StartMode }
}

# Read-only view of everything that decides whether ssh will work.
function Show-Facts {
    $cap = Get-WindowsCapability -Online -Name 'OpenSSH.Server*' -ErrorAction SilentlyContinue |
           Select-Object -First 1
    if ($cap -and $cap.State -eq 'Installed') { Ok "OpenSSH Server capability: $($cap.State)" }
    elseif ($cap) { Warn "OpenSSH Server capability: $($cap.State)" }
    else          { Warn 'OpenSSH Server capability: not present' }

    $svc = Get-ServiceState 'sshd'
    if ($svc) { Ok "service sshd: $($svc.State) / $($svc.StartMode)" }
    else      { Warn 'service sshd: not installed' }

    $listen = @(Get-NetTCPConnection -LocalPort 22 -State Listen -ErrorAction SilentlyContinue)
    if ($listen.Count -gt 0) { Ok "listening on port 22 ($($listen[0].LocalAddress))" }
    else                     { Warn 'nothing is listening on port 22' }

    # Two kinds of rule decide the answer:
    #   * one that names TCP 22              -> listed by name
    #   * one that allows every port, for
    #     every program, from everywhere    -> listed, because it silently
    #                                          re-opens what this script closes
    # UWP/AppContainer rules name "Any" too, but they are loopback scoped and
    # there are dozens of them; listing those would bury the two that matter,
    # so they are only counted.
    Note 'inbound rules that allow TCP 22:'
    $all = @(Get-NetFirewallRule -Direction Inbound -Enabled True -Action Allow -ErrorAction SilentlyContinue)
    $explicit = New-Object System.Collections.ArrayList
    $broad    = New-Object System.Collections.ArrayList
    $noise    = 0
    foreach ($r in $all) {
        $f = $r | Get-NetFirewallPortFilter
        if ($f.Protocol -ne 'TCP' -and $f.Protocol -ne 'Any') { continue }
        $ports = @($f.LocalPort)
        if ($ports -contains '22') { [void]$explicit.Add($r); continue }
        if ($ports -notcontains 'Any') { continue }
        $app = $r | Get-NetFirewallApplicationFilter
        $prog = "$($app.Program)"
        # Two shapes of Store-app rule: Group is an ms-resource id ("@{...}"),
        # or Group is simply the app's own display name. Both allow Any/Any on
        # paper but are loopback scoped. This is a heuristic -- the real answer
        # is the port test from the build machine -- but it keeps the listing
        # down to rules a human could have created.
        $grp = "$($r.Group)"
        $uwp = $grp.StartsWith('@{') -or ($grp -ne '' -and $grp -eq $r.DisplayName)
        if (-not $uwp -and ($prog -eq '' -or $prog -eq 'Any')) {
            [void]$broad.Add($r)
        } else {
            $noise++
        }
    }
    if ($explicit.Count -eq 0 -and $broad.Count -eq 0) {
        Note '  (none)'
    } else {
        foreach ($r in @($explicit) + @($broad)) {
            $addr = ($r | Get-NetFirewallAddressFilter).RemoteAddress
            Note ("  {0}  enabled={1}  from={2}" -f $r.DisplayName, $r.Enabled, ($addr -join ','))
        }
    }
    if ($noise -gt 0) {
        Note "  ($noise app-scoped rules also cover all ports; not shown)"
    }

    if (Test-Path $AdminKeys) {
        $n = @(Get-Content -LiteralPath $AdminKeys | Where-Object { $_ -match '\S' }).Count
        Ok "administrators_authorized_keys: $n line(s)"
    } else {
        Note 'administrators_authorized_keys: absent'
    }

    if (Test-Path $SshdConfig) {
        foreach ($k in 'PubkeyAuthentication', 'PasswordAuthentication', 'AuthorizedKeysFile') {
            $line = (Get-Content -LiteralPath $SshdConfig | Where-Object { $_ -match "^\s*$k\s" } | Select-Object -Last 1)
            $shown = '(not set)'
            if ($line) { $shown = $line.Trim() }
            Note ("  {0}: {1}" -f $k, $shown)
        }
    }

    $console = (Get-CimInstance Win32_ComputerSystem).UserName
    Note "console user: $console"
    Note "running as:   $env:USERDOMAIN\$env:USERNAME"
}

if ($Check) {
    Say '======================================================================'
    Say ' cloud-stream  -  ssh readiness (read-only)'
    Say '======================================================================'
    Show-Facts
    Say ''
    Say 'no changes were made.'
    exit 0
}

# ---- start ----------------------------------------------------------------
Say '======================================================================'
Say ' cloud-stream  -  one-shot ssh bootstrap'
Say '======================================================================'
Say "host   : $env:COMPUTERNAME"
Say "allow  : $ClientAddress + loopback, TCP 22"
Say ("mode   : " + $(if ($KeepPassword) { 'keep password auth' } else { 'key-only (password auth off)' }))

if ($script:ExitCode -ne 0) { exit 1 }

# ---- last resort -----------------------------------------------------------
# Anything that still manages to throw (Start-Process denied, an ACL that moved,
# sshd.exe replaced mid-run) lands here instead of vanishing. The cleanup is the
# point: an interrupted run must never leave a private key in %TEMP% plus a key
# line the next person reads as a backdoor -- which is precisely the state host
# .150 was left in by the 2>&1 bug this script used to have.
trap {
    # -Check is documented as read-only and needs no administrator rights, so it
    # must not write to administrators_authorized_keys on the way out.
    if (-not $Check) { try { Clear-ProbeArtifacts } catch { } }
    Say ''
    Warn "unexpected error: $($_.Exception.Message)"
    if ($_.InvocationInfo) {
        Note ("at {0} line {1}" -f [IO.Path]::GetFileName($_.InvocationInfo.ScriptName),
                                    $_.InvocationInfo.ScriptLineNumber)
    }
    if (-not $Check) { Note 'the self-test key, if one was created, has been removed' }
    exit 1
}

# ---- 1. the capability -----------------------------------------------------
Head '1/6 OpenSSH Server'

$cap = Get-WindowsCapability -Online -Name 'OpenSSH.Server*' -ErrorAction SilentlyContinue |
       Select-Object -First 1
if (-not $cap) {
    Fail 'this Windows build has no OpenSSH.Server capability (needs 10 1809 / Server 2019 or newer)'
    Say '  workaround: install Win32-OpenSSH from'
    Say '  https://github.com/PowerShell/Win32-OpenSSH/releases'
    exit 1
}
if ($cap.State -eq 'Installed') {
    Ok 'already installed'
} else {
    Note "state was '$($cap.State)'; installing"
    Note 'the payload comes from Windows Update: a few minutes normally, but a'
    Note 'throttled or distant update source can make it 15+. Nothing is wrong'
    Note 'while this line is the last one -- it is downloading.'
    try {
        Add-WindowsCapability -Online -Name $cap.Name -ErrorAction Stop | Out-Null
    } catch {
        Warn "Add-WindowsCapability failed: $($_.Exception.Message)"
        Note 'trying DISM'
        & "$env:SystemRoot\System32\Dism.exe" /Online /Add-Capability /CapabilityName:$($cap.Name) /Quiet /NoRestart | Out-Null
    }
    $cap = Get-WindowsCapability -Online -Name 'OpenSSH.Server*' | Select-Object -First 1
    if ($cap.State -ne 'Installed') {
        Fail "still '$($cap.State)'. No local payload and no internet access?"
        Say '  a machine that can reach https://github.com/PowerShell/Win32-OpenSSH/releases'
        Say '  can bring the .msi over; or copy %ProgramFiles%\OpenSSH from a host that has it.'
        exit 1
    }
    Ok 'installed'
}

$sshdExe = Get-SshdExe
if (-not $sshdExe) {
    Fail 'sshd.exe not found under %ProgramFiles%\OpenSSH or %SystemRoot%\System32\OpenSSH'
    exit 1
}
Ok $sshdExe
$sshKeygen = Join-Path (Split-Path $sshdExe) 'ssh-keygen.exe'

# ---- 2. service + host keys ------------------------------------------------
Head '2/6 service sshd'

$svc = Get-ServiceState 'sshd'
if (-not $svc) {
    Fail 'the sshd service was not registered by the capability install'
    exit 1
}
try {
    Set-Service -Name sshd -StartupType Automatic -ErrorAction Stop
    Ok 'start type -> Automatic'
} catch {
    & "$env:SystemRoot\System32\sc.exe" config sshd start= auto | Out-Null
    Note 'start type set through sc.exe'
}
if ((Get-ServiceState 'sshd').State -ne 'Running') {
    Start-Service sshd
    Note 'started'
} else {
    Ok 'already running'
}

# Host keys are generated on first start, but a silent failure here shows up
# later as "sshd exits immediately", so make sure.
if (-not (Test-Path (Join-Path $SshDir 'ssh_host_ed25519_key'))) {
    Note 'generating host keys'
    & $sshKeygen -A | Out-Null
}
Ok 'host keys present'

# ---- 3. the public key -----------------------------------------------------
Head '3/6 authorized keys'

if ($PublicKeyFile) {
    if (-not (Test-Path $PublicKeyFile)) {
        Fail "no such file: $PublicKeyFile"
        exit 1
    }
    $key = (Get-Content -LiteralPath $PublicKeyFile -Raw).Trim()
} elseif ($PublicKey) {
    $key = $PublicKey.Trim()
} else {
    $key = $DefaultKey
}

# One line, right shape. A malformed line is accepted by the file but rejected
# at login time, which is exactly the failure that locks people out.
$key = ($key -split "`r?`n" | Where-Object { $_ -match '\S' } | Select-Object -First 1).Trim()
if ($key -notmatch '^(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp(256|384|521)|sk-ssh-ed25519@openssh\.com|sk-ecdsa-sha2-nistp256@openssh\.com)\s+AAAA[0-9A-Za-z+/]+={0,2}(\s+\S.*)?$') {
    Fail 'the public key does not look like one OpenSSH line'
    Say "  got: $($key.Substring(0, [Math]::Min(60, $key.Length)))..."
    exit 1
}
Ok "key: $($key.Substring(0, [Math]::Min(40, $key.Length)))... ($($key.Split(' ')[0]))"

# Strip the comment for comparison: the same key pasted twice with different
# comments is still the same key, and a duplicate line is harmless but noisy.
$keyBody = ($key -split '\s+' | Select-Object -First 2) -join ' '

function Add-AuthorizedKey ($path, $aclOwnerSid) {
    if (-not (Test-Path $SshDir)) { New-Item -ItemType Directory -Force -Path $SshDir | Out-Null }
    $name = [IO.Path]::GetFileName($path)

    if (Test-Path $path) {
        # A file left with an empty DACL by an older version of this script
        # cannot be read, written, renamed or deleted by anyone. Detect it
        # before trying, so the message is a fix recipe rather than
        # "Access to the path is denied".
        $probe = $null
        try { $probe = Get-Acl -LiteralPath $path -ErrorAction Stop } catch { $probe = $null }
        if ($null -eq $probe -or $probe.Access.Count -eq 0) {
            Fail "$name exists but nobody has access to it (empty DACL)"
            Note 'a previous run removed the inheritance and failed to grant it back.'
            Note 'repair it from a SYSTEM context -- an administrator cannot:'
            Note "  schtasks /Create /TN FixSshAcl /SC ONCE /ST 00:00 /RU SYSTEM /TR `"icacls \`"$path\`" /reset`""
            Note '  schtasks /Run /TN FixSshAcl'
            Note '  schtasks /Delete /TN FixSshAcl /F'
            Note 'then run this script again.'
            exit 1
        }
    } else {
        New-Item -ItemType File -Path $path -Force | Out-Null
    }

    $existing = @()
    try {
        $existing = @(Get-Content -LiteralPath $path -ErrorAction Stop)
    } catch {
        Fail "cannot read $name : $($_.Exception.Message)"
        exit 1
    }

    $dupe = $false
    foreach ($line in $existing) {
        if (-not ($line -match '\S')) { continue }
        if ((($line -split '\s+' | Select-Object -First 2) -join ' ') -eq $keyBody) { $dupe = $true; break }
    }
    if ($dupe) {
        Ok "$name`: key already present"
    } else {
        # Append as bytes: the file may be ASCII today and must stay that way,
        # and Set-Content would happily add a BOM on some hosts.
        try {
            $text = (@($existing | Where-Object { $_ -match '\S' }) + $key) -join "`r`n"
            [IO.File]::WriteAllText($path, $text + "`r`n", (New-Object System.Text.ASCIIEncoding))
            Ok "$name`: key appended"
        } catch {
            Fail "cannot write $name : $($_.Exception.Message)"
            exit 1
        }
    }

    # sshd refuses a key file that anyone but SYSTEM / Administrators can write.
    #
    # One atomic Set-Acl, never "icacls /inheritance:r" followed by grants.
    # That two-step form was the first version of this script and it bricked the
    # file on a real host: /inheritance:r drops every ACE, and when the very
    # next grant failed the file was left with an EMPTY DACL -- no DELETE, no
    # WRITE_DAC for anyone, not even its own owner. It then could not be
    # deleted, renamed, takeown'd or icacls'd, and only a SYSTEM-context task
    # could repair it. Set-Acl replaces the whole DACL in one operation: either
    # the file ends up with exactly these two ACEs, or it keeps what it had.
    #
    # SIDs, not names: on a Chinese-locale host the group is not "Administrators".
    $sidList = @('S-1-5-18')
    if ($aclOwnerSid) { $sidList += $aclOwnerSid }
    try {
        $acl = Get-Acl -LiteralPath $path -ErrorAction Stop
        if ($acl.Access.Count -eq 0) {
            # Already bricked by an older run. Say so instead of pretending.
            Fail "$([IO.Path]::GetFileName($path)): ACL is empty (no one has access)"
            Note 'this is what the old icacls-based version could produce;'
            Note 'fix it from a SYSTEM context, e.g. a scheduled task running:'
            Note "  icacls `"$path`" /reset"
            exit 1
        }
        $acl.SetAccessRuleProtection($true, $false)
        foreach ($rule in @($acl.Access)) { [void]$acl.RemoveAccessRule($rule) }
        $noneInherit = [System.Security.AccessControl.InheritanceFlags]::None
        $noneProp    = [System.Security.AccessControl.PropagationFlags]::None
        $allowType   = [System.Security.AccessControl.AccessControlType]::Allow
        foreach ($sid in $sidList) {
            $sidObj = New-Object System.Security.Principal.SecurityIdentifier($sid)
            $rule = New-Object System.Security.AccessControl.FileSystemAccessRule(
                $sidObj, 'FullControl', $noneInherit, $noneProp, $allowType)
            $acl.AddAccessRule($rule)
        }
        Set-Acl -LiteralPath $path -AclObject $acl -ErrorAction Stop
        Ok "$([IO.Path]::GetFileName($path)): ACL = $($sidList -join ' + ') full control, inheritance off"
    } catch {
        Fail "could not set the ACL on $([IO.Path]::GetFileName($path)): $($_.Exception.Message)"
        Note 'sshd will refuse this key file until only SYSTEM and Administrators hold it.'
        exit 1
    }
    $readback = @(Get-Acl -LiteralPath $path).Access |
        ForEach-Object { "$($_.IdentityReference)=$($_.FileSystemRights)" }
    Note ("  " + ($readback -join '; '))
}

# Administrators authenticate against administrators_authorized_keys on Windows.
# A non-admin console user needs the per-user file instead, so write both when
# the console user is not in the Administrators group.
Add-AuthorizedKey $AdminKeys $AdminsSid

$consoleUser = $null
$consoleName = (Get-CimInstance Win32_ComputerSystem).UserName
if ($consoleName) {
    $consoleUser = ($consoleName -split '\\')[-1]
    $account = Get-LocalUser -Name $consoleUser -ErrorAction SilentlyContinue
    if ($account) {
        $isMember = $false
        try {
            $isMember = @(Get-LocalGroupMember -SID $AdminsSid -ErrorAction Stop |
                Where-Object { $_.SID.Value -eq $account.SID.Value }).Count -gt 0
        } catch {
            Note "could not read group membership ($($_.Exception.Message)); assuming not an administrator"
        }
        if ($isMember) {
            Ok "$consoleUser is in Administrators - administrators_authorized_keys covers it"
        } else {
            $userSsh = Join-Path $env:SystemDrive "Users\$consoleUser\.ssh"
            Note "$consoleUser is NOT an administrator - also writing the per-user file"
            if (-not (Test-Path $userSsh)) { New-Item -ItemType Directory -Force -Path $userSsh | Out-Null }
            Add-AuthorizedKey (Join-Path $userSsh 'authorized_keys') $null
        }
    }
}

# ---- 4. firewall -----------------------------------------------------------
Head '4/6 firewall'

$ourRule = 'cloud-stream ssh 22'

# Same name = same rule; delete first so repeated runs do not stack up rules
# (netsh/NRD add rather than replace, and a stale rule with a wider range would
# quietly win).
Get-NetFirewallRule -DisplayName $ourRule -ErrorAction SilentlyContinue |
    Remove-NetFirewallRule -ErrorAction SilentlyContinue

# 127.0.0.1 only -- NOT ::1. New-NetFirewallRule rejects a rule scoped to the
# IPv6 loopback ("the argument is not valid: multicast, broadcast or loopback
# IPv6 address"), which used to abort this whole step. The self-test below
# connects over 127.0.0.1, so ::1 buys nothing here.
$allowed = @($ClientAddress, '127.0.0.1') | Where-Object { $_ } | Select-Object -Unique
try {
    New-NetFirewallRule -DisplayName $ourRule `
        -Direction Inbound -Action Allow -Protocol TCP -LocalPort 22 `
        -RemoteAddress $allowed -Profile Any -ErrorAction Stop | Out-Null
    Ok "$ourRule -> TCP 22 from $($allowed -join ', ')"
} catch {
    Fail "could not create the inbound rule: $($_.Exception.Message)"
    Note "the allowed source was: $($allowed -join ', ')"
    Note 'if that is not a plain IPv4 address or subnet, fix -ClientAddress:'
    Note '  enable-ssh.cmd -ClientAddress 192.168.59.146'
    exit 1
}

# The stock rules allow 22 from anywhere. Leaving one enabled makes the rule
# above pointless, so disable them and say so -- a future reader needs to know
# why "OpenSSH SSH Server (sshd)" shows as disabled.
foreach ($stockName in 'OpenSSH SSH Server (sshd)', 'OpenSSH-Server-In-TCP') {
    # -DisplayName cannot be combined with -Direction: they live in different
    # parameter sets and the call fails outright. Filter afterwards.
    $stock = @(Get-NetFirewallRule -DisplayName $stockName -ErrorAction SilentlyContinue |
               Where-Object { $_.Direction -eq 'Inbound' })
    if ($stock.Count -eq 0) { continue }
    $stock | Disable-NetFirewallRule -ErrorAction SilentlyContinue
    Note "disabled stock rule '$stockName' (it allowed every source)"
}

# ---- 5. config, then prove the key works -----------------------------------
Head '5/6 sshd_config + self-test'

if (-not (Test-Path $SshdConfig)) {
    Fail "no sshd_config at $SshdConfig"
    exit 1
}

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$script:Backup = "$SshdConfig.bak-$stamp"
Copy-Item -LiteralPath $SshdConfig -Destination $script:Backup -Force
Note "backup: $script:Backup"

# Writes one directive into the GLOBAL part of sshd_config.
#
# "Append at the end of the file" is wrong on Windows, and silently so. The stock
# sshd_config ends with:
#
#     Match Group administrators
#            AuthorizedKeysFile __PROGRAMDATA__/ssh/administrators_authorized_keys
#
# So the end of the file is inside a Match block. A directive written there is
# conditional: it applies only to connections that match, and `sshd -T` -- which
# reports the global values -- keeps printing the old one. Measured on .150
# (2026-11-10): the appended line landed at line 89, right after the Match block,
# and `sshd -T` still answered `kbdinteractiveauthentication yes`. A script that
# reports "key-only" while the door is open is worse than one that fails.
#
# So: replace the first occurrence in the global section, disable every later one
# (including copies stranded inside a Match block), and if there is none, insert
# immediately BEFORE the first Match line rather than at EOF.
function Set-SshdOption ($lines, $name, $value) {
    $rx = "^\s*#*\s*$([regex]::Escape($name))\s+\S"

    $firstMatch = $lines.Count
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if ($lines[$i] -match '^\s*Match\s') { $firstMatch = $i; break }
    }

    $applied = -1
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if ($lines[$i] -notmatch $rx) { continue }
        if ($applied -eq -1 -and $i -lt $firstMatch) {
            $lines[$i] = "$name $value"
            $applied = $i
        } else {
            # duplicate, or a copy that only applies to some connections
            $lines[$i] = "# $($lines[$i].TrimStart())"
        }
    }

    if ($applied -eq -1) {
        $out = New-Object System.Collections.ArrayList
        for ($i = 0; $i -lt $firstMatch; $i++) { [void]$out.Add($lines[$i]) }
        [void]$out.Add("$name $value")
        for ($i = $firstMatch; $i -lt $lines.Count; $i++) { [void]$out.Add($lines[$i]) }
        return ,$out.ToArray()
    }
    return ,$lines
}

$lines = @(Get-Content -LiteralPath $SshdConfig)
$lines = Set-SshdOption $lines 'PubkeyAuthentication' 'yes'
# PasswordAuthentication is touched in step 6 only, after the key has been
# proven. Writing "no" here would leave a host nobody can enter if the key is
# wrong, which is the one failure this script exists to avoid.

$utf8 = New-Object System.Text.UTF8Encoding($false)
[IO.File]::WriteAllText($SshdConfig, (($lines -join "`r`n") + "`r`n"), $utf8)

# A config that does not parse means sshd will not start at all. -T parses and
# prints the effective values without binding a port.
#
# Invoke-Native rather than `& ... 2>&1`: a config warning would arrive as a
# NativeCommandError and, under $ErrorActionPreference='Stop', abort the script
# in the middle of a step that has already rewritten sshd_config.
$effective = Invoke-Native $sshdExe @('-T', '-f', $SshdConfig)
if ($effective.ExitCode -ne 0) {
    Warn 'sshd -T rejected the config; restoring the backup'
    ($effective.Combined | Select-Object -First 5) | ForEach-Object { Say "    $_" }
    Copy-Item -LiteralPath $script:Backup -Destination $SshdConfig -Force
    Fail 'left sshd_config untouched'
    exit 1
}
Ok 'config parses'

Restart-Service sshd
Start-Sleep -Seconds 2
if ((Get-ServiceState 'sshd').State -ne 'Running') {
    Warn 'sshd did not come back; restoring the backup'
    Copy-Item -LiteralPath $script:Backup -Destination $SshdConfig -Force
    Start-Service sshd -ErrorAction SilentlyContinue
    Fail 'sshd failed to start with the new config'
    exit 1
}
Ok 'sshd restarted'

$sshCli = Get-SshClientExe
$selfTestPassed = $false
$tempKey = $null
$tempDir = $null

# sshd's own log on Windows is the event log; the file under logs\ only exists
# if sshd_config was told to write one. Reporting the wrong one means "no
# sshd.log" at the exact moment the operator needs the reason.
function Show-SshdLog {
    $file = Join-Path $LogDir 'sshd.log'
    if (Test-Path $file) {
        Note "sshd log tail ($file):"
        (Get-Content -LiteralPath $file -Tail 12) | ForEach-Object { Say "    $_" }
        return
    }
    try {
        $ev = @(Get-WinEvent -LogName 'OpenSSH/Operational' -MaxEvents 10 -ErrorAction Stop)
        Note 'sshd log tail (event log OpenSSH/Operational):'
        $ev | ForEach-Object {
            Say ("    {0}  {1}" -f $_.TimeCreated.ToString('HH:mm:ss'), $_.Message)
        }
    } catch {
        Say '    (no sshd.log and no OpenSSH/Operational events)'
    }
}

if ($NoSelfTest) {
    Warn 'self-test skipped (-NoSelfTest); password auth left as it was'
} elseif (-not $sshCli) {
    Warn 'no ssh.exe on this host, so the key cannot be proven here'
    Note 'install "OpenSSH Client" or verify from the build machine before'
    Note 'trusting this host; password auth left as it was'
} else {
    # The private half of the operator's key is on the BUILD MACHINE, not here,
    # so this host cannot log in with it. An earlier version tried anyway and
    # always failed with "Permission denied (publickey,...)" -- the client had
    # no key to offer at all.
    #
    # What can be proven locally is the half that actually breaks in practice:
    # that sshd reads THIS key file and accepts its ACL. So generate a
    # throwaway pair, let it through the same door, and remove it afterwards.
    # Sweep first, then create: a previous run that died mid-probe leaves both a
    # key line and a directory holding the private half. Healing them here means
    # "run it again" is always the right answer, never "now go clean up by hand".
    $leftLines = Remove-ProbeKeyLines
    $leftDirs  = Remove-StaleProbeDirs
    if ($leftLines -gt 0) { Ok "removed $leftLines leftover self-test key line(s)" }
    if ($leftDirs  -gt 0) { Ok "removed $leftDirs leftover self-test key dir(s) from %TEMP%" }

    $tempDir = Join-Path $env:TEMP ('cs-sshprobe-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    $tempKey = Join-Path $tempDir 'id_probe'
    New-Item -ItemType Directory -Force -Path $tempDir | Out-Null
    $keygen = Join-Path (Split-Path $sshdExe) 'ssh-keygen.exe'
    # -C tags the throwaway key so the cleanup below can find exactly that line
    # and nothing else. Without it the comment is user@host, which is also what
    # a real key might carry.
    #
    # The empty passphrase needs `-N ""` ON THE COMMAND LINE, and that is
    # surprisingly hard to produce from PowerShell:
    #   -N ''    -> with pwsh 7 gives an empty passphrase, with PowerShell 5.1
    #              (which runs this script) it gives a non-empty one
    #   -N '""'  -> passes the two quote characters as the passphrase
    # Both produced an encrypted key, and the encrypted key silently broke the
    # self-test: ssh would offer the public half, the server would accept it,
    # and then the client could not sign -- the connection just reset, with no
    # useful message anywhere. So: one command-line string built with an explicit
    # quote character, handed to Start-Process as a single argument.
    $quote = [char]34
    $argLine = '-t ed25519 -N {0}{0} -q -C cs-sshprobe -f {0}{1}{0}' -f $quote, $tempKey
    $gen = Start-Process -FilePath $keygen -ArgumentList $argLine -Wait -PassThru -NoNewWindow
    if ($gen.ExitCode -ne 0 -or -not (Test-Path "$tempKey.pub")) {
        Warn "could not generate a throwaway key (ssh-keygen exit $($gen.ExitCode)); skipping the self-test"
        $tempKey = $null
    } elseif (-not (Test-KeyIsUnprotected $tempKey)) {
        Warn 'the throwaway key came out passphrase-protected; skipping the self-test'
        Note 'this is a PowerShell argument-passing problem, not an sshd one'
        $tempKey = $null
    } else {
        $probePub = (Get-Content -LiteralPath "$tempKey.pub" -Raw).Trim()
        Note 'probing key auth with a throwaway key (the real key is on the build machine)'
        try {
            [IO.File]::AppendAllText($AdminKeys, $probePub + "`r`n", (New-Object System.Text.ASCIIEncoding))

            # LogLevel=ERROR matters for more than tidiness: at the default level
            # ssh writes "Warning: Permanently added ... to the list of known
            # hosts." to stderr even on success, and that line is what used to
            # kill this script before the exit code was read.
            $probe = Invoke-Native $sshCli @(
                '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=no',
                '-o', 'UserKnownHostsFile=NUL', '-o', 'ConnectTimeout=8',
                '-o', 'LogLevel=ERROR',
                '-o', 'PreferredAuthentications=publickey',
                '-i', "`"$tempKey`"",
                "$env:USERNAME@127.0.0.1", 'echo CS_SSH_SELFTEST_OK')

            if ($probe.ExitCode -eq 0 -and ($probe.Combined -join ' ') -match 'CS_SSH_SELFTEST_OK') {
                $selfTestPassed = $true
                Ok 'key login works (throwaway key accepted through the real config)'
            } else {
                Warn "key login failed (exit $($probe.ExitCode))"
                ($probe.Combined | Select-Object -First 8) | ForEach-Object { Say "    $_" }
                Show-SshdLog
                if ($KeepPassword) {
                    Fail 'left as is: password auth still on, key auth not working'
                } else {
                    Warn 'not turning password auth off - that would lock the host'
                    $script:ExitCode = 1
                }
            }
        } finally {
            # Close the key line up front when the probe failed, keep the key
            # file until step 6 has re-tested with it, and rely on the
            # script-level trap above for anything that throws in between.
            if (-not $selfTestPassed) { [void](Remove-ProbeKeyLines) }
        }
    }
}

# ---- 6. key-only, only if proven -------------------------------------------
Head '6/6 key-only mode'

if ($KeepPassword) {
    Note 'kept (-KeepPassword or not proven)'
} elseif ($selfTestPassed) {
    $lines = @(Get-Content -LiteralPath $SshdConfig)
    $lines = Set-SshdOption $lines 'PasswordAuthentication' 'no'
    # PasswordAuthentication alone is NOT key-only, and the report used to say
    # "key-only" anyway. .150 disproved it on 2026-11-10: after the run,
    # `sshd -T` still printed `kbdinteractiveauthentication yes`, and the world
    # could see it -- a client trying `-o PubkeyAuthentication=no` is offered
    # "(publickey,keyboard-interactive)". On Windows keyboard-interactive is
    # implemented on top of the same account password, so leaving it on keeps a
    # password door open right next to the locked one.
    $lines = Set-SshdOption $lines 'KbdInteractiveAuthentication' 'no'
    [IO.File]::WriteAllText($SshdConfig, (($lines -join "`r`n") + "`r`n"), $utf8)

    # NOT $check: the script has a [switch]$Check parameter, PowerShell variable
    # names are case-insensitive, and assigning this command's output (an array
    # of lines) to a switch fails with "cannot convert System.Object[] to
    # SwitchParameter" -- a message that points nowhere near the real cause.
    $cfgCheck = Invoke-Native $sshdExe @('-T', '-f', $SshdConfig)
    if ($cfgCheck.ExitCode -ne 0) {
        Warn 'sshd -T rejected the password-off config; restoring the backup'
        ($cfgCheck.Combined | Select-Object -First 5) | ForEach-Object { Say "    $_" }
        Copy-Item -LiteralPath $script:Backup -Destination $SshdConfig -Force
        Restart-Service sshd
        Fail 'password auth unchanged'
        Clear-ProbeArtifacts
        exit 1
    }
    # Reading the config back is not enough: the directive may be spelled the old
    # way (ChallengeResponseAuthentication) or land under a Match block. Ask sshd
    # what it will actually enforce, and refuse to call this "key-only" until
    # both door handles are gone.
    $stillOpen = @()
    foreach ($k in 'passwordauthentication', 'kbdinteractiveauthentication') {
        $v = $cfgCheck.Out | Where-Object { $_ -match "^$k\s" } | Select-Object -First 1
        if (-not $v -or $v -notmatch '\sno\s*$') { $stillOpen += $k }
    }
    if ($stillOpen.Count -gt 0) {
        Warn ("sshd still accepts: " + ($stillOpen -join ', '))
        Note 'that is the difference between "we asked" and "it is off"'
        Copy-Item -LiteralPath $script:Backup -Destination $SshdConfig -Force
        Restart-Service sshd
        Fail 'not key-only; config restored'
        Clear-ProbeArtifacts
        exit 1
    }
    Restart-Service sshd
    Start-Sleep -Seconds 2

    if ($sshCli -and $tempKey) {
        $again = Invoke-Native $sshCli @(
            '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=no',
            '-o', 'UserKnownHostsFile=NUL', '-o', 'ConnectTimeout=8',
            '-o', 'LogLevel=ERROR',
            '-o', 'PreferredAuthentications=publickey',
            '-i', "`"$tempKey`"",
            "$env:USERNAME@127.0.0.1", 'echo CS_SSH_SELFTEST_OK')
        if ($again.ExitCode -eq 0) {
            Ok 'password auth off, key login still works'
        } else {
            Warn "key login broke after disabling passwords (exit $($again.ExitCode)); restoring the backup"
            ($again.Combined | Select-Object -First 5) | ForEach-Object { Say "    $_" }
            Copy-Item -LiteralPath $script:Backup -Destination $SshdConfig -Force
            Restart-Service sshd
            Fail 'rolled back'
            Clear-ProbeArtifacts
            exit 1
        }
    } else {
        Note 'password auth off (not re-tested: no local ssh client)'
    }
} else {
    Note 'not changed: the key was never proven here'
}

# ---- remove the throwaway key ----------------------------------------------
# It proved the door opens; it must not stay in the key file. The private half
# is in %TEMP% and is deleted too, so even a leftover line would be inert --
# but a stale line in authorized_keys is exactly the kind of thing that gets
# found a year later and taken for a backdoor.
if ($tempKey) {
    $hadLine = (Test-Path $AdminKeys) -and
               (@(Get-Content -LiteralPath $AdminKeys) -match 'cs-sshprobe').Count -gt 0
    Clear-ProbeArtifacts
    if ($hadLine) { Ok 'throwaway key removed from administrators_authorized_keys' }
}

# ---- report ----------------------------------------------------------------
Say ''
Say '======================================================================'
Say ' result'
Say '======================================================================'

$svc = Get-ServiceState 'sshd'
Ok "service: $($svc.State) / $($svc.StartMode)"

$listen = @(Get-NetTCPConnection -LocalPort 22 -State Listen -ErrorAction SilentlyContinue)
if ($listen.Count -gt 0) { Ok "listening: $($listen[0].LocalAddress):22" } else { Fail 'not listening on 22' }

$effOut = Invoke-Native $sshdExe @('-T', '-f', $SshdConfig)
$eff = $effOut.Out
# Three knobs, not two: the report is what the operator trusts, so it must show
# the one that decides whether a password can still get in.
$openDoors = @()
foreach ($k in 'pubkeyauthentication', 'passwordauthentication', 'kbdinteractiveauthentication') {
    $v = $eff | Where-Object { $_ -match "^$k\s" } | Select-Object -First 1
    if ($v) { Note $v.Trim() }
    if ($k -ne 'pubkeyauthentication' -and (-not $v -or $v -notmatch '\sno\s*$')) { $openDoors += $k }
}
if ($openDoors.Count -gt 0 -and -not $KeepPassword) {
    Fail ("still accepts a password via: " + ($openDoors -join ', '))
    Note 'this is not key-only; keep passwords on purpose with -KeepPassword instead'
}
# -T without -C prints the GLOBAL AuthorizedKeysFile (".ssh/authorized_keys").
# Administrators are routed to the ProgramData file by a Match block, so the
# global line reads as "the admin key file was ignored" when it was not. Ask
# for the value that actually applies to the account that will log in.
$effUser = (Invoke-Native $sshdExe @('-T', '-C',
            "user=$($env:USERNAME),host=localhost,addr=127.0.0.1",
            '-f', $SshdConfig)).Out
$akf = $effUser | Where-Object { $_ -match '^authorizedkeysfile\s' } | Select-Object -First 1
if ($akf) { Note "for $($env:USERNAME): $($akf.Trim())" }

foreach ($r in @(Get-NetFirewallRule -DisplayName $ourRule -ErrorAction SilentlyContinue)) {
    $addr = ($r | Get-NetFirewallAddressFilter).RemoteAddress
    Ok "rule: $($r.DisplayName) enabled=$($r.Enabled) from=$($addr -join ',')"
}

$acl = @(& "$env:SystemRoot\System32\icacls.exe" $AdminKeys)
Note 'administrators_authorized_keys ACL:'
($acl | Select-Object -First 4) | ForEach-Object { Say "    $($_.Trim())" }
Say ''   # icacls ends without a newline; without this the next line runs on

# The account matters: an account with a blank password cannot log in over the
# network at all (LimitBlankPasswordUse), key or no key. Detect and say so
# rather than let the build machine hit a wall.
$blankPolicy = (Get-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\Lsa' -Name LimitBlankPasswordUse -ErrorAction SilentlyContinue)
if ($blankPolicy -and $blankPolicy.LimitBlankPasswordUse -eq 1) {
    $console = (Get-CimInstance Win32_ComputerSystem).UserName
    if ($console) {
        Note ("console account $console - if it has a BLANK password, network logon is")
        Note 'refused by policy (LimitBlankPasswordUse=1) even with a valid key.'
        Note 'check from the build machine; only then set a password.'
    }
}

Say ''
Say "next, on ${ClientAddress}:"
Say "  ssh -o PreferredAuthentications=publickey $env:USERNAME@<this-host>"
Say '  (add a Host entry in ~/.ssh/config to use a short alias)'
Say ''
Say "config backup: $script:Backup"
Say "sshd log:      $LogDir\sshd.log"
Say ''
if ($script:ExitCode -eq 0) { Say 'OK' } else { Say 'FAILED - see the [fail] lines above' }
exit $script:ExitCode
