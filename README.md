# img-scaling-benchmark

A cross-language benchmark of one small image preprocessing pipeline:

```
PNG in -> grayscale (luma) -> global contrast stretch -> integer bilinear upscale -> 8-bit gray out
```

implemented many times over in Go and in Rust — from a naive baseline through
successive profile-guided optimizations on both sides — and measured on the same
fixtures, single-threaded, with per-phase timings and a pixel-level correctness
comparison.

The final cross-language comparison is **paired and interleaved**: the two recommended
binaries run back to back inside each pair, order shuffled, in a single session, and
the reported statistic is the median of per-pair ratios with a sign count beside it.
An earlier revision of this document compared optimized Go against unoptimized Rust
across two sessions and reported the difference as a language result; that claim is
retracted below.

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

## The variants

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

**K — GO, THE RECOMMENDATION.** D with the factor-2 case specialized. The upscale
factor is capped at 3 by the pixel budget and 1 is a no-op, so in practice it is 2 —
and at factor 2 the bilinear weights are exactly 1/4 and 3/4, which collapses D's whole
16.16 fixed-point apparatus into shift-and-add on 16-bit data (`3a+b` and `a+3b`
horizontally, `(3*M0 + M1 + 8) >> 4` vertically). Two 32-bit and two 64-bit multiplies
per pixel disappear and the intermediate halves from `uint32` to `uint16`. It is
1.38x faster than D on the transform and **bit-identical to D by construction**, since
1/4 and 3/4 are exact in binary. Other factors fall back to D's general scaler.
Recommended build: `GOAMD64=v1`, `-pgo=off`.

**G, H, I, J — GO ABLATIONS.** G removes the horizontal gather (bit-identical, did not
pay); H makes the vertical pass 32-bit through an 8.8 intermediate (did not pay, and
not bit-exact); I pins the luma expression against FMA contraction, which is the
control that proves `GOAMD64=v3`'s win is the FMA; J strips all arithmetic out of the
scaler's inner loops to measure the load/store ceiling, and its output is deliberately
wrong.

**C — RUST, NAIVE REFERENCE.** The same pipeline end to end, ported faithfully from
Go's naive variant A: an `f64` luma side-buffer and a fused four-tap `f64` bilinear
kernel. Decodes with the `png` crate; grayscale, contrast stretch and upscale are
hand-written over flat slices; output is binary PGM. **C is a correctness reference,
not an optimized implementation**, and comparing it against optimized Go is the error
this round retracts.

**DB — RUST, THE RECOMMENDATION.** Go's axis-table, separable and fixed-point
optimizations ported into Rust, while **keeping** the luma side-buffer, which in Rust
is faster than dropping it (see the results). Recommended build:
`RUSTFLAGS="-C target-cpu=native"` with `lto = "fat"` and `panic = "abort"`.

**D, DS, D1, D2, D3, F32, F64 — RUST ABLATIONS.** D is DB without the luma buffer; DS
adds a hand-written AVX2 vertical pass; D1/D2/D3 isolate one optimization each; F32 and
F64 swap the scaler's arithmetic type.

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

**Pairing, for anything compared across binaries.** A single invocation's median is not
a comparable number on this host: the same binary running the same variant on the same
fixture moved 82.8 -> 88.9 ms on transform between two sessions, a 7.4% drift that is
larger than several of the effects under test. Every cross-binary comparison in the
current results therefore uses `prof/paired_xlang.py`, which runs the contestants back
to back inside one pair, shuffles their order per pair, and repeats for 11 pairs per
fixture. The reported statistic is the **median of per-pair ratios** — drift slower
than one pair cancels — reported with a **sign count**, the number of pairs in which
the direction held. A sign count near half the pairs means the contestants traded
places from pair to pair; that is a tie, and it is reported as a tie regardless of
where the median ratio landed.

**Correctness.** Byte identity across variants is not expected — the scalers differ
legitimately in edge handling and in where they round to 8 bits. Instead, each output
is decoded back to a gray buffer and compared against variant A's, reporting the
percentage of differing pixels and the maximum absolute difference. Measured result:
all recommended variants in both languages are byte-identical to A on the 1x fixture
and byte-identical to each other on both fixtures, and on the 2x fixture every
difference from A is exactly one gray level.

## Results

