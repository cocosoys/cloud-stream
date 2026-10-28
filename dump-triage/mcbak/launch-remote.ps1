# MC 1.12.2-Forge launcher with server autoconnect + fullscreen.
# Invoked by scheduled task MCGame so the window lands on the interactive
# desktop session (an SSH-started process runs in the service session and would
# render nowhere). Defaults are the current cloud-game target; override with
# -Server/-Port/-Fullscreen when uploading a variant.
param(
    [string]$Server  = '192.168.59.146',
    [int]   $Port    = 25574,
    [switch]$Fullscreen,
    [string]$GameUser = 'ClientRemote',
    [int]   $Width   = 0,
    [int]   $Height  = 0
)

$ErrorActionPreference = 'Stop'
$mc   = 'C:\Users\Administrator\Desktop\.minecraft'
$ver  = '1.12.2-Forge_14.23.5.2864'
$java = 'C:\dev\jdk8\jdk8u504-b01\bin\java.exe'
$log  = 'C:\dev\mc\mc-remote.log'
New-Item -ItemType Directory -Force -Path 'C:\dev\mc' | Out-Null

function Write-Log([string]$m) {
    "$(Get-Date -Format 'HH:mm:ss') $m" | Out-File $log -Encoding utf8 -Append
}

# ---- classpath: version json, artifact.path preferred, natives skipped.
# Same construction as start-mc.ps1, which is known good on this machine.
$json = Get-Content "$mc\versions\$ver\$ver.json" -Raw | ConvertFrom-Json
$cp = New-Object System.Collections.Generic.List[string]
$missing = New-Object System.Collections.Generic.List[string]
foreach ($lib in $json.libraries) {
    $path = $null
    if ($lib.downloads -and $lib.downloads.artifact -and $lib.downloads.artifact.path) {
        $path = $lib.downloads.artifact.path
    } elseif ($lib.name -match '^([^:]+):([^:]+):([^:]+)$') {
        $path = "libraries\$($Matches[1] -replace '\.','\')\$($Matches[2])\$($Matches[3])\$($Matches[2])-$($Matches[3]).jar"
    }
    if (-not $path) { continue }
    $full = Join-Path (Join-Path $mc 'libraries') $path
    if (Test-Path $full) { $cp.Add($full) } else { $missing.Add($path) }
}
$cp.Add("$mc\versions\$ver\$ver.jar")

"=== start $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') -> ${Server}:${Port} fullscreen=$([bool]$Fullscreen) ===" | Out-File $log -Encoding utf8
Write-Log "classpath_items=$($cp.Count) missing=$($missing.Count)"
if ($missing.Count -gt 0) { Write-Log "MISSING_FIRST=$($missing[0])" }

$classpath = ($cp -join ';')

$argList = @(
    '-Xmx2G', '-Xms512M',
    "-Djava.library.path=$mc\natives-tmp",
    '-cp', "`"$classpath`"",
    'net.minecraft.launchwrapper.Launch',
    "--username=$GameUser",
    "--version=$ver",
    "--gameDir=$mc",
    "--assetsDir=$mc\assets",
    '--assetIndex=1.12',
    '--uuid=00000000-0000-0000-0000-000000000000',
    '--accessToken=0',
    '--userType=legacy',
    '--versionType=Forge',
    '--tweakClass=net.minecraftforge.fml.common.launcher.FMLTweaker'
)
if ($Fullscreen)      { $argList += '--fullscreen=true' }
if ($Width -gt 0 -and $Height -gt 0) { $argList += "--width=$Width"; $argList += "--height=$Height" }
if ($Server)          { $argList += "--server=$Server"; $argList += "--port=$Port" }

# Single string: Start-Process splits arrays on spaces and would cut the
# classpath apart at 'Program Files'-style paths.
$argStr = ($argList -join ' ')
Write-Log "args_tail=$(($argList | Select-Object -Last 3) -join ' ')"

# #75: the sensitivity default is written by start-mc.ps1 before this script
# runs, yet options.txt came back with the old 0.2535211 after a launch, so
# something rewrites it between the write and the game's own load. Snapshot the
# line three times -- just before the JVM starts, once the window exists, and
# after the loading screens would have finished -- to see which step does it.
# Length is logged too: "0.5" and "0.2535211" differ by six bytes, so the size
# alone says which value is on disk even if the grep misses.
$optPath = Join-Path $mc 'options.txt'
function Get-SensSnapshot {
    if (-not (Test-Path $optPath)) { return '<missing options.txt>' }
    $bytes = (Get-Item $optPath).Length
    $hit = Select-String -Path $optPath -Pattern '^mouseSensitivity:' -ErrorAction SilentlyContinue |
        Select-Object -First 1
    $value = if ($hit) { $hit.Line.Trim() } else { '<absent>' }
    return "$value bytes=$bytes mtime=$((Get-Item $optPath).LastWriteTime.ToString('HH:mm:ss'))"
}
Write-Log "sens pre-launch : $(Get-SensSnapshot)"

$p = Start-Process -FilePath $java -ArgumentList $argStr -WorkingDirectory $mc -PassThru
Start-Sleep -Seconds 8
if ($p.HasExited) {
    Write-Log "MC_EXITED code=$($p.ExitCode)"
} else {
    Write-Log "PID=$($p.Id) OK"
    Write-Log "sens at +8s     : $(Get-SensSnapshot)"
    # MC rewrites options.txt once its own GameSettings finishes loading, which
    # lands after the resource reload; 25 s covers that on this host.
    Start-Sleep -Seconds 25
    Write-Log "sens at +33s    : $(Get-SensSnapshot)"
}
