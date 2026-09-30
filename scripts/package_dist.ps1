# package_dist.ps1 — assemble + validate the distributable layout.
#
# Produces dist/:
#   midi-editor.exe           (release build)
#   vst3-host-helper.exe      (cargo install vst3-host, or -HelpersDir)
#   vst3-host-probe.exe
#   assets\icons\*.svg        (app icons)
#   licenses\LICENSE-MIT / LICENSE-APACHE
#
# Then validates every expected file exists and is non-empty, checks each
# .exe is a real x64 PE image, and runs the packaged binary's --smoke mode
# (load fixture MIDI, print document summary, save a copy, exit — no audio
# hardware or GUI session needed). Any missing/invalid piece fails nonzero.
#
#   powershell -File scripts/package_dist.ps1
#     [-DistDir dist] [-HelpersDir <dir-with-helper-exes>]
#     [-SkipBuild] [-SkipSmoke]
param(
    [string]$DistDir = "dist",
    [string]$HelpersDir = "",
    [switch]$SkipBuild,
    [switch]$SkipSmoke
)

$ErrorActionPreference = "Stop"
Set-Location -LiteralPath (Split-Path -Parent $PSScriptRoot) -ErrorAction Stop
$root = (Get-Location).Path
$dist = Join-Path $root $DistDir

function Fail([string]$msg) { Write-Error $msg }   # stops via $ErrorActionPreference

# ---------- 1. release build ----------
if (-not $SkipBuild) {
    Write-Host "== cargo build --release --locked"
    & cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { Fail "cargo build failed ($LASTEXITCODE)" }
}
$appExe = Join-Path $root "target\release\midi-editor.exe"
if (-not (Test-Path $appExe)) { Fail "missing build output: $appExe" }

# ---------- 2. VST3 host helper + probe ----------
if (-not $HelpersDir) {
    $HelpersDir = Join-Path $root "target\vst3-host-pkg\bin"
    if (-not ((Test-Path "$HelpersDir\vst3-host-helper.exe") -and (Test-Path "$HelpersDir\vst3-host-probe.exe"))) {
        Write-Host "== cargo install vst3-host --locked --root target\vst3-host-pkg"
        & cargo install vst3-host --locked --root (Join-Path $root "target\vst3-host-pkg")
        if ($LASTEXITCODE -ne 0) { Fail "cargo install vst3-host failed ($LASTEXITCODE)" }
    }
}

# ---------- 3. assemble dist ----------
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory -Force "$dist\assets", "$dist\licenses", "$dist\smoke" | Out-Null

Copy-Item $appExe "$dist\midi-editor.exe"
foreach ($h in @("vst3-host-helper.exe", "vst3-host-probe.exe")) {
    $src = Join-Path $HelpersDir $h
    if (-not (Test-Path $src)) { Fail "missing VST3 host binary: $src" }
    Copy-Item $src "$dist\$h"
}
Copy-Item (Join-Path $root "crates\app\assets\icons") "$dist\assets\icons" -Recurse
foreach ($lic in @("LICENSE-MIT", "LICENSE-APACHE")) {
    $src = Join-Path $root $lic
    if (-not (Test-Path $src)) { Fail "missing license file: $src" }
    Copy-Item $src "$dist\licenses\$lic"
}