> **RETRACTION, TWICE.** An earlier revision claimed optimized Go's transform is
> **2.77x faster than Rust**. Withdrawn: it compared four-times-optimized Go against a
> faithful port of Go's *naive* code, in two different measurement sessions on a host
> whose same-binary drift reaches 7.4%. The correction below then repeated the same
> error inverted — Go variant K carried the factor-2 exact-weight collapse and the Rust
> contestant did not — and reported **Go by 1.51x** on transform. Also withdrawn.
> With variant K implemented on both sides and byte-identical output, **Rust wins the
> transform by 1.25x on `typical.png` and ties on `big.png`**. See
> [Part 4 in RESULTS.md](RESULTS.md#part-4-variant-k-on-both-sides-and-python-as-a-third-contestant)
> for the K-parity rematch and the Python contestant; the tables in this section are
> the K-vs-DB round and are kept because their decode, flag, PGO and SIMD findings are
> unaffected by the transform correction.

### The corrected head-to-head

Best Go (variant K, `GOAMD64=v1`, no PGO) against best Rust (variant DB,
`-C target-cpu=native`, `lto = "fat"`, `panic = "abort"`), **measured in one
interleaved session**: the two binaries run back to back inside each pair, with the
order shuffled per pair, 11 pairs per fixture. The statistic is the **median of
per-pair ratios**, which cancels machine drift, reported next to a **sign count** — the
number of pairs in which the direction held. A sign count near half the pairs is a tie,
however far the median sits from 1.000, and is reported as one.

Absolute medians of the 11 per-pair medians, milliseconds:

| Fixture | Config | Decode | Transform | Encode+write | Total |
|---------|--------|--------|-----------|--------------|-------|
| typical.png (2x) | **Go K (v1)** | 46.77 | **59.45** | 14.07 | 120.06 |
| typical.png (2x) | **Rust DB (native)** | **17.05** | 87.69 | **10.41** | **115.66** |
| typical.png (2x) | Rust DB (baseline flags) | 16.58 | 100.91 | 10.80 | 127.49 |
| typical.png (2x) | Go K (v3) | 47.29 | 57.71 | 14.31 | 119.49 |
| big.png (1x) | **Go K (v1)** | 144.31 | **57.51** | 6.56 | 208.84 |
| big.png (1x) | **Rust DB (native)** | **53.94** | 72.32 | **5.42** | **131.77** |
| big.png (1x) | Rust DB (baseline flags) | 52.67 | 80.20 | 5.29 | 136.79 |
| big.png (1x) | Go K (v3) | 142.77 | 54.18 | 6.71 | 203.09 |

Go K over Rust DB, per pair (a ratio above 1.000 means Go is slower):

| Fixture | Phase | Median ratio | Sign count | Verdict |
|---------|-------|--------------|-----------|---------|
| typical | decode | 2.752 | 11/11 | **Rust 2.75x faster** |
| typical | transform | 0.662 | 0/11 | **Go 1.51x faster** |
| typical | encode+write | 1.353 | 11/11 | Rust 1.35x faster |
| typical | **total** | 1.031 | 8/11 | **tie**, leaning Rust |
| big | decode | 2.640 | 11/11 | **Rust 2.64x faster** |
| big | transform | 0.793 | 1/11 | **Go 1.26x faster** |
| big | encode+write | 1.313 | 9/11 | Rust 1.31x faster |
| big | **total** | 1.582 | 11/11 | **Rust 1.58x faster** |

Headline findings:

- **Optimized Rust wins the transform phase**, by 1.25x on `typical` (7/7 pairs) and
  to a tie on `big` (2/7 pairs), once variant K is implemented in both languages. The
  Go lead shown in the table below is the missing K and almost nothing else.
- **Optimized Rust wins PNG decode by 2.6-2.8x**, in 11 of 11 pairs on both fixtures.
  That is a decoder-quality gap between the `png` crate and Go's `image/png`, not a
  language gap, and it is unaffected by `-C target-cpu=native`.
- **End to end Rust wins both fixtures**, 1.67x on `typical` and 1.80x on `big`, 7/7
  pairs, with K on both sides. The whole of it is decode: Go spends 144 ms of its
  209 ms `big.png` total there.
- **Python is not the loser you would expect.** Pillow beats fully-optimized Go end to
  end on `big.png` by 1.41x, and beats Go's decode on both fixtures.
- **Rust's lead is not AVX-512.** At baseline flags — same source, same LTO and
  panic settings, no `-C target-cpu=native` — Rust still wins decode by 2.7-2.8x and
  still wins `big.png` end to end by 1.54x. The native-CPU flag buys Rust 10-13% on the
  transform, which *narrows* Go's transform lead rather than creating Rust's.
- **`GOAMD64=v3` is a labelled data point, not a recommendation.** It is worth 2-4% on
  transform and nothing measurable on total, and it silently changes 80 of 7,481,582
  pixels on `big.png` (max delta 1). The cause is pinned: v3 contracts the luma
  expression into two `VFMADD231SD`, which rounds once instead of twice. That is a
  product decision about reproducibility, not a performance decision.
- **PGO is worth nothing here.** Building with a profile of this workload produces
  exactly two PGO devirtualizations in the whole program, both in `compress/flate`, and
  none in the pipeline's own code — because the optimization that made variant D fast
  had already deleted the interface calls PGO would have devirtualized.
- **The dominant lever is not the language.** It is the PNG decode, and above that, the
  decision to hand this pipeline a PNG at all. If the producer emitted an uncompressed
  format, Go's decode would fall from 44.10 to 2.52 ms and from 136.11 to 5.62 ms
  (94-96%) — more than every code optimization in this repo combined, in either
  language.

### Correctness

Go K and Rust DB are **byte-identical to each other on both fixtures** — identical
MD5, not merely a zero pixel diff — and both are byte-identical to the naive Go
reference on the factor-1 fixture:

| Comparison | Fixture | Differing pixels | Max delta |
|------------|---------|------------------|-----------|
| Go K vs Rust DB | typical | **0** / 15,927,560 | **0** |
| Go K vs Rust DB | big | **0** / 7,481,582 | **0** |
| Go K vs naive A | typical | 94,985 (0.5964%) | 1 |
| Go K vs naive A | big | **0** | **0** |
| Rust DB vs naive A | typical | 94,985 (0.5964%) | 1 |
| Rust DB vs naive A | big | **0** | **0** |

The factor-1 agreement pins the decode, luma, min/max and contrast-stretch math as
exactly equivalent across both languages, including the alpha premultiplication that
Go's `At()` performs implicitly and both optimized paths must perform explicitly. The
94,985 pixels on `typical.png` are confined to the scaler, are all one gray level, and
are an algorithm choice rather than a language difference: the naive version
interpolates raw luma and then stretches, while both optimized versions stretch first
and interpolate the stretched 8-bit plane. They make that choice identically, which is
why they agree with each other exactly.

### What each optimization paid — and two that do not transfer

Four optimizations were ported in both directions. Two of them reverse sign.

| Optimization | Go | Rust |
|--------------|-----|------|
| Precompute per-column axis tables from the integer factor | **2.18x on transform** — the biggest single win | the biggest single win |
| Separable two-pass scaler | win | win |
| Fixed point instead of float | beats `f64` by 10.5%, **ties `f32`** | beats `f64` by 35%, **beats `f32` by 23%** |
| Kill the luma side-buffer | **-5.5% / -4.1%** (a win) | **+10% / +22%** (a **regression**) |

**Why "kill the luma buffer" reverses.** Go's `image/png` returns an `*image.RGBA`
whose `Pix` is already alpha-premultiplied, so recomputing luma in a second pass is
three multiplies and two adds. The Rust `png` crate returns **non-premultiplied**
bytes, so the reference semantics require the premultiply inside the pipeline — and a
second pass therefore re-pays **three integer divides per pixel**. Divides are the most
expensive operation in that loop, and paying them twice costs more than the buffer
traffic it saves. This is why the Rust recommendation keeps the buffer (`DB`) while the
Go recommendation drops it (`K`).

**Why "f32 is as good as fixed point" reverses.** In Go the finding is simply "stop
using `float64`" — `float32` and 16.16 fixed point are indistinguishable. In Rust they
are not: fixed point beats `f32` by 23%, because the autovectorizer packs 16- and
32-bit integer lanes far more densely than `f32` lanes.

### The SIMD verdict, both sides

**Rust auto-vectorizes, and its autovectorizer beat hand-written intrinsics.** Under
`-C target-cpu=native` the compiler emits AVX-512 for the scaler loops. An explicit
hand-written AVX2 vertical pass (variant `DS`) is **6.6% slower** than plain Rust at
the same flags — the intrinsics are pinned to 256-bit lanes and the autovectorizer is
not.

**Go auto-vectorizes nothing here, and it did not cost it.** The scaler's assembly is
unchanged between `GOAMD64=v1` and `v3`; v3's entire delta is the two FMA instructions
in the luma expression. The SIMD-shaped headroom was nonetheless collectable in
**scalar** Go. Variant `J` measures the ceiling by stripping all arithmetic out of both
inner loops, keeping only loads, stores and loop overhead — a deliberately wrong output
that exists only to price "what if the arithmetic were free". Variant `K` reaches that
ceiling by exploiting that the factor is not merely an integer but is 2, where the
bilinear weights are exactly 1/4 and 3/4 and the whole fixed-point apparatus collapses
to shift-and-add on 16-bit data. Paired, 7 pairs, typical transform:

| Comparison | Median ratio | Sign count | Verdict |
|------------|--------------|-----------|---------|
| K vs D | 0.723 | 0/7 | **K is 1.38x faster than D** |
| K vs J (arithmetic-free ceiling) | 0.994 | 3/7 | **tie — K is at the ceiling** |

3 of 7 is a coin flip, so K's remaining arithmetic is free relative to its load/store
traffic. There is nothing left in that loop for SIMD to collect, which is why Go's lack
of an autovectorizer costs it nothing on this pipeline. K is bit-identical to D by
construction, not by luck.

### Negative results

Published because they are results.

- **Bounds-check elimination by reslicing rows — refuted.** Verified against
  `-gcflags=-d=ssa/check_bce/debug=1` rather than assumed; table-driven indices are
  unprovable to the compiler, so the checks survive.
- **Removing the horizontal gather (Go variant G) — dead end.** Walking runs of
  constant source index instead of gathering per pixel is bit-identical to D by
  construction and did not pay. The gather was not the bottleneck.
- **A 32-bit vertical pass via an 8.8 intermediate (Go variant H) — dead end, and not
  free.** It did not pay, and unlike G it adds a rounding step, so it is not bit-exact.
- **PGO — a dud.** Two devirtualizations, both in `compress/flate`.
- **Hand-written AVX2 in Rust (variant DS) — lost** to the autovectorizer by 6.6%.
- **Porting "kill the luma buffer" into Rust — actively harmful**, +10% and +22%.

### Profiling coverage

**Go profiles are sampled and are checked in**: `prof/k_typical.flame.svg`,
`prof/k_big.flame.svg`, `prof/d_typical.flame.svg`, `prof/d_big.flame.svg` and
`prof/k_typical.callgraph.svg`, all rendered from `runtime/pprof` CPU profiles of the
full warm+timed loop.

**No sampling profiler was available for Rust on this host and none was used.** `wpr`
requires elevation, and there is no `blondie`/`dtrace` backend for `cargo-flamegraph`
on Windows, no Superluminal and no VTune. There is **no Rust flamegraph**. The Rust
stage attribution in `prof/rust_stages.txt` is manual `Instant` instrumentation of
stage boundaries, coarse-grained by construction and unable to see inside a stage.

### Earlier rounds

The Part 1 and Part 2 measurements — the within-language optimization history, the
`x/image/draw` regression, the LUT-stretch ablation and the PNG decode attribution —
are in [RESULTS.md](RESULTS.md), with every cross-language claim in them explicitly
marked retracted.

## How to run it

Prerequisites: Go 1.24 or newer (the current numbers were measured on Go 1.25.1;
earlier parts on 1.27.0) and a Rust toolchain. The Go harness needs
`golang.org/x/image` v0.25.0, fetched by `go build`. `prof/paired_xlang.py` needs
Python 3.

Build the two recommended binaries, plus the comparison tool. The build flags are part
of the result, so they are spelled out rather than left to defaults:

```sh
mkdir -p bin out

# Go: variant K is recommended at GOAMD64=v1 with PGO explicitly OFF. -pgo=off is
# required, not cosmetic: go build picks up go/default.pgo automatically otherwise.
cd go
GOAMD64=v1 go build -pgo=off -o ../bin/bench_v1nopgo.exe .
GOAMD64=v3 go build -pgo=off -o ../bin/bench_v3nopgo.exe .   # the v3 data point
go build -o ../bin/compare.exe ./compare
cd ..

# Rust: lto = "fat" and panic = "abort" live in Cargo.toml; target-cpu=native is the
# flag under test, so build both with and without it.
cd rust
cargo build --release && cp target/release/rsbench.exe bin/rsbench_ltoonly.exe
RUSTFLAGS="-C target-cpu=native" cargo build --release
cp target/release/rsbench.exe bin/rsbench_nativefat.exe
cd ..
```

Both harnesses take the same three arguments — variant, input PNG, output path — and
print one JSON line with per-phase min/median/mean, the chosen upscale factor, the
output dimensions and the output size:

```sh
./bin/bench_v1nopgo.exe K fixtures/typical.png out/typical_K.pgm
./rust/bin/rsbench_nativefat.exe DB fixtures/typical.png out/typical_rsDB.pgm
```

Other Go variants: `A` (naive, PNG out), `B`, `B2` (Part 1 baselines), `D`, `E`,
and the ablations `D1`-`D4`, `G`, `H`, `I`, `J`, `F64`, `F32`. Other Rust variants:
`C` (naive reference), `D`, `DS`, `D1`-`D3`, `F64`, `F32`.

Reproduce the paired head-to-head. The first spec is the reference; every ratio is
reported against it:

```sh
python prof/paired_xlang.py 11 typical \
  "goK_v1=bin/bench_v1nopgo.exe:K" \
  "rsDB_native=rust/bin/rsbench_nativefat.exe:DB" \
  "rsDB_baseflags=rust/bin/rsbench_ltoonly.exe:DB" \
  "goK_v3=bin/bench_v3nopgo.exe:K"
python prof/paired_xlang.py 11 big \
  "goK_v1=bin/bench_v1nopgo.exe:K" \
  "rsDB_native=rust/bin/rsbench_nativefat.exe:DB" \
  "rsDB_baseflags=rust/bin/rsbench_ltoonly.exe:DB" \
  "goK_v3=bin/bench_v3nopgo.exe:K"
```

`prof/paired_ab.py` is the Go-only ancestor of that script, and `prof/bench_configs.py`
sweeps the `GOAMD64` x PGO matrix for a fixed variant.

Compare any two outputs. The tool accepts `.png` and `.pgm` on either side and reports
differing-pixel count, percentage and maximum absolute delta:

```sh
./bin/compare.exe out/typical_K.pgm out/typical_rsDB.pgm
```

Capture a Go CPU profile of the whole warm+timed loop, and render the flamegraph:

```sh
CPUPROFILE=prof/k_typical.pprof ./bin/bench_v1nopgo.exe K fixtures/typical.png out/typical_K.pgm
go tool pprof -top bin/bench_v1nopgo.exe prof/k_typical.pprof
python prof/foldstacks.py prof/k_typical.pprof   # -> folded stacks + flame.svg
```

There is no equivalent for Rust: see "Profiling coverage" above.

Verify the PGO claim for yourself — the output is two lines, both in `compress/flate`:

```sh
cd go && go build -a -gcflags="all=-m=2" -o /tmp/pgochk.exe . 2>&1 | grep "PGO devirtualizing"
```

Attribute the PNG decode phase (inflate vs unfiltering, alternative decoders, and the
uncompressed-input floor):

```sh
./bin/decodeprobe.exe fixtures/big.png
```

Run the whole Go variant matrix with repeat-and-report (PowerShell), which is how the
Part 2 numbers were produced, and the Rust matrix per build-flag tag:

```powershell
./bench_all.ps1 -Reps 5
python rust/bench_rs.py --reps 5 --tag native --variants C,D,DB,DS,K,D1,D2,D3,F64,F32
python py/bench_py.py --reps 3
```

## Repo layout

```
go/          Go harness: main.go holds variants A, B, B2 and the dispatch; fast.go
             holds D, E and the D1-D4/F* ablations; faster.go holds G, H, I, J and
             the recommended K; plus go.mod and default.pgo
go/compare/  Output comparison tool (PNG or PGM in, pixel diff out)
go/decodeprobe/  PNG decode attribution: inflate vs unfilter, fastpng and
             klauspost/compress comparison, uncompressed-input floor
go/probe/    One-off pixel probe proving the LUT stretch's divergence from A is
             entirely exact half-way rounding ties
rust/        Rust crate: naive reference C, DB, the hand-AVX2 DS, the ablations, and
             K (upscale_2x/upscale_k), the ported factor-2 exact-weight collapse
rust/bench_rs.py  Repeat-and-report driver for the Rust variant matrix, per build tag
py/bench_py.py        Python contestant: Pillow, a numpy port, and a pure-interpreter
             probe, on the same 3+15 protocol and the same correctness gate
prof/paired_lang.py   Paired cross-language A/B driver covering Go, Rust and Python
prof/paired_xlang.py  Its Go-vs-Rust ancestor
prof/paired_ab.py     Its Go-only ancestor
prof/bench_configs.py Sweeps the GOAMD64 x PGO build matrix for a fixed variant
prof/foldstacks.py    pprof -> folded stacks -> flamegraph
prof/*.flame.svg      Go flamegraphs (variants D and K, both fixtures)
prof/rust_stages.txt  Rust manual stage timings, and why no Rust profile exists
bench_all.ps1  Repeat-and-report driver for the whole Go variant matrix
fixtures/    Input screenshots
out/         Benchmark output (git-ignored)
bin/         Built Go binaries (git-ignored)
RESULTS.md   Full measurements, the retraction, and the verdict
```

## License

MIT. See [LICENSE](LICENSE).
