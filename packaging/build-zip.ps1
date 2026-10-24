# Build dist/cpuview-<version>-<arch>-windows.zip: cpuview.exe (C runtime linked in, see
# .cargo/config.toml), the wallpaper pack, LICENSE and README, for Windows 10 and 11.
#   packaging/build-zip.ps1            x86_64
#   packaging/build-zip.ps1 aarch64
param([string]$Arch = "x86_64")
$ErrorActionPreference = "Stop"
if ($Arch -ne "x86_64" -and $Arch -ne "aarch64") { throw "unknown arch: $Arch (x86_64 or aarch64)" }
$Dir = (Resolve-Path "$PSScriptRoot/..").Path
$Target = "$Arch-pc-windows-msvc"
cargo build --release --locked --target $Target --manifest-path "$Dir/Cargo.toml"
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$Version = (Select-String -Path "$Dir/Cargo.toml" -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$Name = "cpuview-$Version-$Arch-windows"
$Stage = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
$R = Join-Path $Stage $Name
New-Item -ItemType Directory -Force "$R/wallpapers" | Out-Null
Copy-Item "$Dir/target/$Target/release/cpuview.exe" $R
Copy-Item "$Dir/wallpapers/*.jpg", "$Dir/wallpapers/*.png", "$Dir/wallpapers/CREDITS.md", "$Dir/wallpapers/credits.toml" "$R/wallpapers"
Copy-Item "$Dir/LICENSE", "$Dir/README.md" $R

New-Item -ItemType Directory -Force "$Dir/dist" | Out-Null
$Out = "$Dir/dist/$Name.zip"
if (Test-Path $Out) { Remove-Item $Out }
Compress-Archive -Path $R -DestinationPath $Out
Remove-Item -Recurse -Force $Stage
Write-Output $Out
