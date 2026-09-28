# Package the desktop runtime on Windows without bash: the PowerShell twin of package-runtime.sh,
# producing the same folder and manifest.
#
#   dist/runtime/collabo-core-<platform>/
#     bin/collabo-core-engine.exe      the native engine (engine/, Rust + wasmtime)
#     app/images/vmlinux.wasm          the kernel
#     app/images/*.cpio                guest root, the CPython and network tools overlays
#     app/images/addons/*              add-ons, off unless the app asks (addons/README.md)
#     app/licenses/*
#     manifest.json                    { platform, engine, protocol, entry, files }
#
#   powershell -ExecutionPolicy Bypass -File scripts\package-runtime-windows.ps1
#   ... -Platform win-arm64            (default: win-x64)
#   ... -Archive                       also write collabo-core-<platform>.zip
#
# dist/engine (kernel and images) comes from a Linux build: copy it over first.
#
# Toolchain notes (see docs/note.md, 2026-09-22):
# - The engine is built with the Rust version engine/rust-toolchain.toml pins, for a host that is the
#   target (for win-x64 on an ARM64 machine: <version>-x86_64-pc-windows-msvc, run under
#   emulation). This script installs it with rustup when it is missing; an older "stable" fails
#   on Cargo.lock v4 and on the dependencies' minimum Rust.
#   Cross-building from an aarch64 host links the build scripts for ARM64, which needs an ARM64
#   CRT that an x64-only Build Tools install lacks (LNK1120).
# - The C helper wasmtime compiles needs MSVC (Visual Studio Build Tools, "Desktop development with
#   C++" with the x64 tools). cc-rs finds it by itself: no Developer prompt and no CC needed.
param(
    [ValidateSet("win-x64", "win-arm64")] [string] $Platform = "win-x64",
    [switch] $Archive
)
$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
$Engine = Join-Path $Root "dist\engine"
$Out = Join-Path $Root "dist\runtime"
if (-not (Test-Path (Join-Path $Engine "kernel\vmlinux.wasm"))) {
    throw "missing dist\engine; build it on Linux (python3 build.py engine) and copy it here"
}
if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) { throw "no rustup; install Rust from https://rustup.rs" }

$Pinned = Select-String -Path (Join-Path $Root "engine\rust-toolchain.toml") -Pattern '^channel\s*=\s*"([^"]+)"'
if (-not $Pinned) { throw "no channel in engine\rust-toolchain.toml" }
$Version = $Pinned.Matches[0].Groups[1].Value
$Toolchain = "$Version-" + @{ "win-x64" = "x86_64-pc-windows-msvc"; "win-arm64" = "aarch64-pc-windows-msvc" }[$Platform]
& rustup toolchain install $Toolchain --profile minimal --no-self-update
if ($LASTEXITCODE) { throw "rustup could not install $Toolchain ($LASTEXITCODE)" }

Write-Host "== build $Platform ($Toolchain)"
Push-Location (Join-Path $Root "engine")
try {
    & cargo "+$Toolchain" build --release
    if ($LASTEXITCODE) { throw "cargo build failed ($LASTEXITCODE)" }
} finally { Pop-Location }
$Built = Join-Path $Root "engine\target\release\collabo-core-engine.exe"
if (-not (Test-Path $Built)) { throw "the engine was not built for $Platform" }

$Dir = Join-Path $Out "collabo-core-$Platform"
if (Test-Path $Dir) { Remove-Item -Recurse -Force $Dir }
New-Item -ItemType Directory -Force (Join-Path $Dir "bin"), (Join-Path $Dir "app\images") | Out-Null
Copy-Item $Built (Join-Path $Dir "bin\collabo-core-engine.exe")
Copy-Item (Join-Path $Engine "kernel\vmlinux.wasm") (Join-Path $Dir "app\images")
Copy-Item (Join-Path $Engine "images\*.cpio") (Join-Path $Dir "app\images")
if (Test-Path (Join-Path $Engine "images\addons")) {
    Copy-Item -Recurse (Join-Path $Engine "images\addons") (Join-Path $Dir "app\images\addons")
}
Copy-Item -Recurse (Join-Path $Engine "licenses") (Join-Path $Dir "app\licenses")

# manifest.json: every file's sha256 by its /-separated path, sorted (ordinal, as Python's sorted).
$Files = [System.Collections.Generic.SortedDictionary[string, string]]::new([System.StringComparer]::Ordinal)
Get-ChildItem -Recurse -File $Dir | ForEach-Object {
    $Rel = $_.FullName.Substring($Dir.Length + 1).Replace("\", "/")
    if ($Rel -ne "manifest.json") { $Files[$Rel] = (Get-FileHash -Algorithm SHA256 $_.FullName).Hash.ToLower() }
}
$FileMap = [ordered]@{}
# The add-ons shipped (app/images/addons/<name>.cpio), off unless the app asks (addons/README.md).
[string[]]$Addons = @()
$AddonDir = Join-Path $Dir "app\images\addons"
if (Test-Path $AddonDir) {
    $Addons = @(Get-ChildItem -File (Join-Path $AddonDir "*.cpio") | ForEach-Object { $_.BaseName })
    [Array]::Sort($Addons, [System.StringComparer]::Ordinal)
}
foreach ($Pair in $Files.GetEnumerator()) { $FileMap[$Pair.Key] = $Pair.Value }
$Manifest = [ordered]@{
    name     = "collabo-core-runtime"
    platform = $Platform
    engine   = "wasmtime"
    protocol = 1
    entry    = @("bin/collabo-core-engine.exe",
                 "--kernel", "app/images/vmlinux.wasm",
                 "--initramfs", "app/images/initramfs.cpio",
                 "--python-image", "app/images/python.cpio") + $(
                 if (Test-Path (Join-Path $Dir "app\images\tools.cpio")) { @("--tools-image", "app/images/tools.cpio") } else { @() }) + $(
                 if ($Addons.Count) { @("--addon-dir", "app/images/addons") } else { @() })
    addons   = $Addons
    files    = $FileMap
}
# Written without a BOM: Windows PowerShell's -Encoding UTF8 would add one.
[System.IO.File]::WriteAllText((Join-Path $Dir "manifest.json"), ($Manifest | ConvertTo-Json -Depth 4))

if ($Archive) {
    $Zip = Join-Path $Out "collabo-core-$Platform.zip"
    if (Test-Path $Zip) { Remove-Item -Force $Zip }
    Compress-Archive -Path $Dir -DestinationPath $Zip
}
$Size = (Get-ChildItem -Recurse -File $Dir | Measure-Object -Sum Length).Sum / 1MB
Write-Host ("== {0}: {1:N0}M  {2}" -f $Platform, $Size, $Dir)
