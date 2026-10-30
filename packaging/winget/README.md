# winget

Manifest templates for [winget](https://learn.microsoft.com/windows/package-manager/)
(nothing here is submitted automatically). For a release that has
`cpuview-<version>-x86_64-windows.zip` attached:

```powershell
packaging/build-zip.ps1                # or download the release's zip into dist/
packaging/winget/make-manifest.ps1     # dist/winget/manifests/a/aipulsedaily/cpuview/<version>/
winget validate dist/winget/manifests/a/aipulsedaily/cpuview/<version>
```

Then open a pull request with that folder in
[microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs) (`manifests/a/aipulsedaily/cpuview/<version>/`).
The zip is installed as a portable package: `cpuview` on PATH, the wallpapers next to
the program.
