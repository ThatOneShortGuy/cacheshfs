#!/usr/bin/env pwsh
# Run cargo inside the Visual Studio "Developer" environment.
#
# The Windows mount backend depends on winfsp-sys, whose build script runs
# bindgen (libclang) against WinFsp's headers. bindgen needs `windows.h` and the
# MSVC/Windows SDK include paths, which only exist in a VS Developer environment
# -- so in a plain terminal the first build of winfsp-sys fails with
# "'windows.h' file not found". This script sets that environment up, then hands
# off to cargo. Once winfsp-sys is built, a plain `cargo build` reuses it.
#
# Usage:
#   .\build.ps1                 # cargo build
#   .\build.ps1 build --release
#   .\build.ps1 test -p cacheshfs
#   .\build.ps1 run -- server:/srv Z:
#
# On Linux/macOS this script is unnecessary -- just use cargo directly.

$ErrorActionPreference = 'Stop'

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) {
    throw "vswhere.exe not found at '$vswhere'. Install Visual Studio (with the " +
        "'Desktop development with C++' workload) or run cargo from a " +
        "'Developer PowerShell for VS' instead."
}

$vsPath = & $vswhere -latest -products * `
    -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
    -property installationPath
if (-not $vsPath) {
    throw "No Visual Studio installation with the C++ tools was found. Install " +
        "the 'Desktop development with C++' workload."
}

$devShell = Join-Path $vsPath 'Common7\Tools\Launch-VsDevShell.ps1'
if (-not (Test-Path $devShell)) {
    throw "Launch-VsDevShell.ps1 not found under '$vsPath'."
}

# Import the MSVC/SDK environment (INCLUDE, LIB, PATH, ...) into this session.
# -SkipAutomaticLocation keeps our working directory; the script may emit a
# benign 'vswhere.exe not recognized' note that does not affect the result.
& $devShell -Arch amd64 -HostArch amd64 -SkipAutomaticLocation | Out-Null

Set-Location $PSScriptRoot

$cargoArgs = if ($args.Count -gt 0) { $args } else { @('build') }
& cargo @cargoArgs
exit $LASTEXITCODE
