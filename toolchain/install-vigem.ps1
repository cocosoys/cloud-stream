<#
    One-time ViGEmBus setup for a controlled Windows machine (#17).

    Runs on the target, Windows PowerShell 5.1 compatible. ASCII only: PS 5.1
    reads a BOM-less .ps1 as ANSI, so non-ASCII comments would turn to mojibake.

    Steps:
      1. Install ViGEmBus 1.22.0 if the service is not there (/qn, no reboot).
      2. Pre-stage the in-box xusb22.inf into the DriverStore.
      3. Report what the machine actually sees.

    Step 2 is the one that is easy to miss and expensive to diagnose. Windows 11
    delivers the Xbox 360 controller driver on demand over Windows Update; a VM
    without internet never gets it, so the virtual controller's child device
    stays "needs further installation", VIGEM_WAIT_DEVICE_READY never returns,
    and vigem_target_add fails with VIGEM_ERROR_TARGET_NOT_PLUGGED_IN (a
    secondary error that hides the real cause). Staging the INF first makes the
    child bind on creation, no reboot involved.

    Usage:
      powershell -NoProfile -ExecutionPolicy Bypass -File install-vigem.ps1 [-Setup <path>]
#>
[CmdletBinding()]
param(
    [string]$Setup = 'C:\dev\deploy\ViGEmBusSetup.exe'
)

$ErrorActionPreference = 'Continue'
function Say($t) { Write-Output $t }
function Bad($t) { Write-Output ('FAIL ' + $t); exit 1 }

# ---- 1 ViGEmBus -----------------------------------------------------------
$svc = Get-Service ViGEmBus -ErrorAction SilentlyContinue
if ($svc) {
    Say ('OK   ViGEmBus already installed (' + $svc.Status + '/' + $svc.StartType + ')')
} else {
    if (-not (Test-Path $Setup)) {
        Bad ("$Setup missing - copy ViGEmBus_1.22.0_x64_x86_arm64.exe there first " +
             '(https://github.com/nefarius/ViGEmBus/releases/download/v1.22.0/)')
    }
    Say 'STEP installing ViGEmBus'
    $p = Start-Process -FilePath $Setup -ArgumentList '/qn', '/norestart' -Wait -PassThru
    Say ('     installer exit ' + $p.ExitCode)
    # 3010 = success, reboot required. The bus itself starts without one.
    if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) { Bad ('installer exit ' + $p.ExitCode) }
}

# ---- 2 xusb22 into the DriverStore ---------------------------------------
$inf = 'C:\Windows\INF\xusb22.inf'
if (-not (Test-Path $inf)) { Bad 'xusb22.inf is not in C:\Windows\INF - not a supported Windows build?' }

$staged = (pnputil /enum-drivers) | Select-String -Pattern 'xusb22\.inf'
if ($staged) {
    Say 'OK   xusb22 already staged'
} else {
    Say 'STEP staging xusb22 (the Xbox 360 child driver)'
    $out = pnputil /add-driver $inf /install 2>&1
    if ($LASTEXITCODE -ne 0) { $out | ForEach-Object { Say ('     ' + $_) }; Bad 'pnputil /add-driver failed' }
    Say 'OK   xusb22 staged'
}

# ---- 3 report -------------------------------------------------------------
$svc = Get-Service ViGEmBus -ErrorAction SilentlyContinue
if ($svc) { Say ('SVC  ' + $svc.Status + '/' + $svc.StartType) } else { Bad 'ViGEmBus service missing after install' }
$bus = Get-PnpDevice -FriendlyName '*Virtual Gamepad Emulation Bus*' -ErrorAction SilentlyContinue
if ($bus) { Say ('BUS  ' + $bus.Status + ' | ' + $bus.FriendlyName) } else { Say 'BUS  not enumerated yet (a reboot may be needed)' }
Say 'DONE run: cargo test --release -p stream-server hardware_roundtrip -- --ignored --nocapture'
