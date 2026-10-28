# Builds and signs the distributable of the rAthena companion: gridspawn_rathena.dll
# (x86) and gridspawn-companion.exe (x64), signed with Azure Trusted Signing (signtool +
# the Trusted Signing dlib + its metadata JSON), each verified with `signtool verify /pa`,
# then staged with the header and licences and zipped.
#
#   .\package_windows.ps1 -AzureTrustedSigningDlibPath <dlib> -AzureTrustedSigningMetadataPath <json>
#
# The dlib and metadata paths come from parameters (or the environment variables
# GRIDSPAWN_TRUSTED_SIGNING_DLIB / GRIDSPAWN_TRUSTED_SIGNING_METADATA) and are never
# printed; neither is signtool's signing output (it can echo account values).
# Development builds stay unsigned (plain cargo). -SkipSigning exists only to check the
# build and staging steps: its output folder and zip are named "-UNSIGNED" and must not
# be given to players.
#
# Needs: Visual Studio 2022 (x64 and x86 tools), the Rust target i686-pc-windows-msvc,
# meson (on PATH or -MesonDir), LLVM (libclang, -LibclangDir). Keep the checkout path
# short: the WebRTC audio build stops with "Cannot open source file" under a long root.
param(
  [string]$AzureTrustedSigningDlibPath = $env:GRIDSPAWN_TRUSTED_SIGNING_DLIB,
  [string]$AzureTrustedSigningMetadataPath = $env:GRIDSPAWN_TRUSTED_SIGNING_METADATA,
  [string]$SignToolPath = "signtool.exe",
  [string]$SignTimestampUrl = "http://timestamp.digicert.com",
  [string]$OutputDir = "dist",
  [string]$MesonDir = "",
  [string]$LibclangDir = "C:\Program Files\LLVM\bin",
  [switch]$SkipBuild,
  [switch]$SkipSigning
)
$ErrorActionPreference = "Stop"

$companion = $PSScriptRoot
$dllCrate = [System.IO.Path]::GetFullPath((Join-Path $companion "..\gridspawn-rathena-dll"))
$version = (Select-String -Path (Join-Path $companion "Cargo.toml") -Pattern '^version = "(.+)"$' |
  Select-Object -First 1).Matches[0].Groups[1].Value

function Assert-SigningReady {
  if (!(Get-Command $SignToolPath -ErrorAction SilentlyContinue)) {
    throw "signtool not found (pass -SignToolPath)."
  }
  if ([string]::IsNullOrWhiteSpace($AzureTrustedSigningDlibPath) -or
      !(Test-Path $AzureTrustedSigningDlibPath -PathType Leaf)) {
    throw "The Azure Trusted Signing dlib is missing (pass -AzureTrustedSigningDlibPath)."
  }
  if ([string]::IsNullOrWhiteSpace($AzureTrustedSigningMetadataPath) -or
      !(Test-Path $AzureTrustedSigningMetadataPath -PathType Leaf)) {
    throw "The Azure Trusted Signing metadata JSON is missing (pass -AzureTrustedSigningMetadataPath)."
  }
}

function Invoke-Signed([string]$Path) {
  $name = [System.IO.Path]::GetFileName($Path)
  # Output discarded on purpose: never print signing account values.
  & $SignToolPath sign /fd SHA256 /td SHA256 /tr $SignTimestampUrl `
    /dlib $AzureTrustedSigningDlibPath /dmdf $AzureTrustedSigningMetadataPath $Path *> $null
  if ($LASTEXITCODE -ne 0) { throw "Signing failed: $name (signtool exit $LASTEXITCODE)" }
  & $SignToolPath verify /pa $Path *> $null
  if ($LASTEXITCODE -ne 0) { throw "Signature verification failed: $name (signtool exit $LASTEXITCODE)" }
  Write-Host "Signed and verified: $name"
}

function Get-VcVarsAll {
  $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
  if (!(Test-Path $vswhere)) { throw "vswhere not found: install Visual Studio 2022 with C++." }
  $root = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
  $vcvars = Join-Path $root "VC\Auxiliary\Build\vcvarsall.bat"
  if (!(Test-Path $vcvars)) { throw "vcvarsall.bat not found under the Visual Studio installation." }
  return $vcvars
}

# Each build runs in its own cmd with the matching MSVC environment: the i686 DLL needs
# the x86 libraries, the companion the x64 tools plus meson and libclang.
function Invoke-Build([string]$Arch, [string]$Dir, [string]$CargoArgs, [string]$Extra = "") {
  $vcvars = Get-VcVarsAll
  $line = "call `"$vcvars`" $Arch >nul && $Extra cd /d `"$Dir`" && cargo build --release --locked $CargoArgs"
  & cmd.exe /d /c $line
  if ($LASTEXITCODE -ne 0) { throw "Build failed in $Dir ($Arch)." }
}

if (-not $SkipSigning) { Assert-SigningReady }
if ($companion.Length -gt 24) {
  Write-Warning "Long checkout path ($($companion.Length) characters): the WebRTC build may fail with 'Cannot open source file'."
}

if (-not $SkipBuild) {
  Invoke-Build "x64_x86" $dllCrate "--target i686-pc-windows-msvc"
  $env_ = "set CC=cl&& set CXX=cl&& set `"LIBCLANG_PATH=$LibclangDir`"&&"
  if ($MesonDir) { $env_ += " set `"PATH=$MesonDir;%PATH%`"&&" }
  Invoke-Build "x64" $companion "" $env_
}

$dll = Join-Path $dllCrate "target\i686-pc-windows-msvc\release\gridspawn_rathena.dll"
$exe = Join-Path $companion "target\release\gridspawn-companion.exe"
foreach ($file in @($dll, $exe)) {
  if (!(Test-Path $file -PathType Leaf)) { throw "Missing build output: $([System.IO.Path]::GetFileName($file))" }
}

# The package must carry no testing switches: the companion is built without the
# `testing` feature (no tone code), and the example client's --next-ticket-file is
# not packaged. Checked in the binaries themselves, not assumed.
$testingMarkers = @("test tone instead of the microphone", "test tone: loudest received", "GRIDSPAWN_TEST_SIGNAL",
  "GRIDSPAWN_TEST_PROCESSING", "next-ticket-file")
foreach ($file in @($dll, $exe)) {
  $text = [System.Text.Encoding]::ASCII.GetString([System.IO.File]::ReadAllBytes($file))
  foreach ($marker in $testingMarkers) {
    if ($text.Contains($marker)) {
      throw "Testing-only code found in $([System.IO.Path]::GetFileName($file)) ('$marker'): rebuild without --features testing."
    }
  }
}
Write-Host "No testing switches in the binaries."

$label = "gridspawn-rathena-$version-windows"
if ($SkipSigning) { $label += "-UNSIGNED" }
if (![System.IO.Path]::IsPathRooted($OutputDir)) { $OutputDir = Join-Path $companion $OutputDir }
$stage = Join-Path $OutputDir $label
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $dll, $exe $stage
Copy-Item (Join-Path $dllCrate "include\gridspawn_rathena.h") $stage
Copy-Item (Join-Path $companion "LICENSE-MIT"), (Join-Path $companion "LICENSE-APACHE") $stage

if ($SkipSigning) {
  Write-Warning "Not signed (-SkipSigning): for checking the build only, never for players."
} else {
  Invoke-Signed (Join-Path $stage "gridspawn_rathena.dll")
  Invoke-Signed (Join-Path $stage "gridspawn-companion.exe")
}

$zip = Join-Path $OutputDir "$label.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip
Write-Host "Packaged: $label.zip"
