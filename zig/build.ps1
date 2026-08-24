# Build a freestanding Windows object without installing Zig on the host.
# The public-ECR base and Zig release are pinned for reproducible code generation.
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
curl --fail --location --silent --show-error \
  https://ziglang.org/download/0.14.1/zig-x86_64-linux-0.14.1.tar.xz \
  -o /tmp/zig.tar.xz
tar -C /tmp -xf /tmp/zig.tar.xz
/tmp/zig-x86_64-linux-0.14.1/zig build-obj zig/transform.zig \
  -target x86_64-windows-msvc -O ReleaseFast -mcpu=native \
  -femit-bin=zig/bin/transform.obj
'@
if ($LASTEXITCODE -ne 0) {
  throw "Zig container build failed with exit code $LASTEXITCODE"
}
Get-Item (Join-Path $PSScriptRoot "bin\transform.obj")
