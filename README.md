# img-scaling-benchmark

A three-way benchmark of one small image preprocessing pipeline:

```
PNG in -> grayscale (luma) -> global contrast stretch -> integer bilinear upscale -> 8-bit gray out
```

implemented three times — a naive Go version, an optimized Go version, and a Rust
version — and measured on the same fixtures, single-threaded, with per-phase timings
and a pixel-level correctness comparison.

## Why it exists

The naive Go implementation is the preprocessing stage in front of an OCR engine:
screenshots are converted to grayscale, contrast-stretched so anti-aliased text
separates cleanly from the background, and upscaled so small glyphs are large enough
to read. CPU profiling showed this stage, together with the PNG encode that follows
it, dominating the cost of the surrounding work.

That leaves an ambiguous question. The naive version makes several expensive choices
*within* Go — a per-pixel interface call to read each pixel, a `float64` intermediate
buffer, a compressing encoder for a file that is deleted seconds later. It is not
obvious how much of the cost is those choices and how much is Go itself. This repo
separates the two by holding the transformation fixed and varying only the
implementation.

## The three variants

**A — GO-ORIGINAL.** The unmodified naive implementation. Reads every pixel through
`image.Image.At(x, y)`, which returns a boxed `color.Color`; calls `RGBA()` on it to
get 16-bit channels and shifts them back down to 8 bits. Accumulates luma into a
`[]float64` while tracking global min/max, then contrast-stretches. Upscales with a
hand-rolled bilinear loop whose edge clamping goes through a closure-based `sample`
function, writing each output pixel with `SetGray`. Encodes the result as PNG with
`png.BestSpeed`.

**B — GO-OPTIMIZED.** Same transformation, different mechanics. Decodes once, then
reads pixels by indexing the image's `Pix` byte slice directly, behind a type switch
on `*image.RGBA`, `*image.NRGBA` and `*image.Gray`, with an `At()` fallback for any
other type. `*image.RGBA`'s `Pix` is alpha-premultiplied and `*image.NRGBA`'s is not,
so the NRGBA branch premultiplies explicitly to keep the luma values bit-identical to
the `At()` path. Upscales via `golang.org/x/image/draw`'s `BiLinear.Scale`. Writes
binary PGM (`P5`) instead of PNG.

**B2 — GO-OPTIMIZED with a hand-rolled scaler.** Identical to B except that
`BiLinear.Scale` is replaced with a hand-rolled `uint8` bilinear loop. This variant
was added after B measured *slower* than A on the fixture that actually scales:
`x/image/draw`'s `BiLinear` has generated fast paths only for an `*image.RGBA`
destination, and an `*image.Gray` destination silently falls back to a generic
interface path that carries four `float64` channels per pixel for a one-channel
image. B2 is the variant that represents optimized Go fairly.

**D — GO, PROFILE-OPTIMIZED.** B2 taken apart with `runtime/pprof`, which attributed
65.4% of all samples on the scaling fixture to the single hand-rolled bilinear loop.
Three changes came out of that. The `[]float64` luma buffer is gone: pass 1 keeps only
min/max and pass 2 recomputes the identical float64 expression, which is cheaper than
storing and reloading 8 bytes per pixel and leaves the arithmetic bit-for-bit
unchanged. The upscale factor is always an integer, so the per-output-column `x0`,
`x1` and interpolation weight have only `width*factor` distinct values and are
precomputed once per image instead of 15.9 million times. And the scaler is separable
(horizontal pass, then vertical) in 16.16 fixed point rather than a single-pass 4-tap
in `float64`. Byte-identical to A at factor 1, and bit-identical to B2 at factor 2.

**E — GO, D WITH A LUT STRETCH.** D plus exact integer luma
(`L = 299r + 587g + 114b`, which is exactly 1000x the float luma) and a 255 KB byte
table that replaces the whole per-pixel contrast stretch with one load. The fastest
variant measured, and the only new one that is *not* byte-identical to A at factor 1 —
it differs on 77 of 7.48 million pixels, every one of them an exact half-way rounding
tie. It is reported as an ablation, not the recommendation.

**C — RUST.** The same pipeline end to end. Decodes with the `png` crate; the
grayscale, contrast stretch and bilinear upscale are hand-written over flat slices;
output is binary PGM.

## Methodology

**Fixtures.** Two real captured web page screenshots, checked into `fixtures/`:
`typical.png` (1366 x 2915, 510,598 bytes) and `big.png` (1366 x 5477, 2,571,138
bytes). Both decode to `*image.RGBA` in Go.

