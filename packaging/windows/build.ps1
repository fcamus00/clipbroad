# Builds the Windows installer (clipbroad-<tag>-windows-x86_64-setup.exe) into the current folder.
# Installs Inno Setup with Chocolatey if it is missing. The installer version comes from the binary.
# Usage: packaging/windows/build.ps1 <path to clipbroad.exe> [<tag>]
param(
    [Parameter(Mandatory)][string]$Exe,
    [string]$Tag
)
$ErrorActionPreference = 'Stop'

$iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
if (-not (Test-Path $iscc)) {
    choco install innosetup --no-progress -y
    if ($LASTEXITCODE -ne 0) { throw "choco install innosetup failed" }
}

$Exe = (Resolve-Path $Exe).Path
$version = (& $Exe --version) -replace '^clipbroad ', ''
if (-not $Tag) { $Tag = "v$version" }

& $iscc /Q "/DVersion=$version" "/DTag=$Tag" "/DExe=$Exe" "/O$PWD" "$PSScriptRoot\clipbroad.iss"
if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }
