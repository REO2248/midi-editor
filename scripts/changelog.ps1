# Drafts a CHANGELOG.md section from commit subjects.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/changelog.ps1 [-Since <ref>]
#
# Groups subjects by conventional prefix:
#   "breaking" / "BREAKING" / "!:" -> Breaking changes
#   "feat" / "add"               -> Added
#   "fix" / "bug"                -> Fixed
#   everything else              -> Changed
# Prints markdown to stdout - review, curate, then paste under the
# [Unreleased] heading in CHANGELOG.md. At release time rename Unreleased to
# the new version and tag v<semver>.

param([string]$Since)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not $Since) {
    $Since = git tag --sort=-v:refname | Select-Object -First 1
    if (-not $Since) { $Since = (git rev-list --max-parents=0 HEAD).Trim() }
}
Write-Host "# commits since $Since" -ForegroundColor DarkGray

$added = @(); $fixed = @(); $breaking = @(); $changed = @()
git log --no-merges --format=%s "$Since..HEAD" | ForEach-Object {
    $s = $_.Trim()
    if (-not $s) { return }
    if ($s -match "(?i)^breaking|BREAKING|!:")       { $breaking += $s }
    elseif ($s -match "(?i)^(feat|feature|add)\b")   { $added    += $s }
    elseif ($s -match "(?i)^(fix|bug|hotfix)\b")     { $fixed    += $s }
    else                                            { $changed  += $s }
}

function Emit-Section([string]$title, [string[]]$items) {
    if ($items.Count -eq 0) { return }
    Write-Output ""
    Write-Output "### $title"
    Write-Output ""
    foreach ($i in $items) { Write-Output "- $i" }
}

Write-Output "## [Unreleased]"
Emit-Section "Breaking" $breaking
Emit-Section "Added" $added
Emit-Section "Fixed" $fixed
Emit-Section "Changed" $changed
