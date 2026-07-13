# assert-not-linked.ps1 - assert a Windows artifact's dependency imports do NOT
# contain any of the given needles (#167). PowerShell sibling of
# assert-not-linked.sh; reuses ci.yml's existing dumpbin-via-vswhere discovery
# (dumpbin ships with MSVC but is not on PATH; vswhere is preinstalled on
# GitHub-hosted Windows runners).
#
#   Usage: scripts/assert-not-linked.ps1 <path-to-artifact> <needle1,needle2,...>
#
# Needles are a single comma-separated string (not an array) so the call site
# in release.yml stays a one-liner; each needle is matched case-insensitively
# as a regex alternative against the dumpbin output.

param(
    [Parameter(Mandatory = $true)][string]$Artifact,
    [Parameter(Mandatory = $true)][string]$Needles
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $Artifact)) {
    Write-Error "FAIL: no artifact at $Artifact"
    exit 1
}

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$dumpbin = & $vswhere -latest -find '**\dumpbin.exe' | Select-Object -First 1
if (-not $dumpbin) { Write-Error 'FAIL: dumpbin.exe not found via vswhere'; exit 1 }

Write-Host "== $dumpbin /DEPENDENTS $Artifact =="
$deps = & $dumpbin /DEPENDENTS $Artifact
$deps | Write-Host

$pattern = ($Needles -split ',') -join '|'
if ($deps -match "(?i)$pattern") {
    Write-Error "FAIL: $Artifact unexpectedly imports a DLL matching: $Needles"
    exit 1
}

Write-Host "OK: $Artifact carries no dynamic import matching: $Needles"
