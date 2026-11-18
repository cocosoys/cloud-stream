# Move the Minecraft client from the user profile (Desktop\.minecraft) into
# games\clients\<name>, then point the instance at the new place.
#
# Why this is a separate, opt-in step: it touches ~1.5 GB of user data that
# lives outside the deployment tree. Nothing else in the project does that, and
# it must stay reversible -- so the default is COPY + VERIFY + KEEP THE SOURCE.
# Purging the original needs -PurgeSource AND -Yes.
#
# What it rewrites:
#   * instances\<id>.json   "client_dir" only (targeted regex, so unknown fields,
#                           key order and formatting survive byte-for-byte)
#   * saves\<dir>\{mods,config,resourcepacks}
#                           junctions pointing into the OLD client. seed-games.ps1
#                           created them; after the move they dangle and the game
#                           silently loads zero mods.
#
# ASCII-only on purpose: PowerShell 5.1 reads a BOM-less .ps1 as ANSI, so any
# non-ASCII comment becomes mojibake in the console and in the exit message.
param(
    [string]$Root = 'C:\Program Files\cloud-stream\games',
    [string]$Source = 'C:\Users\Admin\Desktop\.minecraft',
    [string]$ClientName = '1.12.2-forge',
    [string]$InstanceId = 'mc-1.12.2',
    [switch]$PurgeSource,
    [switch]$Yes,
    [switch]$NoJunctions
)

$ErrorActionPreference = 'Stop'

function Say([string]$m) { Write-Output $m }

function Tree-Stats([string]$path) {
    if (-not (Test-Path -LiteralPath $path)) { return [pscustomobject]@{ Files = 0; Bytes = 0 } }
    $m = Get-ChildItem -LiteralPath $path -Recurse -File -Force -ErrorAction SilentlyContinue |
        Measure-Object -Property Length -Sum
    [pscustomobject]@{ Files = [int]$m.Count; Bytes = [long]($m.Sum) }
}

function Top-Level-Stats([string]$path) {
    $out = @{}
    Get-ChildItem -LiteralPath $path -Directory -Force -ErrorAction SilentlyContinue | ForEach-Object {
        $s = Tree-Stats $_.FullName
        $out[$_.Name] = "$($s.Files)/$($s.Bytes)"
    }
    $out
}

if (-not (Test-Path -LiteralPath $Source)) { throw "source not found: $Source" }
New-Item -ItemType Directory -Force -Path $Root | Out-Null
$dest = Join-Path (Join-Path $Root 'clients') $ClientName

# ---- 1. refuse while the game holds files open ------------------------------
# A running javaw keeps huge jars mapped, and robocopy reports those as errors
# that are easy to mistake for a disk problem.
$busy = Get-CimInstance Win32_Process -Filter "Name = 'javaw.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $_.CommandLine -and $_.CommandLine.IndexOf($Source, 'OrdinalIgnoreCase') -ge 0 }
if ($busy) {
    throw ("javaw is running against the old client (pid " + ($busy.ProcessId -join ',') +
           "); stop the game first (POST /api/v1/control/game/stop)")
}

# ---- 2. space check --------------------------------------------------------
# Copy, not move -- so the destination volume needs room for a second copy.
$srcStats = Tree-Stats $Source
$destRoot = Split-Path $Root -Parent
$free = (Get-PSDrive -Name (Get-Item $destRoot).PSDrive.Name).Free
Say ("source : {0}  files={1} bytes={2:N0}" -f $Source, $srcStats.Files, $srcStats.Bytes)
Say ("dest   : {0}" -f $dest)
Say ("free   : {0:N0} bytes on {1}" -f $free, (Get-Item $destRoot).PSDrive.Name)
if ($free -lt [long]($srcStats.Bytes * 1.15)) {
    throw ("not enough free space: need ~{0:N0} (copy + 15%), have {1:N0}" -f ($srcStats.Bytes * 1.15), $free)
}

# ---- 3. copy ---------------------------------------------------------------
# /E        subdirs incl. empty ones
# /COPY:DAT data+attributes+timestamps (no ACLs: the destination inherits)
# /R:2 /W:2 two retries -- a locked file should fail loudly, not hang
# /XJ       do NOT follow junctions. The source may itself contain junctions
#           (old saves/mods links); following them can duplicate or loop.
New-Item -ItemType Directory -Force -Path $dest | Out-Null
& robocopy $Source $dest /E /COPY:DAT /R:2 /W:2 /XJ /NFL /NDL /NJH /NJS /NP | Out-Null
$rc = $LASTEXITCODE
Say ("robocopy exit = {0} (0-7 = ok, 8+ = failure)" -f $rc)
if ($rc -ge 8) { throw "robocopy failed with exit code $rc" }

# ---- 4. verify -------------------------------------------------------------
$dstStats = Tree-Stats $dest
Say ("verify : files {0} -> {1}; bytes {2:N0} -> {3:N0}" -f $srcStats.Files, $dstStats.Files, $srcStats.Bytes, $dstStats.Bytes)
if ($dstStats.Files -ne $srcStats.Files -or $dstStats.Bytes -ne $srcStats.Bytes) {
    $a = Top-Level-Stats $Source
    $b = Top-Level-Stats $dest
    $keys = ($a.Keys + $b.Keys) | Sort-Object -Unique
    foreach ($k in $keys) {
        if ($a[$k] -ne $b[$k]) { Say ("  mismatch {0}: src={1} dst={2}" -f $k, $a[$k], $b[$k]) }
    }
    throw "copy does not match the source; nothing was switched over"
}

