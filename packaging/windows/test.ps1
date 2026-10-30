# Smoke test for the Windows installer: installs, reinstalls over the running daemon (an upgrade)
# and uninstalls, checking files, PATH, the login registry value and the daemon after each step.
# It changes the current user's setup, so CI runs it on a throwaway windows-latest runner.
# Usage: packaging/windows/test.ps1 <path to setup.exe>
param([Parameter(Mandatory)][string]$Setup)
$ErrorActionPreference = 'Stop'

$app = "$env:LOCALAPPDATA\Programs\clipbroad"
$exe = "$app\clipbroad.exe"
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$silent = '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART'

function Check([bool]$ok, [string]$what) {
    if (-not $ok) { throw "FAILED: $what" }
    Write-Host "ok: $what"
}
function Invoke-Setup([string]$path) {
    # WaitForExit instead of Start-Process -Wait, which would also wait for the daemon it starts.
    $process = Start-Process $path $silent -PassThru
    $process.WaitForExit()
    Check ($process.ExitCode -eq 0) "$(Split-Path $path -Leaf) exits with 0"
}
function Test-Daemon { [bool](Get-Process clipbroad -ErrorAction SilentlyContinue) }
function Get-AppPathEntry { @([Environment]::GetEnvironmentVariable('Path', 'User') -split ';' | Where-Object { $_ -eq $app }) }
function Get-RunValue { (Get-ItemProperty $runKey).clipbroad }

Invoke-Setup $Setup
Check (Test-Path $exe) "clipbroad.exe installed"
Write-Host (& $exe --version)
Check ((Get-AppPathEntry).Count -eq 1) "install folder in the user PATH"
Check ((Get-RunValue) -like "*`"$exe`" run*") "login registry value runs the installed exe"
Start-Sleep 2
Check (Test-Daemon) "daemon running"

Invoke-Setup $Setup
Check ((Get-AppPathEntry).Count -eq 1) "PATH entry not duplicated after an upgrade"
Start-Sleep 2
Check (Test-Daemon) "daemon running after an upgrade"

Invoke-Setup "$app\unins000.exe"
# The uninstaller finishes in a copy of itself, so wait for the files to go.
for ($i = 0; $i -lt 30 -and (Test-Path $exe); $i++) { Start-Sleep 1 }
Check (-not (Test-Path $exe)) "clipbroad.exe removed"
Check (-not (Test-Daemon)) "daemon stopped"
Check ($null -eq (Get-RunValue)) "login registry value removed"
Check ((Get-AppPathEntry).Count -eq 0) "install folder removed from the user PATH"
