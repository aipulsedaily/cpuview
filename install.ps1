# Install (or update) cpuview for this Windows user from the latest GitHub release:
#   irm https://raw.githubusercontent.com/aipulsedaily/cpuview/main/install.ps1 | iex
# Puts cpuview.exe and the wallpapers in %LOCALAPPDATA%\Programs\cpuview and that folder on
# the user's PATH. No administrator rights; the release zip's SHA-256 is checked against
# the one GitHub publishes for it. Remove it again:
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/aipulsedaily/cpuview/main/install.ps1))) -Uninstall
# (Windows PowerShell 5.1 and PowerShell 7 both)
param([switch]$Uninstall)
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"  # (5.1's progress bar slows downloads to a crawl)
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Dir = Join-Path ([Environment]::GetFolderPath("LocalApplicationData")) "Programs\cpuview"

# the user's PATH from the registry (not $env:Path, which has the machine's in it too)
function Set-UserPath([scriptblock]$Edit) {
    $p = [Environment]::GetEnvironmentVariable("Path", "User")
    $parts = @(if ($p) { $p.Split(";") | Where-Object { $_ } })
    $new = (& $Edit $parts) -join ";"
    if ($new -ne $p) { [Environment]::SetEnvironmentVariable("Path", $new, "User") }
}

if ($Uninstall) {
    if (Test-Path "$Dir\cpuview.exe") {
        # (the sensor service, if --setup-sensors installed it: its own undo, elevated)
        & "$Dir\cpuview.exe" --setup-sensors --undo
    }
    Remove-Item -Recurse -Force $Dir -ErrorAction SilentlyContinue
    Set-UserPath { param($parts) $parts | Where-Object { $_.TrimEnd("\") -ne $Dir } }
    Write-Host "Removed cpuview ($Dir). Your settings stay in $env:APPDATA\cpuview."
    return
}

$os = try { [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() } catch { $null }
if (-not $os) { $os = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE } }
$Arch = switch -regex ($os) {
    "^(X64|AMD64)$" { "x86_64" }
    "^(Arm64|ARM64)$" { "aarch64" }
    default { throw "cpuview has no Windows build for this processor ($os)" }
}

$rel = Invoke-RestMethod -Headers @{ "User-Agent" = "cpuview-install" } "https://api.github.com/repos/aipulsedaily/cpuview/releases/latest"
$asset = $rel.assets | Where-Object { $_.name -like "cpuview-*-$Arch-windows.zip" } | Select-Object -First 1
if (-not $asset) { throw "the latest release ($($rel.tag_name)) has no Windows $Arch zip yet" }

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("cpuview-" + [Guid]::NewGuid())
New-Item -ItemType Directory $tmp | Out-Null
try {
    $zip = Join-Path $tmp $asset.name
    Write-Host "Downloading $($asset.name)"
    Invoke-WebRequest -UseBasicParsing -Headers @{ "User-Agent" = "cpuview-install" } $asset.browser_download_url -OutFile $zip
    $want = "$($asset.digest)" -replace "^sha256:", ""
    if (-not $want) { throw "GitHub gives no SHA-256 for $($asset.name): not installing it unchecked" }
    $got = (Get-FileHash -Algorithm SHA256 $zip).Hash
    if ($got -ne $want) { throw "$($asset.name): SHA-256 $got, GitHub says ${want}: not installing it" }
    Expand-Archive -Path $zip -DestinationPath $tmp
    $src = Join-Path $tmp ($asset.name -replace "\.zip$", "")

    New-Item -ItemType Directory -Force $Dir | Out-Null
    # a running cpuview.exe can't be overwritten, but it can be renamed out of the way
    Remove-Item -Force "$Dir\cpuview.old.exe" -ErrorAction SilentlyContinue
    if (Test-Path "$Dir\cpuview.exe") { Rename-Item "$Dir\cpuview.exe" "cpuview.old.exe" }
    Copy-Item "$src\cpuview.exe", "$src\LICENSE", "$src\README.md" $Dir -Force
    Remove-Item -Recurse -Force "$Dir\wallpapers" -ErrorAction SilentlyContinue
    Copy-Item -Recurse "$src\wallpapers" $Dir
    Remove-Item -Force "$Dir\cpuview.old.exe" -ErrorAction SilentlyContinue
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

Set-UserPath { param($parts) @($parts | Where-Object { $_.TrimEnd("\") -ne $Dir }) + $Dir }
if (-not ($env:Path.Split(";") -contains $Dir)) { $env:Path += ";$Dir" }

Write-Host "Installed cpuview $($rel.tag_name) in $Dir (on your PATH; new terminals find it too)."
Write-Host "Run: cpuview          CPU temperatures, fans and voltages: cpuview --setup-sensors"