**Upscale cap.** The upscale factor is the largest integer up to 3 whose output stays
within a 20,000,000-pixel budget, so that a tall page does not produce an image too
large for the downstream consumer to finish. `typical.png` runs at 2x; `big.png` at
7.48 MPix exceeds the budget at both 3x and 2x and therefore runs at 1x. The 1x case
is useful on its own: it isolates the grayscale and contrast-stretch stage with no
scaler in the path.

**Timing.** 15 timed iterations after 3 warmup iterations, per variant per fixture,
single-threaded. Input bytes are read into memory once, outside the loop; each
iteration re-decodes from that buffer. Each iteration is split into three phases:
PNG decode, transform (grayscale + stretch + upscale), and encode+write. Minimum,
median and mean are reported for each phase.

**Correctness.** Byte identity across variants is not expected — the scalers differ
legitimately in edge handling and in where they round to 8 bits. Instead, each output
is decoded back to a gray buffer and compared against variant A's, reporting the
percentage of differing pixels and the maximum absolute difference. Measured result:
all variants are byte-identical to A on the 1x fixture, and on the 2x fixture every
difference is exactly one gray level.

## Results

Median milliseconds per image. Full tables, per-phase minimums and means, output
sizes and the correctness breakdown are in [RESULTS.md](RESULTS.md).

| Fixture | Variant | Decode | Transform | Encode+write | Total |
|---------|---------|--------|-----------|--------------|-------|
| typical.png (2x) | A GO-ORIGINAL | 47.42 | 375.12 | 141.97 | **565.84** |
| typical.png (2x) | B GO-OPTIMIZED | 43.98 | 786.90 | 10.62 | **845.56** |
| typical.png (2x) | B2 GO-OPT, hand scaler | 43.23 | 209.92 | 11.07 | **263.13** |
| typical.png (2x) | C RUST | 13.51 | 231.09 | 8.83 | **253.58** |
| big.png (1x) | A GO-ORIGINAL | 145.92 | 364.37 | 111.58 | **622.61** |
| big.png (1x) | B GO-OPTIMIZED | 144.18 | 62.41 | 5.22 | **209.93** |
| big.png (1x) | B2 GO-OPT, hand scaler | 143.55 | 60.29 | 5.70 | **208.64** |
| big.png (1x) | C RUST | 47.13 | 63.07 | 4.48 | **114.83** |

Headline findings:

