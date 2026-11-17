# Seed the games\ tree on the controlled machine (S1 of the game-hosting work).
#
# Deliberately ASCII-only: PowerShell 5.1 reads a BOM-less .ps1 as ANSI, so any
# non-ASCII comment would turn into mojibake in the console output.
#
# What this does NOT do: move or delete anything under the user's Desktop. The
# existing .minecraft stays where it is and is used read-only as the client;
# only the *saves* are copied into the new writable root, so the new layout can
# be exercised without touching the original.
param(
    [string]$Root = 'C:\Program Files\cloud-stream\games',
    [string]$ClientDir = 'C:\Users\Admin\Desktop\.minecraft',
    [string]$Java = 'C:\mc\jre8\bin\javaw.exe',
    [string]$Version = '1.12.2-Forge_14.23.5.2864',
    [string]$InstanceId = 'mc-1.12.2',
    [switch]$NoCopySaves
)

$ErrorActionPreference = 'Stop'

foreach ($d in @('clients', 'saves', 'instances', 'runtime')) {
    New-Item -ItemType Directory -Force -Path (Join-Path $Root $d) | Out-Null
}
$savesRoot = Join-Path $Root 'saves\default'
New-Item -ItemType Directory -Force -Path $savesRoot | Out-Null

if (-not $NoCopySaves -and (Test-Path (Join-Path $ClientDir 'saves'))) {
    # /E copies subdirs incl. empty ones; the summary lines are noise here.
    & robocopy (Join-Path $ClientDir 'saves') (Join-Path $savesRoot 'saves') /E /NFL /NDL /NJH /NJS /NP | Out-Null
    Write-Output ("saves copied: exit=" + $LASTEXITCODE)
}
if (Test-Path (Join-Path $ClientDir 'options.txt')) {
    Copy-Item (Join-Path $ClientDir 'options.txt') (Join-Path $savesRoot 'options.txt') -Force
    Write-Output 'options.txt copied'
}

# mods / config / resourcepacks live under --gameDir in 1.12.2, NOT next to the
# version jar. A "read-only client + writable saves" split therefore silently
# drops every mod (the first run after splitting loaded 4 mods instead of 5 --
# CustomNPCs was simply not on the class path's mod scan). Junction them so the
# client stays the single source of truth: updating a jar in the client updates
# the instance, without copying hundreds of megabytes.
foreach ($name in @('mods', 'config', 'resourcepacks')) {
    $src = Join-Path $ClientDir $name
    if (-not (Test-Path $src)) { continue }
    $dst = Join-Path $savesRoot $name
    if (Test-Path $dst) {
        $item = Get-Item $dst -Force
        if ($item.LinkType -eq 'Junction') {
            Write-Output "junction ok: $name -> $($item.Target)"
            continue
        }
        Remove-Item $dst -Recurse -Force
    }
    New-Item -ItemType Junction -Path $dst -Target $src | Out-Null
    Write-Output "junction made: $name -> $src"
}

$spec = [ordered]@{
    schema     = 1
    name       = 'Minecraft 1.12.2 Forge'
    client_dir = $ClientDir
    saves_dir  = 'saves\default'
    java       = [ordered]@{ path = $Java; heap_mb = 2048; jvm_args = @() }
    game       = [ordered]@{
        version     = $Version
        main_class  = 'net.minecraft.launchwrapper.Launch'
        tweak_class = 'net.minecraftforge.fml.common.launcher.FMLTweaker'
        player      = 'CloudPlayer'
        game_args   = @()
    }
    display    = [ordered]@{ model = 'windowed'; width = 0; height = 0; x = 0; y = 0 }
    lifecycle  = [ordered]@{ stop_timeout_sec = 15; restart_max = 0; restart_backoff_sec = 5 }
}
$json = ($spec | ConvertTo-Json -Depth 8)
$instPath = Join-Path $Root ("instances\$InstanceId.json")
[IO.File]::WriteAllText($instPath, $json, (New-Object System.Text.UTF8Encoding $false))
Write-Output "instance written: $instPath"

# Report what the tree looks like now, so the next step is not a guess.
Get-ChildItem $Root -Directory | ForEach-Object {
    $n = (Get-ChildItem $_.FullName -Recurse -File -ErrorAction SilentlyContinue | Measure-Object).Count
    Write-Output ("  {0}\  files={1}" -f $_.Name, $n)
}
Write-Output ("version json present: " + (Test-Path (Join-Path $ClientDir "versions\$Version\$Version.json")))
Write-Output ("java present: " + (Test-Path $Java))
