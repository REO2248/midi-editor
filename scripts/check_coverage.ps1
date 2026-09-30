# Enforce per-crate line-coverage floors from a cargo-llvm-cov JSON export.
#
# Usage:  pwsh scripts/check_coverage.ps1 <coverage.json>
# where coverage.json is produced by
#   cargo llvm-cov report --json --summary-only > coverage.json
#
# Floors are the initial ratchet baseline for the safety-critical crates
# (parse, save, transaction, MCP validation, timing). Raise them as coverage
# improves; lowering one needs a review note in the PR that does it.

param(
    [Parameter(Mandatory = $true, Position = 0)] [string] $Report
)

$ErrorActionPreference = "Stop"

# crate name -> minimum line coverage % (measured 2026-09-30, ~5% headroom)
$Floors = [ordered]@{
    "smf-core"   = 80   # parse/serialize: measured 85.13%
    "document"   = 75   # transaction path + derived views: measured 83.67%
    "commands"   = 90   # undo stack: measured 98.19%
    "mcp-server" = 25   # tool dispatch + validation: measured 29.79%
}

$json = Get-Content -Raw $Report | ConvertFrom-Json

$byCrate = @{}
foreach ($f in $json.data.files) {
    if ($f.filename -match "crates[/\\]([^/\\]+)[/\\]") {
        $crate = $Matches[1]
        if (-not $byCrate.ContainsKey($crate)) {
            $byCrate[$crate] = [pscustomobject]@{ count = 0; covered = 0 }
        }
        $byCrate[$crate].count += $f.summary.lines.count
        $byCrate[$crate].covered += $f.summary.lines.covered
    }
}

if ($byCrate.Count -eq 0) {
    Write-Error "no crates/ source files found in $Report - is this a cargo llvm-cov --json export?"
}

$failed = $false
Write-Output ("{0,-12} {1,-14} {2,7} {3,7}  {4}" -f "crate", "lines", "cover", "floor", "result")
foreach ($crate in ($byCrate.Keys | Sort-Object)) {
    $c = $byCrate[$crate]
    if ($c.count -eq 0) { continue }
    $pct = [math]::Round(100.0 * $c.covered / $c.count, 2)
    if ($Floors.Contains($crate)) {
        $floor = $Floors[$crate]
        if ($pct -lt $floor) {
            $result = "BELOW FLOOR"
            $failed = $true
        } else {
            $result = "ok"
        }
        Write-Output ("{0,-12} {1,-14} {2,6}% {3,6}%  {4}" -f $crate, "$($c.covered)/$($c.count)", $pct, $floor, $result)
    } else {
        Write-Output ("{0,-12} {1,-14} {2,6}% {3,7}  {4}" -f $crate, "$($c.covered)/$($c.count)", $pct, "-", "report-only")
    }
}

if ($failed) {
    Write-Error "coverage floor check failed - new code in a floored crate landed without tests"
}
