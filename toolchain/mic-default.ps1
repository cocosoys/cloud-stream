# Read/write the Windows default *recording* device, independently of
# stream-server.
#
# Why this exists:
#   * P1 makes the server switch the default capture endpoint to the virtual
#     cable. Verifying that needs a second opinion -- asking the same code that
#     did the switch "did it work?" is not evidence.
#   * The server is stopped with Stop-Process -Force (deploy, watchdog, reboot),
#     so it never gets to restore. This script is the manual way back.
#
# ASCII only on purpose: Windows PowerShell 5.1 reads a BOM-less file as ANSI,
# so a Chinese comment here would be mojibake before the parser ever sees it.
# Device names printed at runtime come from the registry and are fine.
#
# Examples:
#   .\mic-default.ps1 -List
#   .\mic-default.ps1 -Name "CABLE Output"
#   .\mic-default.ps1 -Name "High Definition"
#   .\mic-default.ps1 -Id "{0.0.1.00000000}.{9fccccc3-e768-4042-8c53-e7416e511def}"

param(
    [switch]$List,
    [string]$Name = '',
    [string]$Id = ''
)

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class PolicyConfig
{
    // Undocumented, but stable since Vista. The vtable order is the contract:
    // SetDefaultEndpoint is the 12th method after the three IUnknown slots.
    // The placeholders are never called -- they only reserve their slot.
    [ComImport, Guid("870af99c-171d-4f9e-af0d-e63df40c2bc9")]
    private class CPolicyConfigClient { }

    [ComImport, Guid("f8679f50-850a-41cf-9c72-430f290290c8"),
     InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IPolicyConfig
    {
        void GetMixFormat();
        void GetDeviceFormat();
        void ResetDeviceFormat();
        void SetDeviceFormat();
        void GetProcessingPeriod();
        void SetProcessingPeriod();
        void GetShareMode();
        void SetShareMode();
        void GetPropertyValue();
        void SetPropertyValue();
        void SetDefaultEndpoint([MarshalAs(UnmanagedType.LPWStr)] string deviceId, int role);
        void SetEndpointVisibility();
    }

    // role: 0 console, 1 multimedia, 2 communications. All three, because
    // Discord / Steam voice read the communications role specifically.
    public static void SetDefault(string deviceId)
    {
        var cfg = (IPolicyConfig)new CPolicyConfigClient();
        try
        {
            for (int role = 0; role < 3; role++)
            {
                cfg.SetDefaultEndpoint(deviceId, role);
            }
        }
        finally
        {
            Marshal.ReleaseComObject(cfg);
        }
    }
}
'@

function Get-CaptureDevices {
    $root = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Capture'
    $nameKey = '{a45c254e-df1c-4efd-8020-67d146a850e0},2'
    Get-ChildItem $root | ForEach-Object {
        $guid = $_.PSChildName
        $state = (Get-ItemProperty -Path $_.PSPath -Name DeviceState -ErrorAction SilentlyContinue).DeviceState
        $props = Join-Path $_.PSPath 'Properties'
        $friendly = (Get-ItemProperty -Path $props -Name $nameKey -ErrorAction SilentlyContinue).$nameKey
        [pscustomobject]@{
            # IMMDevice::GetId prefixes capture endpoints with 0.0.1; render
            # endpoints use 0.0.0. Getting this wrong is just an E_INVALIDARG.
            Id    = "{0.0.1.00000000}.$guid"
            Name  = $friendly
            State = $state
        }
    }
}

$devices = @(Get-CaptureDevices)

if ($List -or (-not $Name -and -not $Id)) {
    if ($devices.Count -eq 0) {
        Write-Output 'no capture endpoints found'
    } else {
        # DeviceState: 1 active, 2 disabled, 4 not present, 8 unplugged.
        $devices | Select-Object @{n = 'State'; e = { $_.State } }, Name, Id | Format-Table -AutoSize | Out-String -Width 200
    }
    if (-not $Name -and -not $Id) {
        Write-Output 'pass -Name <substring> or -Id <endpoint id> to switch'
        exit 0
    }
    exit 0
}

$targetId = $Id
if (-not $targetId) {
    $hits = @($devices | Where-Object { $_.Name -and $_.Name -like "*$Name*" })
    if ($hits.Count -eq 0) {
        Write-Output "no capture endpoint matches '$Name'; run -List to see them"
        exit 1
    }
    if ($hits.Count -gt 1) {
        Write-Output "'$Name' matches $($hits.Count) endpoints; be more specific or use -Id"
        $hits | Select-Object Name, Id | Format-Table -AutoSize | Out-String -Width 200
        exit 1
    }
    $targetId = $hits[0].Id
    Write-Output ("target: " + $hits[0].Name)
}

[PolicyConfig]::SetDefault($targetId)
Write-Output ("switched all three roles to " + $targetId)
