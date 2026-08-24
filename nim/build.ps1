# Generate C with Nim's standard backend, then cross-compile one Windows object.
# Both toolchains run in a public-ECR image; no host installation is required.
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
apt-get install -y -qq ca-certificates curl nim xz-utils >/dev/null
curl --fail --location --silent --show-error \
  https://ziglang.org/download/0.14.1/zig-x86_64-linux-0.14.1.tar.xz \
  -o /tmp/zig.tar.xz
tar -C /tmp -xf /tmp/zig.tar.xz
nim c --compileOnly --nimcache:/tmp/nimcache --os:windows --cpu:amd64 \
  --cc:clang --opt:speed -d:danger --mm:none --noMain:on nim/transform.nim
generated=$(find /tmp/nimcache -name '*transform.nim.c' -print -quit)
test -n "$generated"
/tmp/zig-x86_64-linux-0.14.1/zig cc -target x86_64-windows-gnu \
  -O3 -march=native -I/tmp/nimcache -I/usr/lib/nim/lib \
  -c "$generated" -o nim/bin/transform.obj
/tmp/zig-x86_64-linux-0.14.1/zig cc -target x86_64-windows-msvc \
  -O3 -c nim/runtime_stubs.c -o nim/bin/runtime_stubs.obj
'@
if ($LASTEXITCODE -ne 0) {
  throw "Nim container build failed with exit code $LASTEXITCODE"
}
Get-Item (Join-Path $PSScriptRoot "bin\transform.obj")
Get-Item (Join-Path $PSScriptRoot "bin\runtime_stubs.obj")
