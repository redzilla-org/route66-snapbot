param(
    [string] $SourcePath = '',
    [string] $OutPath = 'bin\rust\eng.traineddata'
)

$ErrorActionPreference = 'Stop'

# WHY: Tesseract model files are runtime inputs. A static ocrd.exe removes the
# native DLL dependency, but it still needs eng.traineddata at first OCR, so the
# release must carry the model file as a separate artifact.
$repoRoot = Resolve-Path (Join-Path $PSScriptRoot '..')
$resolvedOut = if ([System.IO.Path]::IsPathRooted($OutPath)) {
    $OutPath
} else {
    Join-Path $repoRoot $OutPath
}

# WHY: prefer an explicit path when the release operator supplies one, then the
# standard Tesseract runtime location. vcpkg installs libraries and headers, not
# language data, so this intentionally fails instead of pretending the static
# toolchain provides a model.
$candidates = @()
if (-not [string]::IsNullOrWhiteSpace($SourcePath)) {
    $candidates += $SourcePath
}
if (-not [string]::IsNullOrWhiteSpace($env:TESSDATA_PREFIX)) {
    $candidates += (Join-Path $env:TESSDATA_PREFIX 'eng.traineddata')
}
$candidates += @(
    'C:\Program Files\Tesseract-OCR\tessdata\eng.traineddata',
    'C:\Program Files (x86)\Tesseract-OCR\tessdata\eng.traineddata',
    'C:\ProgramData\Tesseract-OCR\tessdata\eng.traineddata'
)

$source = $candidates | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
if (-not $source) {
    throw "eng.traineddata was not found. Pass -SourcePath or set TESSDATA_PREFIX to a directory containing eng.traineddata."
}

# WHY: keep the release asset name exactly what Tesseract expects on disk. The
# install side can use the release download directory itself as TESSDATA_PREFIX.
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $resolvedOut) | Out-Null
Copy-Item -LiteralPath $source -Destination $resolvedOut -Force
Write-Host "Wrote $resolvedOut from $source"
