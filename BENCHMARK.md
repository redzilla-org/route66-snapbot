# img-scaling-benchmark

A live benchmark for one complete, single-threaded pipeline:

```text
PNG bytes -> decode -> grayscale -> global contrast stretch -> integer bilinear scale -> PGM file
```

The production objective weights the `typical` fixture at 80% and the `big`
fixture at 20%. The headline metric is the fastest valid observed end-to-end
total across repeated sessions. Median timings are retained only as diagnostics;
they do not determine rank or recommendations.

## Current winner

Pure Rust `KSTREAM`, using `png` 0.18.1 and fdeflate without PGO, is the fastest
eligible pipeline.

| pipeline | `typical.png`, 2x | `big.png`, 1x | 80/20 score |
|---|---:|---:|---:|
| **Rust `png`/fdeflate + KSTREAM** | 28.43 ms | **54.11 ms** | **33.57 ms** |
| Rust `png`/fdeflate + KSTREAM, PGO | 29.17 ms | 55.18 ms | 34.37 ms |
| Go custom PNG + GOFAST | **27.59 ms** | 69.99 ms | 36.07 ms |
| Go 1.27 custom PNG + API SIMD | 38.32 ms | 79.38 ms | 46.53 ms |
| Wuffs decode + C++ KLUT | 50.24 ms | 74.64 ms | 55.12 ms |
| Zig `zignal` + KLUT | 84.08 ms | 148.74 ms | 97.01 ms |
| Nim `nimPNG` + KLUT | 90.08 ms | 228.21 ms | 117.70 ms |
| Python Pillow | 154.75 ms | 115.07 ms | 146.81 ms, ineligible |

The non-pure Rust-decode/C++-transform `HYBRID` measures 41.50 ms on `typical`,
63.21 ms on `big`, and 45.84 ms weighted. It does not beat pure Rust.

`KSTREAM` configures `png` 0.18.1/fdeflate for trusted browser-produced input,
skipping checksum validation and metadata processing. It converts decoded pixels
once into a retained 8.8 fixed-point luma plane while collecting global minimum
and maximum. Factor-2 scaling streams through two luma rows; factor-1 output is
materialized directly. This is the recommended implementation.

See [RESULTS.md](RESULTS.md) for the complete ranking and quality results.
See [BUILD_TIMES.md](BUILD_TIMES.md) for measured build-time considerations
(cold/incremental compiles, toolchain bootstrap, CI stage caching, binary size).

## Quality gate

Eligible output may differ by at most one gray level on at most 1% of pixels.

Rust native, Rust PGO, Go `GOFAST`, and Go 1.27 `GO127` are byte-identical on
both fixtures. Zig, Nim, Wuffs/C++, and `HYBRID` differ from that output on
44,641 of 15,927,560 `typical` pixels (0.2803%) and 24,579 of 7,481,582 `big`
pixels (0.3285%); every difference is one gray level, so all remain eligible.

Pillow differs on 244,195 `typical` pixels (1.5332%, maximum delta 2) and 24,669
`big` pixels (0.3297%, maximum delta 1). It fails the quality gate and cannot win.

## Input contract

The optimized pipelines target trusted, local Playwright screenshots. Checksum
validation and metadata processing are excluded where the decoder permits it.
Unsupported PNG encodings and structurally invalid payloads hard-fail; checksum-only
corruption need not be detected. These are benchmark and production-workload
constraints, not general-purpose PNG decoder guarantees.

## Method

- Measurements run on a dual-socket Windows 11 x86-64 host with two
  `Intel(R) Xeon(R) Gold 6138 CPU @ 2.00GHz` packages: 40 physical cores and 80
  logical processors total. Each benchmark process is single-threaded.
- `typical.png` is 1366x2915 and scales 2x. `big.png` is 1366x5477 and remains
  1x under the 20-million-output-pixel budget.
- Every process reads the PNG bytes once, performs 3 warmups, then runs complete
  PNG-to-PGM iterations for a 250 ms timed window.
- Each process exposes `total.min`, its fastest complete timed iteration.
- A session runs every contender in shuffled order five times.
- The headline for each fixture is the fastest valid `total.min` observed across
  repeated sessions. Medians and paired signs diagnose noise only.
