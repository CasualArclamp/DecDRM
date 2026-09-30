# Builds single-file DecDRM executables for 64-bit Windows 10/11 that need nothing
# installed: the C runtime is linked statically (no Visual C++ redistributable) and the
# experimental EnCodec codec comes with its weights built in. The results go to exe\
# (git-ignored): decdrm-gui.exe (receiver + transmitter) and decdrm.exe (command line).
# With the 3GPP EVS source zip in reference\evs (or $env:DECDRM_EVS_SRC) the EVS decoder
# (KCBS on 6140 kHz) is built in too — private use only, see crates/decdrm-evs.
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-portable.ps1
#
# Needs the EnCodec weights at build time (`decdrm models download encodec`, or
# $env:DECDRM_MODELS). Builds in target\portable, so the usual target directory and its
# cache stay as they are.

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$triple = "x86_64-pc-windows-msvc"

# With an explicit --target the flags reach only the executables, not build scripts
# and proc macros.
$env:RUSTFLAGS = "-C target-feature=+crt-static"
$env:CARGO_TARGET_DIR = Join-Path $root "target\portable"
if (-not $env:CARGO_BUILD_JOBS) { $env:CARGO_BUILD_JOBS = "6" }

$features = "decdrm-gui/embed-weights,decdrm-cli/embed-weights"
$evsSrc = if ($env:DECDRM_EVS_SRC) { $env:DECDRM_EVS_SRC } else { Join-Path $root "reference\evs" }
if ((Test-Path $evsSrc) -and ((Get-Item $evsSrc).PSIsContainer -eq $false -or
        (Get-ChildItem $evsSrc -Filter *.zip -ErrorAction SilentlyContinue) -or
        (Test-Path (Join-Path $evsSrc "c-code")) -or (Test-Path (Join-Path $evsSrc "lib_dec")))) {
    $features += ",decdrm-gui/evs,decdrm-cli/evs"
    Write-Host "EVS decoder: included (source in $evsSrc)"
} else {
    Write-Host "EVS decoder: not included (no source in $evsSrc)"
}

Push-Location $root
try {
    cargo build --release --target $triple -p decdrm-gui -p decdrm-cli --features $features
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    $out = Join-Path $root "exe"
    New-Item -ItemType Directory -Force $out | Out-Null
    foreach ($exe in "decdrm-gui.exe", "decdrm.exe") {
        Copy-Item (Join-Path $env:CARGO_TARGET_DIR "$triple\release\$exe") $out -Force
        $mb = (Get-Item (Join-Path $out $exe)).Length / 1MB
        Write-Host ("{0,-16} {1,6:N1} MB" -f $exe, $mb)
    }
    Write-Host "written to $out"
} finally {
    Pop-Location
}
