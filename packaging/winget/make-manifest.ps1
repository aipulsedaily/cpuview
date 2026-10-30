# Write the winget manifests for a release (not submitted anywhere: microsoft/winget-pkgs
# takes them as a pull request, see README.md here). The version comes from Cargo.toml,
# the SHA256 from the zip that packaging/build-zip.ps1 made (the one attached to the
# release: same file, same hash).
#   packaging/winget/make-manifest.ps1   -> dist/winget/manifests/a/aipulsedaily/cpuview/<version>/
$ErrorActionPreference = "Stop"
$Dir = (Resolve-Path "$PSScriptRoot/../..").Path
$Version = (Select-String -Path "$Dir/Cargo.toml" -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$Zip = "$Dir/dist/cpuview-$Version-x86_64-windows.zip"
if (-not (Test-Path $Zip)) { throw "$Zip missing: run packaging/build-zip.ps1 first" }
$Sha = (Get-FileHash -Algorithm SHA256 $Zip).Hash
$Out = "$Dir/dist/winget/manifests/a/aipulsedaily/cpuview/$Version"
New-Item -ItemType Directory -Force $Out | Out-Null
foreach ($f in Get-ChildItem "$PSScriptRoot/*.yaml") {
    $t = (Get-Content -Raw $f.FullName).Replace("{version}", $Version).Replace("{sha256}", $Sha)
    # (LF, UTF-8 without BOM, as winget-pkgs expects)
    [IO.File]::WriteAllText("$Out/$($f.Name)", $t.Replace("`r`n", "`n"))
}
Write-Output $Out
