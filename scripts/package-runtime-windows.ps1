# Package the desktop runtime on Windows without bash: the PowerShell twin of package-runtime.sh,
# producing the same folders and manifests, for x64 and ARM64 alike.
#
#   dist/runtime/collabo-core-<platform>/
#     bin/collabo-core-engine.exe      the native engine (engine/, Rust + wasmtime)
#     launcher.exe, launcher.conf      starts the engine as launcher.conf says, with no arguments
#     work/                            the folder launcher.conf shares with the guest as /work
#     app/images/vmlinux.wasm          the kernel
#     app/images/*.cpio                guest root, the CPython and tools overlays
#     app/images/addons/*              add-ons, off unless the app asks (addons/README.md)
#     app/licenses/*
#     manifest.json                    { platform, engine, protocol, entry, files }
#
#   powershell -ExecutionPolicy Bypass -File scripts\package-runtime-windows.ps1
#                                      both: win-x64 and win-arm64 (python build.py runtime)
#   ... -Platform win-arm64            one (several: "win-x64,win-arm64"; "host": this machine's)
#   ... -Archive                       also write collabo-core-<platform>.zip
#
# dist/engine (kernel and images) comes from a Linux build: copy it over first.
#
# Toolchains (see docs/note.md, 2026-09-22 and 2026-10-06). The engine is built with the Rust
# version engine/rust-toolchain.toml pins, always with --target, so each platform has its own
# output (engine\target\<triple>\release). Which toolchain builds which platform:
#
#   machine \ platform   win-x64                                win-arm64
#   x64                  <ver>-x86_64 (native)                  <ver>-x86_64 + the aarch64 target
#                                                               (cross-compiled; x64 Windows cannot
#                                                               run an ARM64 rustc)
#   ARM64                <ver>-x86_64, --force-non-host         <ver>-aarch64 (native)
#                        (runs under emulation)
#
# - On ARM64, the x64 toolchain builds win-x64 rather than the native one cross-compiling: that
#   would link the build scripts for ARM64, which needs the ARM64 CRT an x64-only Build Tools
#   install lacks (LNK1120). rustup installs a toolchain for another machine only with
#   --force-non-host, which this script passes whenever the toolchain is not for the machine
#   (or not rustup's own host).
# - Visual Studio Build Tools, "Desktop development with C++", with the MSVC build tools of every
#   platform built here: "MSVC ... x64/x86 build tools" (Microsoft.VisualStudio.Component.VC.Tools.x86.x64)
#   for win-x64, "MSVC ... ARM64/ARM64EC build tools" (Microsoft.VisualStudio.Component.VC.Tools.ARM64)
#   for win-arm64. cc-rs and rustc find them by themselves: no Developer prompt, no CC.
param(
    [string[]] $Platform = @("win-x64", "win-arm64"),
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

$Triples = @{ "win-x64" = "x86_64-pc-windows-msvc"; "win-arm64" = "aarch64-pc-windows-msvc" }

# The machine's own architecture. A process emulated on ARM64 (an x64 PowerShell, an x64
# Git Bash that started this) is told AMD64 by PROCESSOR_ARCHITECTURE; the registry has the
# machine's.
function Get-MachinePlatform {
    $Arch = $null
    try {
        $Arch = (Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment" -Name PROCESSOR_ARCHITECTURE).PROCESSOR_ARCHITECTURE
    } catch { }
    if (-not $Arch) { $Arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE } }
    switch ($Arch.ToUpperInvariant()) {
        "ARM64" { return "win-arm64" }
        "AMD64" { return "win-x64" }
        default { throw "unsupported machine architecture $Arch" }
    }
}
$Machine = Get-MachinePlatform

# -Platform as an array, or as one string "win-x64,win-arm64" (powershell -File passes it so).
$Platforms = @($Platform | ForEach-Object { $_ -split "[,\s]+" } | Where-Object { $_ } | ForEach-Object {
    if ($_ -eq "host") { $Machine } else { $_ }
} | Select-Object -Unique)
foreach ($P in $Platforms) {
    if (-not $Triples.ContainsKey($P)) { throw "unknown platform $P (win-x64, win-arm64, host)" }
}
if (-not $Platforms.Count) { throw "no platform given" }

# rustup's own host triple: a toolchain for any other host needs --force-non-host.
$RustupHost = (& rustup show 2>$null | Select-String -Pattern "^Default host:\s*(\S+)" | Select-Object -First 1)
$RustupHost = if ($RustupHost) { $RustupHost.Matches[0].Groups[1].Value } else { $Triples[$Machine] }

# Cargo's output folder: CARGO_TARGET_DIR if set, else engine\target. A source tree on a network
# share (a VM's shared folder such as \\Mac\...) gets a local one instead: rustc cannot remove its
# temporary archive folders there ("failed to build archive ... os error 87").
if (-not $env:CARGO_TARGET_DIR) {
    $RootUri = [System.Uri]$Root
    $Drive = if (-not $RootUri.IsUnc) { [System.IO.DriveInfo]::new([System.IO.Path]::GetPathRoot($Root)) }
    if ($RootUri.IsUnc -or ($Drive -and $Drive.DriveType -eq "Network")) {
        $env:CARGO_TARGET_DIR = Join-Path $env:LOCALAPPDATA "collabo-core\target"
        Write-Host "== $Root is on a network share; cargo builds in $env:CARGO_TARGET_DIR"
    }
}
$TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root "engine\target" }

