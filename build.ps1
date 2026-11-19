# Compila WizEverything con el toolchain GNU de Rust + MinGW-w64 (WinLibs).
# Uso: .\build.ps1        -> target\release\wizeverything.exe
$mingw = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages" -Recurse -Filter dlltool.exe -Depth 6 -ErrorAction SilentlyContinue |
    Select-Object -First 1
if (-not $mingw) { throw "No se encontró MinGW-w64. Instalalo con: winget install BrechtSanders.WinLibs.POSIX.UCRT" }
$env:PATH = "$($mingw.DirectoryName);$env:PATH"
Set-Location $PSScriptRoot
cargo build --release
if ($LASTEXITCODE -eq 0) { Write-Host "OK -> $PSScriptRoot\target\release\wizeverything.exe" }
