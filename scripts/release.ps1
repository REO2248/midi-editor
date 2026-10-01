# Builds release artifacts and writes reproducible release metadata.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/release.ps1
#
# Produces in dist\:
#   midi-editor-<ver>-windows-x64.zip   staged binaries + licenses + docs
#   <name>-setup.exe (when scripts/package.ps1 exists and built one)
#   SHA256SUMS.txt                    sha256sum-style list of shipped files
#   manifest.json                     version / commit / dirty provenance
#
# Tag the release afterwards:  git tag -a v<semver> -m "release <semver>"

param(
    [string]$Vcvars = "C:\Users\Administrator\vcargo.cmd",
    [switch]$SkipPackage,   # do not run scripts/package.ps1 even if present
    [switch]$SkipBuild      # reuse whatever is already built / staged
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Invoke-Cargo {
    param([Parameter(Mandatory = $true)][string]$CargoArgs)
    # MSYS2 link.exe shadows MSVC link.exe - always go through vcvars
    & cmd /c "`"$Vcvars`" >NUL 2>&1 && cargo $CargoArgs"
    if ($LASTEXITCODE -ne 0) { throw "cargo $CargoArgs failed ($LASTEXITCODE)" }
}

# --- identity: semver from the workspace manifest, commit from git ---------
$meta = (& cmd /c "`"$Vcvars`" >NUL 2>&1 && cargo metadata --format-version 1 --no-deps") | ConvertFrom-Json
$version = ($meta.packages | Where-Object { $_.name -eq "midi-editor" }).version
if (-not $version) { throw "could not resolve midi-editor package version" }
$sha = (git rev-parse --short HEAD).Trim()
$dirty = [bool](git status --porcelain --untracked-files=no)
$identity = "$version+$sha" + $(if ($dirty) { ".dirty" } else { "" })
Write-Host "release identity: $identity"

# --- build -----------------------------------------------------------------
if (-not $SkipBuild) {
    if ((Test-Path "scripts\package.ps1") -and -not $SkipPackage) {
        & powershell -NoProfile -ExecutionPolicy Bypass -File scripts\package.ps1 -Vcvars $Vcvars
        if ($LASTEXITCODE -ne 0) { throw "package.ps1 failed" }
    } else {
        Invoke-Cargo "build --release --locked --workspace"
    }
}

# --- stage release payload --------------------------------------------------
$payload = "midi-editor-$version-windows-x64"
$relDir = "dist\release"
New-Item -ItemType Directory -Force $relDir | Out-Null
$stage = Join-Path $relDir $payload
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null

# bin names come from cargo metadata (this checkout's truth) — never from
# filesystem probing, since target\ accumulates stale exes across branches;
# the vst3-host-*.exe helpers are external (cargo install), listed manually
$bins = @(
    $meta.packages |
        Where-Object { $_.name -in @("midi-editor", "mcp-server") } |
        ForEach-Object { $_.targets } |
        Where-Object { $_.kind -contains "bin" } |
        ForEach-Object { "$($_.name).exe" }
) + @("vst3-host-helper.exe", "vst3-host-probe.exe")
$docs = @("LICENSE-MIT", "LICENSE-APACHE", "README.md", "CHANGELOG.md",
          "docs\UPGRADING.md", "THIRD-PARTY-NOTICES.txt")

# core exes always come from this build's target\release — never a stale
# dist\stage dir; only the vst3-host-*.exe helpers may come from stage
# (they are produced by cargo install, outside the workspace build)
foreach ($b in $bins) {
    $src = if ($b -like "vst3-host-*" -and (Test-Path "dist\stage\$b")) { "dist\stage\$b" }
           elseif (Test-Path "target\release\$b") { "target\release\$b" }
           else { $null }
    if ($src) { Copy-Item $src $stage }
}
foreach ($d in $docs) {
    if (Test-Path $d) { Copy-Item $d $stage }
}

# --- zip + checksums + manifest ----------------------------------------------
$zip = "dist\$payload.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path "$stage\*" -DestinationPath $zip

$shipped = @($zip)
$shipped += Get-ChildItem "dist\midi-editor-$version-setup.exe" -ErrorAction SilentlyContinue |
    ForEach-Object { $_.FullName }

$sums = @()
$artifacts = @()
foreach ($f in $shipped) {
    $h = Get-FileHash $f -Algorithm SHA256
    $name = Split-Path -Leaf $f
    $sums += "$($h.Hash.ToLower())  $name"
    $artifacts += [ordered]@{
        file   = $name
        sha256 = $h.Hash.ToLower()
        bytes  = (Get-Item $f).Length
    }
}
$sums | Set-Content -Encoding ASCII "dist\SHA256SUMS.txt"

$manifest = [ordered]@{
    package       = "midi-editor"
    version       = $version
    build_identity = $identity
    git_sha       = $sha
    git_dirty     = $dirty
    generated_utc = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    artifacts     = $artifacts
}
$manifest | ConvertTo-Json -Depth 5 | Set-Content -Encoding ASCII "dist\manifest.json"

Write-Host "zip:      $zip"
Write-Host "sums:     dist\SHA256SUMS.txt"
Write-Host "manifest: dist\manifest.json"
Write-Host "next:     git tag -a v$version -m `"release $version`""
