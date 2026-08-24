# Cross-compile the pure-Zig pipeline in the requested public-ECR container.
# The compiler version exactly matches the minimum pinned by the zignal submodule.
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
apt-get install -y -qq ca-certificates curl xz-utils >/dev/null
version='0.17.0-dev.1564+97ced1272'
curl --fail --location --silent --show-error \
  "https://ziglang.org/builds/zig-x86_64-linux-${version}.tar.xz" -o /tmp/zig.tar.xz
tar -C /tmp -xf /tmp/zig.tar.xz
ZIG_GLOBAL_CACHE_DIR=/tmp/zig-global \
"/tmp/zig-x86_64-linux-${version}/zig" build --build-file zig/build.zig \
  --cache-dir /tmp/zig-cache \
  --prefix zig/out -Dtarget=x86_64-windows-gnu -Dcpu=native -Doptimize=ReleaseFast
cp zig/out/bin/zigbench.exe zig/bin/zigbench.exe
'@
if ($LASTEXITCODE -ne 0) { throw "pure Zig build failed with exit code $LASTEXITCODE" }
Get-Item (Join-Path $PSScriptRoot "bin\zigbench.exe")
