# Builds WizEverything (release).
#   - Uses the MSVC toolchain when Visual Studio Build Tools (C++ workload) are installed:
#     smaller binary, no MinGW needed.
#   - Otherwise falls back to the GNU toolchain + MinGW-w64 (WinLibs).
# Usage: .\build.ps1 [-Gnu] [-Msvc]
param([switch]$Gnu, [switch]$Msvc)
Set-Location $PSScriptRoot

function Test-MsvcAvailable {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { return $false }
    $vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    return [bool]$vs
}

$useMsvc = $Msvc -or (-not $Gnu -and (Test-MsvcAvailable))

if ($useMsvc) {
    rustup toolchain list | Select-String "stable-x86_64-pc-windows-msvc" -Quiet | Out-Null
    if (-not $?) { rustup toolchain install stable-x86_64-pc-windows-msvc --profile minimal }
    Write-Host "Toolchain: MSVC"
    cargo +stable-x86_64-pc-windows-msvc build --release
} else {
    $mingw = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages" -Recurse -Filter dlltool.exe -Depth 6 -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $mingw) { throw "MinGW-w64 not found. Install it with: winget install BrechtSanders.WinLibs.POSIX.UCRT (or install VS Build Tools for MSVC)" }
    $env:PATH = "$($mingw.DirectoryName);$env:PATH"
    Write-Host "Toolchain: GNU (MinGW-w64)"
    cargo build --release
}
if ($LASTEXITCODE -eq 0) { Write-Host "OK -> $PSScriptRoot\target\release\wizeverything.exe" }
