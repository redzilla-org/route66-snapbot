# Build with the same floating-point contract as GOAMD64=v1. FMA contraction
# changes a handful of half-way luma rounding decisions, so it is disabled.
param([string]$Compiler = "clang++")

$ErrorActionPreference = "Stop"
$outDir = Join-Path $PSScriptRoot "bin"
$out = Join-Path $outDir "wuffsbench.exe"
$wuffsRelease = Join-Path $PSScriptRoot "upstream\release\c\wuffs-v0.3.c"

# A missing generated release means the repository was cloned without submodules.
# Fail with the repair command instead of surfacing a misleading C++ include error.
if (-not (Test-Path -LiteralPath $wuffsRelease)) {
  throw "Wuffs submodule missing; run: git submodule update --init wuffs/upstream"
}
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

& $Compiler -O3 -DNDEBUG -D_CRT_SECURE_NO_WARNINGS -std=c++17 -march=native -ffp-contract=off `
  -Wno-unused-function (Join-Path $PSScriptRoot "bench_wuffs.cc") -o $out
if ($LASTEXITCODE -ne 0) {
  throw "Wuffs build failed with exit code $LASTEXITCODE"
}
Write-Output $out
