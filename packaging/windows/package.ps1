<#
.SYNOPSIS
  Build, sign and package FilmCraft for Windows.

.DESCRIPTION
  Produces, in $env:DIST (default: dist/release):
    filmcraft-<version>-windows-<arch>.msi            per-machine installer (WiX v5)
    filmcraft-<version>-windows-<arch>-portable.zip   filmcraft.exe + filmcraft-cli.exe + portable.txt

  The binaries link the C runtime statically (+crt-static), so neither the MSI nor the portable
  zip needs the Visual C++ redistributable. Signing is delegated to sign.ps1 (skipped with a
  warning when no signing secrets are set).

  Needs: Rust (MSVC toolchain + the target), the Windows SDK (rc.exe, signtool.exe),
  and WiX v5: dotnet tool install --global wix --version 5.0.2

.EXAMPLE
  pwsh packaging/windows/package.ps1 -Arch x64
  pwsh packaging/windows/package.ps1 -Arch x86 -SkipBuild
  pwsh packaging/windows/package.ps1 -Arch arm64     # cross-compiled; needs the MSVC ARM64 build tools
#>
param(
  [ValidateSet('x64', 'x86', 'arm64')] [string] $Arch = 'x64',
  [switch] $SkipBuild
)
$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

function Invoke-Native([string] $What, [scriptblock] $Block) {
  Write-Output "==> $What"
  & $Block
  if ($LASTEXITCODE -ne 0) { throw "$What failed with exit code $LASTEXITCODE" }
}

# The version lives in one place: [workspace.package] version in the root Cargo.toml.
$Version = $env:FILMCRAFT_VERSION
if (-not $Version) {
  $inPkg = $false
  foreach ($line in Get-Content (Join-Path $Root 'Cargo.toml')) {
    if ($line -match '^\s*\[') { $inPkg = ($line.Trim() -eq '[workspace.package]'); continue }
    if ($inPkg -and $line -match '^\s*version\s*=\s*"([^"]+)"') { $Version = $Matches[1]; break }
  }
}
if (-not $Version) { throw 'could not read [workspace.package] version from Cargo.toml' }
# MSI ProductVersion is numeric (major.minor.build); pre-release tags are dropped there.
$MsiVersion = ($Version -split '-')[0]

$Target = switch ($Arch) { 'x64' { 'x86_64-pc-windows-msvc' } 'x86' { 'i686-pc-windows-msvc' } 'arm64' { 'aarch64-pc-windows-msvc' } }
$Dist = if ($env:DIST) { $env:DIST } else { Join-Path $Root 'dist\release' }
$TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root 'target' }
New-Item -ItemType Directory -Force -Path $Dist | Out-Null

if (-not $env:FILMCRAFT_BUILD_SHA) { $env:FILMCRAFT_BUILD_SHA = (git -C $Root rev-parse HEAD 2>$null) }
if (-not $env:FILMCRAFT_BUILD_DATE) { $env:FILMCRAFT_BUILD_DATE = (Get-Date).ToUniversalTime().ToString('yyyy-MM-dd') }

Write-Output "FilmCraft $Version for Windows $Arch ($Target)"

if (-not $SkipBuild) {
  # Static CRT: no VC++ redistributable needed. Scoped to the target so host build scripts and
  # proc-macros are unaffected.
  $flagVar = 'CARGO_TARGET_' + ($Target.ToUpper() -replace '-', '_') + '_RUSTFLAGS'
  [Environment]::SetEnvironmentVariable($flagVar, '-C target-feature=+crt-static')
  # Fail the build (rather than warn) if the icon/VERSIONINFO can't be embedded.
  $env:FILMCRAFT_REQUIRE_WINRES = '1'
  Invoke-Native "cargo build ($Target)" { cargo build --release --locked -p filmcraft -p filmcraft-cli --target $Target }
}

$Bin = Join-Path $TargetDir "$Target\release"

