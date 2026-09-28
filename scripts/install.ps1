#Requires -Version 7
<#
.SYNOPSIS
    Builds TapClip and installs it for the current user.

.DESCRIPTION
    Copies the release build to %LOCALAPPDATA%\Programs\TapClip and starts it.
    A running copy is asked to quit first, so this also upgrades in place.
    Your config (%APPDATA%\TapClip) is never touched.

.PARAMETER Autostart
    Also start TapClip when you sign in to Windows (per-user Run key).
    It can be toggled later from the tray icon menu.

.PARAMETER Uninstall
    Quit TapClip, remove it from autostart and delete the installed program.
#>
param(
    [switch]$Autostart,
    [switch]$Uninstall
)
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\TapClip'
$exe = Join-Path $installDir 'tapclip.exe'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'

function Stop-TapClip {
    if (-not (Test-Path $exe)) { return }
    & $exe --quit
    Get-Process tapclip -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $exe } |
        Wait-Process -Timeout 5 -ErrorAction SilentlyContinue
}

if ($Uninstall) {
    Stop-TapClip
    Remove-ItemProperty -Path $runKey -Name TapClip -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $installDir -ErrorAction SilentlyContinue
    Write-Host "TapClip removed. Config and log are kept in $env:APPDATA\TapClip and $env:LOCALAPPDATA\TapClip."
    return
}

cargo build --release --manifest-path (Join-Path $root 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

Stop-TapClip
New-Item -ItemType Directory -Force -Path $installDir | Out-Null
Copy-Item -Force (Join-Path $root 'target\release\tapclip.exe') $exe

if ($Autostart) {
    Set-ItemProperty -Path $runKey -Name TapClip -Value "`"$exe`""
}

Start-Process -FilePath $exe
Write-Host "TapClip installed to $installDir and started."
if ($Autostart) { Write-Host 'It will also start when you sign in.' }
