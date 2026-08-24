# Build-time considerations

Runtime latency is not the only axis that picks a winner. This page records
measured build costs for the leading contenders on the reference workstation
(80-core Windows 11, warm package registries unless noted). Every number is a
real measured wall time from 2026-08-23, not an estimate.

## Measured build times

| Build | Cold cache | Incremental (touch one file) | Notes |
|---|---:|---:|---|
| Go GOFAST benchmark binary | 27.2 s | 2.6 s | pure Go + Plan 9 asm; forced full rebuild with warm dep cache: 22.1 s |
| Go OCR daemon (GOFAST pipeline + cgo tesseract capi) | 41.1 s | 1.6–1.8 s | cgo against vcpkg tesseract55.lib via mingw-w64 gcc |
| Rust OCR daemon (KSTREAM, fat LTO, codegen-units=1) | 39.7 s | 9.9–13.5 s | cold = fresh target/, warm crates registry; three measured warm rebuilds: 9.85 s, 11.46 s, 13.51 s |

"Cold cache" means an empty GOCACHE (Go) or a deleted `target/` directory
(Rust) with dependencies already downloaded. First-ever builds add network
fetch time to both, roughly equally.

## The numbers that are NOT in the table, and dominate anyway

- **vcpkg bootstrap (Windows, one-time): ~52 minutes.** Building
  tesseract + leptonica + the image-codec chain from source is the actual
  "Rust took forever" experience — but it is a *toolchain* cost, not a Rust
  cost. A Go OCR daemon binding the same libraries needs the identical vcpkg
  tree (cgo links the same `tesseract55.lib`), so this cost is
  language-independent and paid once per machine.
- **PGO adds a full profile-collect + rebuild cycle** to every Rust release
  build, and measured end-to-end it is a wash at runtime (29.17 ms vs
  28.43 ms non-PGO on `typical`). Not worth its build cost.
- **GO127 requires an experimental toolchain**: Go 1.27 with
  `GOEXPERIMENT=simd`. That is a per-machine toolchain pin, and the variant
  is 10 ms slower than GOFAST at runtime anyway (the gap is codegen quality
  on thin streaming loops — see below), so it buys nothing today.

## Edit-loop consequence

The day-to-day cost is the incremental column: **~2 s (Go) vs ~10–14 s
(Rust)**. The Rust penalty is self-inflicted by the release profile —
`lto = "fat"` + `codegen-units = 1` re-runs whole-program LTO on every touch.
A dev profile or thin-LTO would cut it, at some runtime cost that has not
been measured here.

Cold builds are effectively a tie (27–41 s both languages), which surprised
us: the folk memory of "the Rust build takes forever" was the one-time vcpkg
bootstrap plus the first crates.io dependency compile, not the crate itself.

## CI / Docker-stage angle

- The Rust daemon builds in its own Docker stage on Alpine
  (`apk add rust cargo musl-dev clang… tesseract-ocr-dev`), layer-cached on
  the crate's file hashes — the multi-minute apk+cargo cost recurs only when
  the crate changes.
- A Go daemon in the same image would reuse the already-present Go toolchain
  (the base image IS golang:alpine) and skip the ~300 MB rust/cargo apk
  layer entirely; cgo needs only gcc/musl-dev + the same tesseract-ocr-dev
  headers.

## Binary size

| | size |
|---|---:|
| Rust ocrd.exe (dynamic, vcpkg DLLs on PATH) | 403 KB |
| Go ocrd-go.exe (dynamic, same DLLs) | 5.5 MB |

Both link the same dynamic tesseract/leptonica chain (~11 MB of DLLs) and
need `TESSDATA_PREFIX`; neither size matters in a container image.

## Why GOFAST's assembly survives the Go 1.27 SIMD API (runtime aside)

Phase timings show decode — PNG filters, fused luma — is a tie between the
hand-assembly and API-SIMD variants. The entire GO127 gap (transform
0.52 ms → 10.22 ms on `typical`) is in the thin streaming upscale kernels,
led by `verticalRows2x`: the experimental compiler does not yet keep
`archsimd` values in registers across statements, so each vector op
round-trips through memory and each load is a visible call in the profile.
Heavy dependency-bound loops amortize that overhead; one-instruction
streaming loops are nothing but that overhead. Expect this gap to shrink
with toolchain maturity, not with user-code changes.