# ---------- 4. fixture MIDI for smoke (generated — no licensing surface) ----------
function Add-Vlq([System.Collections.Generic.List[byte]]$Out, [uint64]$N) {
    $groups = [System.Collections.Generic.List[byte]]::new()
    while ($true) {
        $groups.Insert(0, [byte]($N -band 0x7F))
        $N = $N -shr 7
        if ($N -eq 0) { break }
    }
    for ($i = 0; $i -lt $groups.Count - 1; $i++) { $groups[$i] = $groups[$i] -bor 0x80 }
    $Out.AddRange($groups)
}
function Write-FixtureMidi([string]$Path) {
    $b = [System.Collections.Generic.List[byte]]::new()
    $b.AddRange([byte[]](0x4D,0x54,0x68,0x64, 0,0,0,6, 0,1, 0,2, 0x01,0xE0)) # MThd fmt1 2trk 480ppq
    $trk0 = [System.Collections.Generic.List[byte]]::new()
    $trk0.AddRange([byte[]](0x00,0xFF,0x51,0x03,0x07,0xA1,0x20))  # tempo 500000us
    $trk0.AddRange([byte[]](0x00,0xFF,0x2F,0x00))                # EOT
    $trk1 = [System.Collections.Generic.List[byte]]::new()
    foreach ($k in @(60,64,67)) {
        Add-Vlq $trk1 240; $trk1.AddRange([byte[]](0x90,$k,96))
        Add-Vlq $trk1 240; $trk1.AddRange([byte[]](0x80,$k,0))
    }
    $trk1.AddRange([byte[]](0x00,0xFF,0x2F,0x00))
    foreach ($t in @($trk0, $trk1)) {
        $b.AddRange([byte[]](0x4D,0x54,0x72,0x6B))
        $len = $t.Count
        $b.AddRange([byte[]]([int]($len -shr 24 -band 255),[int]($len -shr 16 -band 255),[int]($len -shr 8 -band 255),[int]($len -band 255)))
        $b.AddRange($t.ToArray())
    }
    [System.IO.File]::WriteAllBytes($Path, $b.ToArray())
}
$fixture = Join-Path $dist "smoke\fixture.mid"
Write-FixtureMidi $fixture

# ---------- 5. validate layout + architecture ----------
$expected = @(
    "midi-editor.exe", "vst3-host-helper.exe", "vst3-host-probe.exe",
    "licenses\LICENSE-MIT", "licenses\LICENSE-APACHE"
)
foreach ($f in $expected) {
    $p = Join-Path $dist $f
    if (-not (Test-Path $p)) { Fail "dist missing: $f" }
    if ((Get-Item $p).Length -lt 1) { Fail "dist file is empty: $f" }
}
$iconCount = @(Get-ChildItem "$dist\assets\icons" -Filter *.svg -ErrorAction SilentlyContinue).Count
if ($iconCount -lt 1) { Fail "dist missing icon assets (assets\icons\*.svg)" }

function Assert-PeX64([string]$Path) {
    $fs = [System.IO.File]::OpenRead($Path)
    try {
        $br = New-Object System.IO.BinaryReader($fs)
        if ($br.ReadUInt16() -ne 0x5A4D) { Fail "${Path}: not a PE image (no MZ)" }
        $fs.Seek(0x3C, 'Begin') | Out-Null
        $pe = $br.ReadUInt32()
        $fs.Seek([int64]$pe, 'Begin') | Out-Null
        if ($br.ReadUInt32() -ne 0x00004550) { Fail "${Path}: no PE signature" }
        $machine = $br.ReadUInt16()
        if ($machine -ne 0x8664) { Fail ("${Path}: machine 0x{0:X4} is not x64 (0x8664)" -f $machine) }
    } finally { $fs.Close() }
}
foreach ($exe in @("midi-editor.exe", "vst3-host-helper.exe", "vst3-host-probe.exe")) {
    Assert-PeX64 (Join-Path $dist $exe)
}

# ---------- 6. smoke ----------
if (-not $SkipSmoke) {
    $copy = Join-Path $dist "smoke\smoke_copy.mid"
    Write-Host "== $dist\midi-editor.exe --smoke smoke\fixture.mid smoke\smoke_copy.mid"
    $out = & (Join-Path $dist "midi-editor.exe") --smoke $fixture $copy 2>&1
    $out | ForEach-Object { Write-Host "   $_" }
    if ($LASTEXITCODE -ne 0) { Fail "smoke exited $LASTEXITCODE" }
    if (-not (Test-Path $copy)) { Fail "smoke did not produce $copy" }
    $head = [System.IO.File]::ReadAllBytes($copy)[0..3] -join ','
    if ($head -ne "77,84,104,100") { Fail "$copy does not start with MThd" }
    if (-not (($out -join ' ') -match '"tracks"')) { Fail "smoke output missing document summary" }
}

Write-Host "== dist OK:"
Get-ChildItem -Recurse $dist | ForEach-Object { Write-Host ("   " + $_.FullName.Substring($dist.Length + 1)) }
Write-Host "== package_dist PASS"