- Dropping the PNG encode saves 131.35 ms on `typical` (23.2% of A's total) and
  106.36 ms on `big` (17.1%), at the cost of an 18x larger transient file.
- Replacing `At()` with direct `Pix` indexing makes the transform 5.8x faster on the
  1x fixture (364.37 -> 62.41 ms), with byte-identical output.
- `x/image/draw`'s `BiLinear.Scale` into an `*image.Gray` destination is 2.1x
  *slower* than the naive hand-rolled loop it replaced.
- Rust does not beat optimized Go on the transform phase — it is 5-10% slower. Its
  only real margin is PNG decode, where the `png` crate is about 3x faster than Go's
  `image/png`.

### Part 2 — after profile-guided optimization

Re-measured in a single interleaved session (`bench_all.ps1 -Reps 5`), median of five
per-run medians, because run-to-run noise on this host reaches 5-15% on transform and
up to 40% on decode. Rust reproduced its Part 1 numbers in the same session.

| Fixture | Variant | Decode | Transform | Encode+write | Total |
|---------|---------|--------|-----------|--------------|-------|
| typical.png (2x) | B2 GO-OPT, hand scaler | 47.06 | 224.39 | 11.72 | **284.79** |
| typical.png (2x) | **D GO, profile-optimized** | 45.28 | **79.82** | 11.55 | **135.28** |
| typical.png (2x) | E GO, D + LUT stretch | 44.97 | **72.00** | 11.10 | **128.19** |
| typical.png (2x) | C RUST | 13.43 | 221.44 | 9.64 | **246.26** |
| big.png (1x) | B2 GO-OPT, hand scaler | 144.11 | 61.77 | 6.07 | **212.83** |
| big.png (1x) | **D GO, profile-optimized** | 150.31 | **58.05** | 6.11 | **214.10** |
| big.png (1x) | E GO, D + LUT stretch | 156.17 | **39.78** | 6.05 | **200.84** |
| big.png (1x) | C RUST | 46.96 | 62.75 | 4.85 | **114.25** |

- **Go now wins the transform decisively on both fixtures**: D is 2.77x faster than
  Rust on `typical` and at parity-to-1.58x on `big`. It beats Rust on *total* time on
  `typical` by 1.82x, but still loses `big` by 1.58x — and the whole of that deficit
  is PNG decode, which is 70% of Go's total there.
- The biggest single win was precomputing the per-column bilinear tables from the
  integer upscale factor: 2.18x on the transform, by itself.
- Making the scaler **separable** was worth another 22%, which the hypothesis had
  expected to lose to cache effects.
- 16.16 fixed point beat `float64` by 10.5% but was **indistinguishable from
  `float32`** — the win is in not using `float64`, not in using integers.
- Reslicing rows did **not** eliminate the hot-loop bounds checks (verified against
  `-d=ssa/check_bce/debug=1`); table-driven indices are unprovable to the compiler.
- Go's PNG decode is 58-65% **unfiltering**, only 35-42% inflate. The best pure-Go
  decoder found (`fastpng`, a `klauspost/compress` fork of `image/png`) is worth
  1.05-1.06x. The real prize is upstream: if the producer emitted an uncompressed
  format, decode would drop from 44.10 to 2.52 ms and from 136.11 to 5.62 ms — 94-96%.
- The transform is deliberately **not** parallelized; the real consumer already runs
  many images concurrently across all cores, so single-threaded per-image throughput
  is the correct metric.

## How to run it

Prerequisites: Go 1.24 or newer (measured on 1.27.0) and a Rust toolchain (measured
on cargo 1.93.1). The Go harness needs `golang.org/x/image` v0.25.0, fetched by
`go build`.

Build both harnesses and the comparison tool:

```sh
mkdir -p bin out
cd go && go build -o ../bin/bench.exe . && go build -o ../bin/compare.exe ./compare && cd ..
cd rust && cargo build --release && cd ..
```

Run the Go variants. The first argument is the variant, then the input PNG, then the
output path. `A`, `B` and `B2` are the Part 1 baselines; `D` and `E` are the optimized
variants; `D1`-`D4`, `F64` and `F32` are the ablations that attribute each individual
optimization (see RESULTS.md):

```sh
./bin/bench.exe A  fixtures/typical.png out/typical_A.png
./bin/bench.exe B  fixtures/typical.png out/typical_B.pgm
./bin/bench.exe B2 fixtures/typical.png out/typical_B2.pgm
./bin/bench.exe D  fixtures/typical.png out/typical_D.pgm
./bin/bench.exe E  fixtures/typical.png out/typical_E.pgm
```

Set `CPUPROFILE` to capture a CPU profile of the whole warm+timed loop:

```sh
CPUPROFILE=prof/b2.pprof ./bin/bench.exe B2 fixtures/typical.png out/typical_B2.pgm
go tool pprof -top bin/bench.exe prof/b2.pprof
```

Run the whole variant matrix with repeat-and-report (PowerShell), which is how the
Part 2 numbers were produced:

```powershell
./bench_all.ps1 -Reps 5
```

Attribute the PNG decode phase (inflate vs unfiltering, alternative decoders, and the
uncompressed-input floor):

```sh
./bin/decodeprobe.exe fixtures/big.png
```

Run the Rust variant:

```sh
./rust/target/release/rsbench.exe fixtures/typical.png out/typical_C.pgm
```

Each run prints one JSON line with the phase statistics, the chosen upscale factor,
the output dimensions and the output size.

Compare any output against the baseline. The tool accepts `.png` and `.pgm` on either
side and reports differing-pixel count, percentage and maximum absolute delta:

```sh
./bin/compare.exe out/typical_A.png out/typical_B2.pgm
```

## Repo layout

```
go/          Go harness: main.go holds variants A, B, B2; fast.go holds D, E and
             the ablations; plus go.mod
go/compare/  Output comparison tool (PNG or PGM in, pixel diff out)
go/decodeprobe/  PNG decode attribution: inflate vs unfilter, fastpng and
             klauspost/compress comparison, uncompressed-input floor
go/probe/    One-off pixel probe proving the LUT stretch's divergence from A is
             entirely exact half-way rounding ties
bench_all.ps1  Repeat-and-report driver for the whole variant matrix
rust/        Rust crate: variant C
fixtures/    Input screenshots
out/         Benchmark output (git-ignored)
bin/         Built Go binaries (git-ignored)
RESULTS.md   Full measurements and the verdict
```

## License

MIT. See [LICENSE](LICENSE).
