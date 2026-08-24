param(
    [string] $VcpkgRoot = $(if ($env:VCPKG_ROOT) { $env:VCPKG_ROOT } else { 'C:\Users\alexr\vcpkg' }),
    [switch] $SkipVcpkgInstall
)

$ErrorActionPreference = 'Stop'

# WHY: the static executable depends on the static vcpkg triplet existing
# before Cargo's tesseract/leptonica -sys build scripts run. The dynamic
# x64-windows triplet already present on this host cannot be reused because it
# emits import-library links to DLLs.
$vcpkgExe = Join-Path $VcpkgRoot 'vcpkg.exe'
if (-not (Test-Path -LiteralPath $vcpkgExe)) {
    throw "vcpkg.exe was not found at $vcpkgExe. Pass -VcpkgRoot or set VCPKG_ROOT."
}

# WHY: vcpkg is idempotent here. If the static triplet is already installed,
# this returns quickly; if not, it performs the one-time Tesseract dependency
# build that gives Cargo actual .lib archives to link into the executable.
if (-not $SkipVcpkgInstall) {
    & $vcpkgExe install 'tesseract:x64-windows-static'
    if ($LASTEXITCODE -ne 0) {
        throw "vcpkg install tesseract:x64-windows-static failed with exit code $LASTEXITCODE"
    }
}

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot '..')
$crateRoot = Join-Path $repoRoot 'ocrd-rust'
$outDir = Join-Path $repoRoot 'bin\rust'
$targetDir = Join-Path $crateRoot 'target-static'
$builtExe = Join-Path $targetDir 'x86_64-pc-windows-msvc\release\ocrd.exe'
$artifact = Join-Path $outDir 'ocrd-windows-amd64-static.exe'

# WHY: these environment variables are process-local build inputs. The script
# restores the caller's shell afterwards so a static build cannot accidentally
# poison later dynamic Cargo or vcpkg work in the same terminal.
$saved = @{
    VCPKG_ROOT = $env:VCPKG_ROOT
    VCPKGRS_TRIPLET = $env:VCPKGRS_TRIPLET
    VCPKGRS_DYNAMIC = $env:VCPKGRS_DYNAMIC
    RUSTFLAGS = $env:RUSTFLAGS
}

try {
    $env:VCPKG_ROOT = $VcpkgRoot
    $env:VCPKGRS_TRIPLET = 'x64-windows-static'

    # WHY: VCPKGRS_DYNAMIC asks vcpkg-rs for the DLL triplet. It must be absent,
    # not merely set to false, for static library discovery.
    Remove-Item Env:\VCPKGRS_DYNAMIC -ErrorAction SilentlyContinue

    # WHY: vcpkg's x64-windows-static triplet links the native dependency graph
    # to the static MSVC runtime. Rust must use the same CRT mode or the final
    # executable can still depend on vcruntime/ucrt DLLs.
    # WHY: vcpkg-rs emits the static third-party libraries, but not every
    # transitive Win32 import library used by libarchive, libcurl and OpenSSL.
    # Keep those system libraries here, at the final link boundary, instead of
    # baking local absolute paths into source.
    $staticFlags = @(
        '-C target-feature=+crt-static',
        '-C link-arg=Advapi32.lib',
        '-C link-arg=Crypt32.lib',
        '-C link-arg=Xmllite.lib',
        '-C link-arg=User32.lib',
        '-C link-arg=Bcrypt.lib',
        '-C link-arg=Iphlpapi.lib',
        '-C link-arg=Secur32.lib'
    )
    $staticFlagString = $staticFlags -join ' '
    if ([string]::IsNullOrWhiteSpace($saved.RUSTFLAGS)) {
        $env:RUSTFLAGS = $staticFlagString
    } elseif ($saved.RUSTFLAGS -notlike '*target-feature=+crt-static*') {
        $env:RUSTFLAGS = "$($saved.RUSTFLAGS) $staticFlagString"
    }

    # WHY: a dedicated target directory prevents Cargo from reusing build-script
    # products created for the dynamic vcpkg triplet. The native target is still
    # explicit so the artifact path is stable on any Windows Rust install.
    Push-Location $crateRoot
    try {
        cargo build --release --locked --target x86_64-pc-windows-msvc --target-dir $targetDir
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }

    # WHY: all public artifacts live under bin/rust, with implementation detail
    # kept in the directory name rather than in the executable's internal CLI.
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null
    Copy-Item -LiteralPath $builtExe -Destination $artifact -Force

    # WHY: --version exits before binding a port or loading tessdata, so it is
    # the cheapest proof that Windows can load the executable at all.
    & $artifact --version
    if ($LASTEXITCODE -ne 0) {
        throw "$artifact --version failed with exit code $LASTEXITCODE"
    }

    Write-Host "Wrote $artifact"
} finally {
    foreach ($key in $saved.Keys) {
        if ($null -eq $saved[$key]) {
            Remove-Item "Env:\$key" -ErrorAction SilentlyContinue
        } else {
            Set-Item "Env:\$key" $saved[$key]
        }
    }
}