- The production score is `0.8 * typical + 0.2 * big`.
- `prof/paired_lang.py` compares every output pixel after timing; an invalid output
  is ineligible regardless of speed.

The retained session data and its aggregate are in `results/`. In particular,
`results/fastest_summary.json` derives the headline from two five-run sessions per
fixture. The separate hybrid sessions are retained in the same directory.

## Dependencies

The non-Cargo language decoders are pinned as submodules:

- Zig `zignal`: `ac83881046a5e672d926e22306a0c82b369b9198`.
- Nim `nimPNG`: `8f8e774be15218919235a044cd2ece13728e637d`.
- Wuffs v0.3.5: `d1a6f2c3de7e52b8775782028e5a49946a7f9184`.

Zig and Nim are cross-compiled to Windows executables in
`public.ecr.aws/docker/library/ubuntu:24.04`. The pure Zig build pins the exact
Zig 0.17 development compiler required by the pinned `zignal` revision.

## Build

Initialize the pinned decoder sources before building their contenders:

```powershell
git submodule update --init --recursive
```

Build the custom Go and independent Go 1.27 pipelines with their measured CPU
settings:

```powershell
Push-Location go
$env:GOAMD64 = "v3"
go build -pgo=off -o ..\bin\bench_fastest_v3.exe .
Pop-Location

go install golang.org/dl/go1.27.0@latest
go1.27.0 download
.\go1.27\build.ps1
```

Build native Rust, then train the separate PGO contestant on the declared 80/20
fixture mix:

```powershell
$env:RUSTFLAGS = "-C target-cpu=native"
cargo build --release --manifest-path rust\Cargo.toml
Copy-Item rust\target\release\rsbench.exe rust\bin\rsbench_nativefat.exe

rustup component add llvm-tools-preview
.\rust\build_pgo.ps1 -ProfileVariant KSTREAM
```

Build the remaining full-field contenders:

```powershell
.\zig\build_pure.ps1
.\nim\build_pure.ps1
.\wuffs\build.ps1
```

## Run

Run the winning native and PGO Rust binaries with `KSTREAM`:

```powershell
.\rust\bin\rsbench_nativefat.exe KSTREAM fixtures\typical.png out\rust.pgm
.\rust\bin\rsbench_kstreampgo.exe KSTREAM fixtures\typical.png out\rust-pgo.pgm
```

Run the current eight-contestant field in five shuffled outer runs per fixture:

```powershell
python prof\paired_lang.py typical 5 rs:nativefat:KSTREAM `
  rs:kstreampgo:KSTREAM go:fastest_v3:GOFAST go127:GO127 `
  wf:KLUT zig:KLUT nim:KLUT py:PIL

python prof\paired_lang.py big 5 rs:nativefat:KSTREAM `
  rs:kstreampgo:KSTREAM go:fastest_v3:GOFAST go127:GO127 `
  wf:KLUT zig:KLUT nim:KLUT py:PIL
```

Run the hybrid separately so phase ownership remains explicit:

```powershell
python prof\paired_lang.py typical 5 rs:nativefat:KSTREAM rs:nativefat:HYBRID
python prof\paired_lang.py big 5 rs:nativefat:KSTREAM rs:nativefat:HYBRID
```

## Playwright constraint

Playwright screenshots are encoded PNG, JPEG, or WebP; returning a buffer does
not expose a raw framebuffer. Chromium CDP screenshot and screencast APIs are also
compressed. Avoiding image encoding requires a lower-level Chromium capture path.

- [Playwright screenshot API](https://playwright.dev/docs/screenshots)
- [Chromium CDP Page domain](https://chromedevtools.github.io/devtools-protocol/tot/Page/)

## Layout

```text
go/                       Custom Go PNG and SIMD pipeline
go1.27/                   Independent unsafe Go + Go 1.27 SIMD-API pipeline
rust/                     Pure Rust KSTREAM and explicit hybrid kernels
zig/bench_zig.zig         Pure Zig pipeline
zig/zignal/               Pinned pure-Zig PNG decoder
nim/bench_nim.nim         Pure Nim pipeline
nim/nimpng/               Pinned pure-Nim PNG decoder
wuffs/                    Pinned Wuffs decoder and C++ harness
prof/paired_lang.py       Timing and pixel-correctness driver
```

Benchmark code is MIT licensed. Submodules retain their upstream licenses.
