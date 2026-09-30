#requires -Version 5
# Builds midi-editor release binaries, stages them with the VST3 host helpers
# and license files, and compiles installer\midi-editor.iss into
# dist\midi-editor-<version>-setup.exe.
#
#   powershell -File scripts\package.ps1            # full package
#   powershell -File scripts\package.ps1 -SkipInstaller   # stage only
#
# Code signing is opt-in: set MIDI_EDITOR_SIGN_PFX to a .pfx path (and
# MIDI_EDITOR_SIGN_PFX_PASSWORD / MIDI_EDITOR_SIGN_TIMESTAMP if needed) and
# signtool signs the staged binaries and the setup executable. See
# docs\packaging.md for the signing + key-handling policy.
param(
    [string]$Vst3HostVersion = "0.9.0",
    [string]$Vcvars = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat",
    [switch]$SkipHelpers,
    [switch]$SkipInstaller
)

$ErrorActionPreference = "Stop"
$repo   = Split-Path -Parent $PSScriptRoot
$dist   = Join-Path $repo "dist"
$stage  = Join-Path $dist "stage"
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

function Invoke-Cargo([string]$CargoArgs) {
    # bare cargo links through MSYS2 link.exe on Git Bash - every invocation
    # goes through vcvars64 so MSVC link.exe wins, same as vcargo.cmd
    & cmd /c "`"$Vcvars`" >NUL 2>&1 && cargo $CargoArgs"
    if ($LASTEXITCODE -ne 0) { throw "cargo $CargoArgs failed (exit $LASTEXITCODE)" }
}

function Sign-File([string]$File) {
    if (-not $env:MIDI_EDITOR_SIGN_PFX) { return }
    $signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
    if (-not $signtool) { throw "MIDI_EDITOR_SIGN_PFX set but signtool.exe was not found" }
    $ts = if ($env:MIDI_EDITOR_SIGN_TIMESTAMP) { $env:MIDI_EDITOR_SIGN_TIMESTAMP } else { "http://timestamp.digicert.com" }
    $args = @("sign", "/fd", "sha256", "/td", "sha256", "/tr", $ts, "/f", $env:MIDI_EDITOR_SIGN_PFX)
    if ($env:MIDI_EDITOR_SIGN_PFX_PASSWORD) { $args += @("/p", $env:MIDI_EDITOR_SIGN_PFX_PASSWORD) }
    $args += $File
    & $signtool @args | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "signtool failed on $File" }
    Write-Host "signed $File"
}

# ---- version comes from the workspace crate, not a second source of truth
$meta = & cargo metadata --format-version 1 --no-deps | ConvertFrom-Json
$pkg  = $meta.packages | Where-Object { $_.name -eq "midi-editor" } | Select-Object -First 1
$version = $pkg.version
Write-Host "packaging midi-editor $version"

# ---- release binaries
Invoke-Cargo "build --release --locked --bin midi-editor --bin mcp-bridge"

New-Item -ItemType Directory -Force -Path $stage | Out-Null
Remove-Item "$stage\*" -Recurse -Force -ErrorAction SilentlyContinue
Copy-Item (Join-Path $repo "target\release\midi-editor.exe") $stage
Copy-Item (Join-Path $repo "target\release\mcp-bridge.exe")  $stage
Copy-Item (Join-Path $repo "LICENSE-MIT")                   $stage
Copy-Item (Join-Path $repo "LICENSE-APACHE")                $stage
if (Test-Path (Join-Path $repo "THIRD-PARTY-NOTICES.txt")) {
    Copy-Item (Join-Path $repo "THIRD-PARTY-NOTICES.txt")   $stage
}

# ---- VST3 process-isolation helpers ship from the vst3-host crate
if (-not $SkipHelpers) {
    $helperRoot = Join-Path $dist "vst3host"
    Invoke-Cargo "install vst3-host --version =$Vst3HostVersion --locked --root `"$helperRoot`""
    foreach ($h in "vst3-host-helper.exe", "vst3-host-probe.exe") {
        $src = Join-Path $helperRoot "bin\$h"
        if (Test-Path $src) { Copy-Item $src $stage } else { Write-Warning "$h was not produced by cargo install" }
    }
}

foreach ($exe in Get-ChildItem $stage -Filter *.exe) { Sign-File $exe.FullName }

if ($SkipInstaller) { Write-Host "staged at $stage (installer skipped)"; exit 0 }

# ---- compile the installer
$iscc = Get-ChildItem "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "${env:ProgramFiles}\Inno Setup 6\ISCC.exe" -ErrorAction SilentlyContinue |
    Select-Object -First 1 -ExpandProperty FullName
if (-not $iscc) {
    $cmd = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($cmd) { $iscc = $cmd.Source }
}
if (-not $iscc) { throw "ISCC.exe not found - install Inno Setup 6 (https://jrsoftware.org/isinfo.php)" }

& $iscc "/DAppVersion=$version" "/DSourceDir=$stage" "/DOutputDir=$dist" (Join-Path $repo "installer\midi-editor.iss")
if ($LASTEXITCODE -ne 0) { throw "ISCC failed (exit $LASTEXITCODE)" }

$setup = Join-Path $dist "midi-editor-$version-setup.exe"
Sign-File $setup
Write-Host "installer: $setup"
