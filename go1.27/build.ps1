# Go 1.27's SIMD packages are experimental, so the build must opt in explicitly.
# GOAMD64=v3 gives archsimd the same AVX2 baseline as the assembly contestant.
$goPath = (& go env GOPATH).Trim()
$go127 = Join-Path $goPath "bin\go1.27.0.exe"
if (-not (Test-Path -LiteralPath $go127)) {
    throw "go1.27.0 wrapper not found; run: go install golang.org/dl/go1.27.0@latest; go1.27.0 download"
}

$oldExperiment = $env:GOEXPERIMENT
$oldAmd64 = $env:GOAMD64
Push-Location $PSScriptRoot
try {
    $env:GOEXPERIMENT = "simd"
    $env:GOAMD64 = "v3"
    New-Item -ItemType Directory -Path .\bin -Force | Out-Null
    & $go127 build -pgo=off -o .\bin\go127bench.exe .
    if ($LASTEXITCODE -ne 0) {
        throw "Go 1.27 SIMD build failed"
    }
} finally {
    $env:GOEXPERIMENT = $oldExperiment
    $env:GOAMD64 = $oldAmd64
    Pop-Location
}
