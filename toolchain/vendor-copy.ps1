# 0e-1: copy scrap / base / hbb_common into cloud-stream\vendor, excluding VCS/build artifacts.
# Idempotent: overwrites changed sources; does not delete extra vendor files added later.
$ErrorActionPreference = 'Stop'
$root = 'C:\Users\user\Desktop\rustdesk\libs'
$dest = 'C:\Users\user\Desktop\cloud-stream\vendor'
$crates = @('hbb_common', 'base', 'scrap')

New-Item -ItemType Directory -Force -Path $dest | Out-Null

foreach ($c in $crates) {
    $src = Join-Path $root $c
    $out = Join-Path $dest $c
    # robocopy: /E all subdirs, /XD exclude dirs, /XF exclude files, /NFL /NDL quiet per-file
    $args = @(
        $src, $out,
        '/E',
        '/XD', '.git', 'target', 'generated', 'node_modules',
        '/XF', 'Cargo.lock',
        '/NFL', '/NDL', '/NP', '/R:1', '/W:1'
    )
    & robocopy @args
    $code = $LASTEXITCODE
    if ($code -ge 8) { throw "robocopy $c failed with exit code $code" }
    "$c copied (robocopy exit=$code)"
}

# sanity: list copied crate roots
Get-ChildItem $dest -Directory | ForEach-Object {
    $files = (Get-ChildItem $_.FullName -Recurse -File | Measure-Object).Count
    "$($_.Name): $files files"
}
