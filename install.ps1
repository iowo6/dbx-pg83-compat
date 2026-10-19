# Install script for the DBX Legacy PostgreSQL Driver plugin (Windows desktop).
# Copies the assembled dist/ package into the DBX plugin store.
#
# Plugin directory resolution (mirrors dbx data_dir.rs):
#   1. Custom plugin store dir configured in DBX settings (this script does not
#      read the settings DB; pass it via -PluginDir if you customized it).
#   2. Portable install (portable.dbx beside dbx.exe): <exe dir>\data\plugins
#   3. Installed build (uninstall.exe beside dbx.exe) / default:
#      %APPDATA%\com.dbx.app\plugins
param(
    [string]$DbxExeDir = "D:\DBX",
    [string]$PluginDir
)

$ErrorActionPreference = "Stop"

$source = Join-Path $PSScriptRoot "dist\dbx-pg83-compat"
if (-not (Test-Path (Join-Path $source "manifest.json"))) {
    Write-Error "dist package not found. Run 'cargo build --release' and reassemble dist/ first."
}

if (-not $PluginDir) {
    $portableMarker = Join-Path $DbxExeDir "portable.dbx"
    $installerMarker = Join-Path $DbxExeDir "uninstall.exe"
    if ((Test-Path $portableMarker) -and -not (Test-Path $installerMarker)) {
        $PluginDir = Join-Path $DbxExeDir "data\plugins"
    } else {
        $PluginDir = Join-Path $env:APPDATA "com.dbx.app\plugins"
    }
}

$target = Join-Path $PluginDir "dbx-pg83-compat"
if (Test-Path $target) {
    Write-Host "Removing existing plugin at $target"
    Remove-Item -Recurse -Force $target
}

New-Item -ItemType Directory -Force -Path $PluginDir | Out-Null
Copy-Item -Recurse $source $target
Write-Host "Installed to $target"
Write-Host "Restart DBX, then create a JDBC connection with a jdbc:postgresql:// URL."