$Pinned = Select-String -Path (Join-Path $Root "engine\rust-toolchain.toml") -Pattern '^channel\s*=\s*"([^"]+)"'
if (-not $Pinned) { throw "no channel in engine\rust-toolchain.toml" }
$Version = $Pinned.Matches[0].Groups[1].Value

function Install-Toolchain([string] $HostTriple) {
    $Toolchain = "$Version-$HostTriple"
    $RustupArgs = @("toolchain", "install", $Toolchain, "--profile", "minimal", "--no-self-update")
    if ($HostTriple -ne $Triples[$Machine] -or $HostTriple -ne $RustupHost) {
        # Not this machine's (or not rustup's) host: x64 on ARM64 runs under emulation.
        $RustupArgs += "--force-non-host"
    }
    Write-Host "== rustup $($RustupArgs -join ' ')"
    # Out-Host: rustup's output is not this function's result.
    & rustup @RustupArgs | Out-Host
    if ($LASTEXITCODE) { throw "rustup could not install $Toolchain ($LASTEXITCODE)" }
    return $Toolchain
}

function Build-Platform([string] $P) {
    $Target = $Triples[$P]
    # The toolchain whose host builds this platform (the table above): the target's own on a
    # machine of that architecture or on ARM64 (x64 under emulation); on x64, x64 cross-compiling.
    $HostTriple = if ($Machine -eq "win-x64") { $Triples["win-x64"] } else { $Target }
    $Toolchain = Install-Toolchain $HostTriple
    if ($HostTriple -ne $Target) {
        & rustup target add $Target --toolchain $Toolchain | Out-Host
        if ($LASTEXITCODE) { throw "rustup could not add $Target to $Toolchain ($LASTEXITCODE)" }
    }

    # ring (rustls, rcgen) builds its ARM64 assembly only with clang, which cc-rs looks for on
    # PATH. Build Tools' "C++ Clang Compiler for Windows" (Microsoft.VisualStudio.Component.VC.Llvm.Clang)
    # has one, but outside a Developer prompt it is not on PATH: put it there.
    if ($P -eq "win-arm64" -and -not (Get-Command clang -ErrorAction SilentlyContinue)) {
        $Vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
        $LlvmArch = if ($Machine -eq "win-arm64") { "ARM64" } else { "x64" }
        $ClangBin = $null
        if (Test-Path $Vswhere) {
            $ClangBin = & $Vswhere -all -products * -property installationPath | ForEach-Object {
                Join-Path $_ "VC\Tools\Llvm\$LlvmArch\bin"
            } | Where-Object { Test-Path (Join-Path $_ "clang.exe") } | Select-Object -First 1
        }
        if (-not $ClangBin -and (Test-Path "$env:ProgramFiles\LLVM\bin\clang.exe")) { $ClangBin = "$env:ProgramFiles\LLVM\bin" }
        if (-not $ClangBin) {
            throw "win-arm64 needs clang (for ring): add Build Tools' C++ Clang Compiler for Windows (Microsoft.VisualStudio.Component.VC.Llvm.Clang) or install LLVM"
        }
        Write-Host "== clang from $ClangBin"
        $env:PATH = "$ClangBin;$env:PATH"
    }

    Write-Host "== build $P ($Target with $Toolchain, on $Machine)"
    Push-Location (Join-Path $Root "engine")
    try {
        & cargo "+$Toolchain" build --release --target $Target
        if ($LASTEXITCODE) {
            $Component = @{ "win-x64" = "x64/x86 build tools (Microsoft.VisualStudio.Component.VC.Tools.x86.x64)";
                            "win-arm64" = "ARM64/ARM64EC build tools (Microsoft.VisualStudio.Component.VC.Tools.ARM64)" }[$P]
            throw "cargo build for $P failed ($LASTEXITCODE); a link error usually means Visual Studio Build Tools lack the MSVC $Component"
        }
    } finally { Pop-Location }
    $Release = Join-Path $TargetDir "$Target\release"
    $Built = Join-Path $Release "collabo-core-engine.exe"
    if (-not (Test-Path $Built)) { throw "the engine was not built for $P" }

    $Dir = Join-Path $Out "collabo-core-$P"
    if (Test-Path $Dir) { Remove-Item -Recurse -Force $Dir }
    New-Item -ItemType Directory -Force (Join-Path $Dir "bin"), (Join-Path $Dir "app\images"), (Join-Path $Dir "work") | Out-Null
    Copy-Item $Built (Join-Path $Dir "bin\collabo-core-engine.exe")
    Copy-Item (Join-Path $Release "launcher.exe") (Join-Path $Dir "launcher.exe")
    Copy-Item (Join-Path $Root "engine\launcher.conf") (Join-Path $Dir "launcher.conf")
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
        if ($Rel -ne "manifest.json" -and $Rel -ne "launcher.conf") { $Files[$Rel] = (Get-FileHash -Algorithm SHA256 $_.FullName).Hash.ToLower() }
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
        platform = $P
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
        $Zip = Join-Path $Out "collabo-core-$P.zip"
        if (Test-Path $Zip) { Remove-Item -Force $Zip }
        Compress-Archive -Path $Dir -DestinationPath $Zip
    }
    $Size = (Get-ChildItem -Recurse -File $Dir | Measure-Object -Sum Length).Sum / 1MB
    Write-Host ("== {0}: {1:N0}M  {2}" -f $P, $Size, $Dir)
}

New-Item -ItemType Directory -Force $Out | Out-Null
Write-Host "== machine $Machine (rustup host $RustupHost), platforms: $($Platforms -join ', ')"
foreach ($P in $Platforms) { Build-Platform $P }
