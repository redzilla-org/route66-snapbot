# Build Zig and Nim transforms, then link both into the common Rust harness.
# Docker is explicit here so ordinary Rust builds never acquire container overhead.
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot

& (Join-Path $repo "zig\build.ps1")
if ($LASTEXITCODE -ne 0) { throw "Zig build failed" }
& (Join-Path $repo "nim\build.ps1")
if ($LASTEXITCODE -ne 0) { throw "Nim build failed" }

$savedRustFlags = $env:RUSTFLAGS
try {
  $env:RUSTFLAGS = "-C target-cpu=native"
  cargo build --release --manifest-path (Join-Path $PSScriptRoot "Cargo.toml") `
    --features zig,nim
  if ($LASTEXITCODE -ne 0) { throw "polyglot Rust link failed" }
  New-Item -ItemType Directory -Force -Path (Join-Path $PSScriptRoot "bin") | Out-Null
  Copy-Item (Join-Path $PSScriptRoot "target\release\rsbench.exe") `
    (Join-Path $PSScriptRoot "bin\rsbench_polyglot.exe") -Force
} finally {
  $env:RUSTFLAGS = $savedRustFlags
}
