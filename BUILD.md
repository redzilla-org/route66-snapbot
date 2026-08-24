# Build

This repository ships `ocrd-rust` as the release implementation. The Go daemon
is kept buildable as a protocol cross-check, but release assets should come from
the Rust tree unless the owner explicitly changes that policy.

## Artifacts

Release artifacts live under `bin/rust/`:

| Artifact | Meaning |
|---|---|
| `ocrd-windows-amd64-static.exe` | Windows/amd64 Rust daemon with Tesseract, Leptonica, their native dependencies, and the MSVC CRT linked statically. |
| `ocrd-linux-amd64-static` | Linux/amd64 Rust daemon with Tesseract, Leptonica, their native dependencies, and musl libc linked statically. |
| `eng.traineddata` | English Tesseract language data. This is runtime data and is never part of the executable, even for static builds. |

`eng.traineddata`, not `eng.trainingdata`, is the filename Tesseract expects.
Installers should place it in a directory and set `TESSDATA_PREFIX` to that
directory before OCR requests use `lang: "eng"`.

## Windows Static

Build:

```powershell
make windows-rust-static
```

Direct script entry point:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build-windows-rust-static.ps1
```

What the script does:

1. Finds vcpkg from `VCPKG_ROOT`, or falls back to `C:\Users\alexr\vcpkg`.
2. Installs `tesseract:x64-windows-static` if it is missing.
3. Sets `VCPKGRS_TRIPLET=x64-windows-static`.
4. Removes `VCPKGRS_DYNAMIC`, because that variable forces DLL linkage.
5. Adds Rust `RUSTFLAGS=-C target-feature=+crt-static`.
6. Adds Win32 import libraries needed by the static vcpkg closure:
   `Advapi32.lib`, `Crypt32.lib`, `Xmllite.lib`, `User32.lib`, `Bcrypt.lib`,
   `Iphlpapi.lib`, and `Secur32.lib`.
7. Builds in `ocrd-rust\target-static\` so dynamic and static build-script outputs cannot mix.
8. Copies the executable to `bin\rust\ocrd-windows-amd64-static.exe`.
9. Runs `--version`, which proves the executable loads without contacting a NATS server or loading tessdata.

The first static Windows build is slow because vcpkg must build the whole static
Tesseract dependency graph. Later runs reuse vcpkg's installed triplet and binary
cache.

## Windows Dynamic

Build:

```powershell
make windows-rust
```

This target intentionally remains available for local comparison. It sets
`VCPKGRS_DYNAMIC=1`, uses vcpkg's `x64-windows` triplet, and writes
`bin/rust/ocrd-windows-amd64.exe`. Running that executable requires the vcpkg
`installed\x64-windows\bin` directory on `PATH` so Tesseract/Leptonica DLLs can
load.

Do not publish the dynamic Windows artifact when the static target is available.

## Linux Dynamic

Build:

```sh
make linux-rust
```

The Linux build runs inside `ocrd-rust/Dockerfile.build`, which starts from the
same pinned Alpine/musl base used by the consuming worker image. This keeps the
build userland and runtime userland aligned for the system Tesseract and
Leptonica libraries.

## Linux Static

Build:

```sh
make linux-rust-static
```

The static Linux build uses `ocrd-rust/Dockerfile.static-linux`. Alpine ships
dynamic Tesseract and Leptonica libraries, but not `libtesseract.a` or
`libleptonica.a`, so the Dockerfile builds those two native libraries from
source with shared libraries disabled. It then runs Cargo with:

```sh
PKG_CONFIG_ALL_STATIC=1
cargo rustc --bin ocrd -- \
  -C linker=/usr/local/bin/ocrd-static-cxx-link \
  -C target-feature=+crt-static \
  -C relocation-model=static \
  -C link-arg=-no-pie
```

`ocrd-static-cxx-link` wraps `g++` with static C++ runtime flags and restores
`-Wl,-Bstatic` at the end of Cargo's link line. That is necessary because the
Tesseract Rust binding emits Linux native libraries through build scripts, and
Cargo's generated link command otherwise leaves the driver in dynamic-library
mode before `g++` appends `libstdc++`.

The static flags are passed through `cargo rustc -- ...` rather than
`RUSTFLAGS`; on Alpine the host and target triple are the same, and global
static flags make bindgen's build scripts unable to dynamically load
`libclang`.

The static target writes `bin/rust/ocrd-linux-amd64-static`.

The important checks are:

```sh
file bin/rust/ocrd-linux-amd64-static
ldd bin/rust/ocrd-linux-amd64-static
```

For a true static Linux binary, `file` should report static linkage and `ldd`
should report that the binary is not dynamically linked. Treat a successful
Cargo build as insufficient until those checks pass.

## Language Data

Stage English language data:

```powershell
make rust-tessdata
```

Or provide the source explicitly:

```powershell
make rust-tessdata ENG_TRAINEDDATA_SOURCE="C:\path\to\eng.traineddata"
```

The staging script searches in this order:

1. `ENG_TRAINEDDATA_SOURCE`, passed through Make to `-SourcePath`.
2. `$env:TESSDATA_PREFIX\eng.traineddata`.
3. `C:\Program Files\Tesseract-OCR\tessdata\eng.traineddata`.
4. `C:\Program Files (x86)\Tesseract-OCR\tessdata\eng.traineddata`.
5. `C:\ProgramData\Tesseract-OCR\tessdata\eng.traineddata`.

It writes `bin/rust/eng.traineddata`. The release process uploads this file as a
separate asset because static linking covers code libraries, not Tesseract model
data.

## Publish

Publish:

```sh
make publish VERSION=v0.1.0 REPO=redzilla-org/ocr-daemon
```

The publish target creates the GitHub release if needed, copies the static build
outputs into `bin/publish/` under their **public** names, and uploads from there
with `--clobber`:

| Build output | Published asset |
|---|---|
| `bin/rust/ocrd-windows-amd64-static.exe` | `ocrd-windows-amd64.exe` |
| `bin/rust/ocrd-linux-amd64-static` | `ocrd-linux-amd64` |
| `bin/rust/eng.traineddata` | `eng.traineddata` |

**The published names carry no build detail, and the rename is why the staging
directory exists.** `gh release upload` names each asset by its file basename,
so those filenames are the download URLs every consumer hardcodes. The artifact
name says platform and nothing else: no language (see the Makefile preamble) and
no linkage. Whether a release was linked statically is this repo's business, the
same as which language it was written in, and baking `-static` into the URL
would force a breaking rename on every consumer the next time the linkage
changes.

`bin/rust/` keeps the `-static` suffixes because there the distinction is real —
it holds both linkages at once, and `bin/rust/ocrd-windows-amd64.exe` is the
DYNAMIC build, one character away from the public name. Uploading that by
mistake would produce a release that runs only on a machine with vcpkg's DLLs on
`PATH` and fails at rc=127 everywhere else, so `publish` never uploads from
`bin/rust/` directly and rebuilds `bin/publish/` from scratch each run.

`publish` deliberately does not depend on `all`; it should not block on the Go
implementation, and it should not upload Go artifacts by accident.

## Verification

Cheap executable-load checks:

```powershell
bin\rust\ocrd-windows-amd64-static.exe --version
```

```sh
bin/rust/ocrd-linux-amd64-static --version
```

These commands exit before connecting to NATS or loading any language data.
They prove loader compatibility only. An OCR smoke test still needs a reachable
NATS server (with `max_payload` raised past the 1 MB default) and
`TESSDATA_PREFIX` pointing at the directory containing `eng.traineddata`.