# Check both binaries' PE headers before packaging. Machine (COFF header) must match -Arch, so an
# x64 build can never ship labelled arm64. Subsystem (optional header): 2 = Windows GUI, 3 = console.
# The app must be GUI (no console window opens with it); the CLI must stay console so its output
# reaches the terminal.
function Get-PeHeader([string] $Path) {
  $bytes = [System.IO.File]::ReadAllBytes($Path)
  $pe = [BitConverter]::ToInt32($bytes, 0x3C)
  return @{ Machine = [BitConverter]::ToUInt16($bytes, $pe + 4); Subsystem = [BitConverter]::ToUInt16($bytes, $pe + 0x5C) }
}
$Machine = switch ($Arch) { 'x64' { 0x8664 } 'x86' { 0x14C } 'arm64' { 0xAA64 } }
foreach ($check in @(@('filmcraft.exe', 2), @('filmcraft-cli.exe', 3))) {
  $h = Get-PeHeader (Join-Path $Bin $check[0])
  if ($h.Machine -ne $Machine) { throw "$($check[0]) is for machine 0x$('{0:X}' -f $h.Machine), expected 0x$('{0:X}' -f $Machine) ($Arch)" }
  if ($h.Subsystem -ne $check[1]) { throw "$($check[0]) has PE subsystem $($h.Subsystem), expected $($check[1])" }
  Write-Output "ok $($check[0]): $Arch, PE subsystem $($h.Subsystem)"
}
$Stage = Join-Path $TargetDir "windows-package\$Arch"
Remove-Item -Recurse -Force $Stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Stage | Out-Null
# Dependency font notices are required even without optional craft-fonts.
Copy-Item -LiteralPath (Join-Path $Root 'NOTICE') -Destination $Stage
Copy-Item (Join-Path $Bin 'filmcraft.exe'), (Join-Path $Bin 'filmcraft-cli.exe') $Stage

& (Join-Path $PSScriptRoot 'sign.ps1') (Join-Path $Stage 'filmcraft.exe') (Join-Path $Stage 'filmcraft-cli.exe')

# ---- runtime libraries the executables load at run time ----------------------------------------
# FFmpeg and the face pipeline's ONNX Runtime link dynamically and are found beside the
# executable; the FFmpeg DLLs use the dynamic CRT, so the redistributable's DLLs ride along (the
# executables themselves are /MT). Both are optional CI inputs: without FFMPEG_DIR /
# ONNXRUNTIME_DIR the build is expected to have resolved them another way (statically), and the
# installers carry only the executables.
$HasRuntimeLibs = 0
$Lic = Join-Path $Stage 'licences'
if ($env:FFMPEG_DIR) {
  Copy-Item (Join-Path $env:FFMPEG_DIR 'bin\*.dll') $Stage
  if (-not (Get-ChildItem (Join-Path $Stage 'avcodec-*.dll') -ErrorAction SilentlyContinue)) {
    throw 'FFMPEG_DIR\bin carried no avcodec DLL'
  }
  New-Item -ItemType Directory -Force -Path $Lic | Out-Null
  Get-ChildItem (Join-Path $env:FFMPEG_DIR 'share') -Filter copyright -Recurse |
    ForEach-Object { Copy-Item $_.FullName (Join-Path $Lic "$($_.Directory.Name).txt") }
  $HasRuntimeLibs = 1
}
if ($env:ONNXRUNTIME_DIR) {
  Copy-Item (Join-Path $env:ONNXRUNTIME_DIR '*.dll') $Stage
  if (-not (Test-Path (Join-Path $Stage 'onnxruntime.dll'))) {
    throw 'ONNXRUNTIME_DIR carried no onnxruntime.dll'
  }
  New-Item -ItemType Directory -Force -Path $Lic | Out-Null
  Copy-Item (Join-Path $env:ONNXRUNTIME_DIR 'LICENSE') (Join-Path $Lic 'onnxruntime-LICENSE.txt')
  Copy-Item (Join-Path $env:ONNXRUNTIME_DIR 'ThirdPartyNotices.txt') (Join-Path $Lic 'onnxruntime-ThirdPartyNotices.txt')
  $HasRuntimeLibs = 1
}
if ($HasRuntimeLibs -eq 1) {
  # The whole Microsoft.VC*.CRT folder rather than a list of file names: it is exactly the set
  # Microsoft redistributes together, and picking from it by hand is how an install ends up
  # missing vcruntime140_1.dll.
  $crt = Get-ChildItem 'C:/Program Files/Microsoft Visual Studio/*/*/VC/Redist/MSVC/*/x64/Microsoft.VC*.CRT' -Directory |
    Sort-Object { [version]$_.Parent.Parent.Name } |
    Select-Object -Last 1
  if (-not $crt) { throw 'no Visual C++ redistributable found on the runner (the FFmpeg DLLs need it)' }
  Write-Output "CRT DLLs from $($crt.FullName)"
  Copy-Item (Join-Path $crt.FullName '*.dll') $Stage
}

