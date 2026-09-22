# Package the desktop runtime on Windows without bash: the PowerShell twin of package-runtime.sh,
# producing the same folder and manifest.
#
#   dist/runtime/collabo-core-<platform>/
#     bin/collabo-core-engine.exe      the native engine (engine/, Rust + wasmtime)
#     app/images/vmlinux.wasm          the kernel
#     app/images/*.cpio                guest root, the CPython and network tools overlays
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
# - The engine is built with a Rust toolchain whose host is the target (for win-x64 on an ARM64
#   machine: `rustup toolchain install stable-x86_64-pc-windows-msvc`, run under emulation).
#   Cross-building from an aarch64 host links the build scripts for ARM64, which needs an ARM64
#   CRT that an x64-only Build Tools install lacks (LNK1120).
# - engine/.cargo/config.toml sets CC=clang-19 for the Linux build; CC=cl.exe here overrides it.
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
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw "no cargo; install Rust (rustup)" }

$Toolchain = @{ "win-x64" = "stable-x86_64-pc-windows-msvc"; "win-arm64" = "stable-aarch64-pc-windows-msvc" }[$Platform]
if (-not $env:CC) { $env:CC = "cl.exe" }

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
Copy-Item -Recurse (Join-Path $Engine "licenses") (Join-Path $Dir "app\licenses")

# manifest.json: every file's sha256 by its /-separated path, sorted (ordinal, as Python's sorted).
$Files = [System.Collections.Generic.SortedDictionary[string, string]]::new([System.StringComparer]::Ordinal)
Get-ChildItem -Recurse -File $Dir | ForEach-Object {
    $Rel = $_.FullName.Substring($Dir.Length + 1).Replace("\", "/")
    if ($Rel -ne "manifest.json") { $Files[$Rel] = (Get-FileHash -Algorithm SHA256 $_.FullName).Hash.ToLower() }
}
$FileMap = [ordered]@{}
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
                 if (Test-Path (Join-Path $Dir "app\images\tools.cpio")) { @("--tools-image", "app/images/tools.cpio") } else { @() })
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
