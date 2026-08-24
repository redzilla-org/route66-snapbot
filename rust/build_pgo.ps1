# Build Rust K with LLVM instrumentation, train both scale paths, then rebuild
# with the merged profile so the benchmark can reproduce the PGO contestant.
param(
  [string]$LlvmProfdata = "",
  [ValidateSet("HYBRID", "KLUT", "KSTREAM")]
  [string]$ProfileVariant = "KSTREAM"
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$targetRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "target"))
$profileDir = [IO.Path]::GetFullPath((Join-Path $targetRoot "pgo-data"))
$generateDir = [IO.Path]::GetFullPath((Join-Path $targetRoot "pgo-generate"))
$useDir = [IO.Path]::GetFullPath((Join-Path $targetRoot "pgo-use"))
$mergedProfile = Join-Path $targetRoot "pgo.profdata"
$outputName = switch ($ProfileVariant) {
  "KSTREAM" { "rsbench_kstreampgo.exe" }
  "KLUT" { "rsbench_rustpgo.exe" }
  default { "rsbench_nativefatpgo.exe" }
}
$outputBinary = Join-Path $PSScriptRoot "bin\$outputName"

# Rust raw-profile formats follow rustc's bundled LLVM, which can be newer than
# a system LLVM installation. Use the active toolchain's merger unless overridden.
if ([string]::IsNullOrWhiteSpace($LlvmProfdata)) {
  $rustSysroot = (& rustc --print sysroot).Trim()
  if ($LASTEXITCODE -ne 0) {
    throw "could not discover the active Rust sysroot"
  }
  $LlvmProfdata = Join-Path $rustSysroot `
    "lib\rustlib\x86_64-pc-windows-msvc\bin\llvm-profdata.exe"
}
if (-not (Test-Path -LiteralPath $LlvmProfdata)) {
  throw "matching llvm-profdata missing; run: rustup component add llvm-tools-preview"
}

# Fresh profile data is mandatory. Validate every recursive-delete target against
# rust/target before removing it so a malformed path cannot affect source files.
foreach ($path in @($profileDir, $generateDir, $useDir)) {
  if (-not $path.StartsWith($targetRoot + [IO.Path]::DirectorySeparatorChar,
      [StringComparison]::OrdinalIgnoreCase)) {
    throw "unsafe PGO target path: $path"
  }
  if (Test-Path -LiteralPath $path) {
    Remove-Item -LiteralPath $path -Recurse -Force
  }
}
if (Test-Path -LiteralPath $mergedProfile) {
  Remove-Item -LiteralPath $mergedProfile -Force
}
New-Item -ItemType Directory -Force -Path $profileDir | Out-Null
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $outputBinary) | Out-Null

$savedRustFlags = $env:RUSTFLAGS
try {
  # Instrument the complete release graph, including the png dependency, while
  # preserving the native-CPU flags used by the non-PGO comparison binary.
  $env:RUSTFLAGS = "-C target-cpu=native -C profile-generate=$($profileDir.Replace('\', '/'))"
  cargo build --release --manifest-path (Join-Path $PSScriptRoot "Cargo.toml") `
    --target-dir $generateDir
  if ($LASTEXITCODE -ne 0) {
    throw "Rust PGO instrumented build failed with exit code $LASTEXITCODE"
  }

  $instrumented = Join-Path $generateDir "release\rsbench.exe"
  # Four factor-2 runs and one factor-1 run encode the declared 80/20 production
  # mix into LLVM's profile. Each process uses the benchmark's normal warmups and
  # 250 ms timed window, so both startup and steady-state paths receive samples.
  foreach ($trainingRun in 1..4) {
    & $instrumented $ProfileVariant (Join-Path $repoRoot "fixtures\typical.png") `
      (Join-Path $repoRoot "out\pgo_train_typical.pgm") | Out-Null
    if ($LASTEXITCODE -ne 0) {
      throw "Rust PGO typical training failed with exit code $LASTEXITCODE"
    }
  }
  & $instrumented $ProfileVariant (Join-Path $repoRoot "fixtures\big.png") `
    (Join-Path $repoRoot "out\pgo_train_big.pgm") | Out-Null
  if ($LASTEXITCODE -ne 0) {
    throw "Rust PGO big training failed with exit code $LASTEXITCODE"
  }

  $rawProfiles = @(Get-ChildItem -LiteralPath $profileDir -Filter "*.profraw" -File)
  if (-not $rawProfiles.Count) {
    throw "Rust PGO training produced no .profraw files"
  }
  & $LlvmProfdata merge -o $mergedProfile @($rawProfiles.FullName)
  if ($LASTEXITCODE -ne 0) {
    throw "llvm-profdata merge failed with exit code $LASTEXITCODE"
  }

  # Warn on missing profile coverage so dependency or workload drift is visible
  # instead of silently producing a partially trained release binary.
  $profileUse = $mergedProfile.Replace('\', '/')
  $env:RUSTFLAGS = "-C target-cpu=native -C profile-use=$profileUse -C llvm-args=-pgo-warn-missing-function"
  cargo build --release --manifest-path (Join-Path $PSScriptRoot "Cargo.toml") `
    --target-dir $useDir
  if ($LASTEXITCODE -ne 0) {
    throw "Rust PGO profile-use build failed with exit code $LASTEXITCODE"
  }
  Copy-Item -LiteralPath (Join-Path $useDir "release\rsbench.exe") `
    -Destination $outputBinary -Force
  Write-Output $outputBinary
} finally {
  $env:RUSTFLAGS = $savedRustFlags
}
