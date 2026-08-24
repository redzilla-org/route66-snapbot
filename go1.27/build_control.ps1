# This control uses the same Go 1.27 compiler and experiment settings as GO127
# but deliberately compiles the existing handwritten-assembly Go contestant.
$goPath = (& go env GOPATH).Trim()
$go127 = Join-Path $goPath "bin\go1.27.0.exe"
if (-not (Test-Path -LiteralPath $go127)) {
    throw "go1.27.0 wrapper not found; run: go install golang.org/dl/go1.27.0@latest; go1.27.0 download"
}

$oldExperiment = $env:GOEXPERIMENT
$oldAmd64 = $env:GOAMD64
Push-Location (Join-Path $PSScriptRoot "..\go")
try {
    $env:GOEXPERIMENT = "simd"
    $env:GOAMD64 = "v3"
    & $go127 build -pgo=off -o ..\bin\bench_fastest127asm_v3.exe .
    if ($LASTEXITCODE -ne 0) {
        throw "Go 1.27 assembly control build failed"
    }
} finally {
    $env:GOEXPERIMENT = $oldExperiment
    $env:GOAMD64 = $oldAmd64
    Pop-Location
}