# ---- 5. rewrite the instance ----------------------------------------------
# Targeted replace instead of ConvertFrom-Json/ConvertTo-Json: the round trip
# reorders keys, turns \\ into \\\\ in some PowerShell versions and normalises
# numbers -- and this file is read by the service, not by PowerShell.
$instPath = Join-Path (Join-Path $Root 'instances') ($InstanceId + '.json')
if (-not (Test-Path -LiteralPath $instPath)) { throw "instance not found: $instPath" }
$raw = [IO.File]::ReadAllText($instPath)
$rel = 'clients\' + $ClientName
$m = [regex]::Match($raw, '"client_dir"\s*:\s*"([^"]*)"')
if (-not $m.Success) { throw "no client_dir key in $instPath" }
$cur = $m.Groups[1].Value
Say ("client_dir: '{0}' -> '{1}'" -f $cur, $rel)
# $cur is the RAW json text, so backslashes are doubled. Compare unescaped, or
# a second run rewrites the file (and makes a backup) for no reason.
if ($cur.Replace('\\', '\') -ieq $rel) {
    Say 'client_dir already migrated (file left untouched)'
} else {
    # Splice only the captured span: everything else -- unknown fields, key
    # order, comments if any, line endings -- stays byte-for-byte.
    $g = $m.Groups[1]
    $new = $raw.Substring(0, $g.Index) + $rel.Replace('\', '\\') + $raw.Substring($g.Index + $g.Length)
    $bak = $instPath + '.bak-' + (Get-Date -Format 'yyyyMMdd-HHmmss')
    Copy-Item -LiteralPath $instPath -Destination $bak -Force
    [IO.File]::WriteAllText($instPath, $new, (New-Object System.Text.UTF8Encoding $false))
    Say ("instance rewritten: {0}  (backup {1})" -f $instPath, (Split-Path $bak -Leaf))
}

# The move is only useful if the version json came along -- the classpath is
# derived from versions\<version>\, so a missing one turns into "classpath 是空的".
$inst = Get-Content -LiteralPath $instPath -Raw | ConvertFrom-Json
$ver = [string]$inst.game.version
$vjson = Join-Path (Join-Path (Join-Path $dest 'versions') $ver) ($ver + '.json')
Say ("version json at new client: {0} ({1})" -f (Test-Path -LiteralPath $vjson), $vjson)
if (-not (Test-Path -LiteralPath $vjson)) {
    Say 'WARNING: the version json is not there -- the instance will not start until it is'
}

# ---- 6. repair the junctions under saves ----------------------------------
# Junctions are reparse points; deleting one with Remove-Item -Recurse would
# walk INTO the target and delete the real files. Directory.Delete on the link
# removes the link itself, which is what we want.
if (-not $NoJunctions) {
    $savesDir = [string]$inst.saves_dir
    if (-not [IO.Path]::IsPathRooted($savesDir)) { $savesDir = Join-Path $Root $savesDir }
    foreach ($name in @('mods', 'config', 'resourcepacks')) {
        $src = Join-Path $dest $name
        if (-not (Test-Path -LiteralPath $src)) { continue }
        $link = Join-Path $savesDir $name
        if (Test-Path -LiteralPath $link) {
            $item = Get-Item -LiteralPath $link -Force
            if ($item.LinkType -ne 'Junction') {
                Say ("skip {0}: exists and is not a junction (left alone)" -f $name)
                continue
            }
            $old = @($item.Target)[0]
            if ($old -and $old.TrimEnd('\') -ieq $src.TrimEnd('\')) {
                Say ("junction ok: {0} -> {1}" -f $name, $old)
                continue
            }
            [IO.Directory]::Delete($link)
            Say ("junction removed: {0} (was -> {1})" -f $name, $old)
        }
        New-Item -ItemType Junction -Path $link -Target $src | Out-Null
        Say ("junction made: {0} -> {1}" -f $name, $src)
    }
}

# ---- 7. prove it can still be planned -------------------------------------
$exe = Join-Path (Split-Path $Root -Parent) 'stream-server.exe'
if (Test-Path -LiteralPath $exe) {
    Say '--- --print-game ---'
    & $exe --print-game 2>&1 | Select-Object -First 12 | ForEach-Object { Say $_ }
} else {
    Say ("(no stream-server.exe at {0}; run --print-game by hand)" -f $exe)
}

# ---- 8. the original stays ------------------------------------------------
if ($PurgeSource) {
    if (-not $Yes) {
        Say 'refusing to delete the source: add -Yes if you really mean it'
        Say ("  literal command: Remove-Item -LiteralPath '{0}' -Recurse -Force" -f $Source)
        return
    }
    Say ("PURGING SOURCE {0} -- the copy at {1} is now the only one" -f $Source, $dest)
    Remove-Item -LiteralPath $Source -Recurse -Force
    Say 'source purged'
} else {
    Say ("source kept: {0}" -f $Source)
    Say ("  after a successful launch, delete it with: Remove-Item -LiteralPath '{0}' -Recurse -Force" -f $Source)
}
