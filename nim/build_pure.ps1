# Cross-compile the pure-Nim pipeline with Nim's normal C backend and MinGW.
# Everything runs in the requested public-ECR image; the host needs only Docker.
$ErrorActionPreference = "Stop"
$repo = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$drive = $repo.Substring(0, 1).ToLowerInvariant()
$mount = "/mnt/$drive" + $repo.Substring(2).Replace('\', '/')

New-Item -ItemType Directory -Force -Path (Join-Path $PSScriptRoot "bin") | Out-Null
docker run --rm --mount "type=bind,source=$mount,target=/work" `
  --workdir /work public.ecr.aws/docker/library/ubuntu:24.04 `
  bash -lc @'
set -euo pipefail
apt-get update -qq
apt-get install -y -qq gcc-mingw-w64-x86-64 nim >/dev/null
nim c --os:windows --cpu:amd64 --cc:gcc --mm:arc -d:danger --opt:speed \
  --path:nim --gcc.exe:x86_64-w64-mingw32-gcc --gcc.linkerexe:x86_64-w64-mingw32-gcc \
  --passC:-O3 --passC:-march=x86-64-v3 --passL:-s \
  --out:nim/bin/nimbench.exe nim/bench_nim.nim
'@
if ($LASTEXITCODE -ne 0) { throw "pure Nim build failed with exit code $LASTEXITCODE" }
Get-Item (Join-Path $PSScriptRoot "bin\nimbench.exe")