# ---- font notices for the MSI ------------------------------------------------------------------
# The portable ZIP already carries these notices; stage the complete files before MSI creation too.
$Docs = Join-Path $Stage 'font-licences'
New-Item -ItemType Directory -Force -Path $Docs | Out-Null
$HasCraftFontLicences = 0
if ($env:CRAFT_FONTS_DIR) {
  foreach ($ofl in Get-ChildItem -Path (Join-Path $env:CRAFT_FONTS_DIR 'fonts\*\OFL.txt') -File -ErrorAction SilentlyContinue) {
    Copy-Item -LiteralPath $ofl.FullName -Destination (Join-Path $Docs "OFL-$($ofl.Directory.Name).txt")
    $HasCraftFontLicences = 1
  }
}
# ---- end font notices for the MSI --------------------------------------------------------------

# ---- MSI ---------------------------------------------------------------------------------------
$Msi = Join-Path $Dist "filmcraft-$Version-windows-$Arch.msi"
Invoke-Native 'WiX UI extension' { wix extension add -g WixToolset.UI.wixext/5.0.2 }
Invoke-Native 'wix build' {
  wix build (Join-Path $PSScriptRoot 'filmcraft.wxs') -arch $Arch -ext WixToolset.UI.wixext `
    -d "Version=$MsiVersion" -d "BinDir=$Stage" -d "IconPath=$(Join-Path $Root 'assets\app-icon\filmcraft.ico')" `
    -d "DocsDir=$Docs" -d "HasCraftFontLicences=$HasCraftFontLicences" `
    -d "LicDir=$Lic" -d "HasRuntimeLibraries=$HasRuntimeLibs" `
    -o $Msi
}
# Check the compiled scope, path and registration contract before signing.
& (Join-Path $PSScriptRoot 'check-install-policy.ps1') -MsiPath $Msi
# wix writes its debug symbols (.wixpdb) next to the MSI; keep them out of the release assets.
Remove-Item -Force -ErrorAction SilentlyContinue ([IO.Path]::ChangeExtension($Msi, '.wixpdb'))
& (Join-Path $PSScriptRoot 'sign.ps1') $Msi

# ---- portable zip ------------------------------------------------------------------------------
$Portable = Join-Path $TargetDir "windows-package\filmcraft-$Version-windows-$Arch-portable"
Remove-Item -Recurse -Force $Portable -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Portable | Out-Null
Copy-Item (Join-Path $Stage '*.exe') $Portable
# The runtime libraries beside the executable are what make the zip work on a stock machine
# (FFmpeg and its CRT) and let the face pipeline run (ONNX Runtime); their licences follow.
if ($HasRuntimeLibs -eq 1) {
  Copy-Item (Join-Path $Stage '*.dll') $Portable
  Copy-Item $Lic $Portable -Recurse
}
# The marker that makes FilmCraft keep its data in .\data next to the exe instead of %APPDATA% (#139).
Set-Content -Encoding ascii -Path (Join-Path $Portable 'portable.txt') -Value @(
  'FilmCraft portable: preferences, presets, logs and crash recovery are kept in the data folder',
  'next to filmcraft.exe. Delete this file to use %APPDATA%\FilmCraft instead.'
)
foreach ($f in 'README.md', 'LICENSE', 'LICENSE-MIT', 'LICENSE-APACHE', 'NOTICE') {
  $p = Join-Path $Root $f
  if (Test-Path $p) { Copy-Item $p $Portable }
}
# Licences of the embedded craft-fonts fonts (builds with CRAFT_FONTS_DIR) as OFL-<family>.txt.
if ($env:CRAFT_FONTS_DIR) {
  foreach ($lic in Get-ChildItem -Path (Join-Path $env:CRAFT_FONTS_DIR 'fonts\*\OFL.txt') -ErrorAction SilentlyContinue) {
    Copy-Item $lic.FullName (Join-Path $Portable "OFL-$($lic.Directory.Name).txt")
  }
}
$Zip = Join-Path $Dist "filmcraft-$Version-windows-$Arch-portable.zip"
Remove-Item -Force $Zip -ErrorAction SilentlyContinue
Compress-Archive -Path $Portable -DestinationPath $Zip

# Smoke-test the CLI when this machine can run it. An ARM64 build made on an x64 runner can't run
# here; .github/workflows/windows-arm64.yml installs and runs it on ARM64 instead.
$HostArch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
if ($Arch -ne 'arm64' -or $HostArch -eq 'arm64') {
  Invoke-Native 'filmcraft-cli --version' { & (Join-Path $Stage 'filmcraft-cli.exe') --version }
} else {
  Write-Output "skipping filmcraft-cli --version: an $Arch build doesn't run on this $HostArch machine"
}
Get-Item $Msi, $Zip | Format-Table Name, Length
