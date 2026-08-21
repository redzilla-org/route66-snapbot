# Results

> **RETRACTION -- read this before Parts 1 and 2.** Every cross-language claim in
> Parts 1 and 2 is **withdrawn**, including the headline "Go's transform is 2.77x
> faster than Rust". Those parts compared a four-times-optimized Go implementation
> against Rust variant C, which was a faithful port of Go's *naive* variant -- an
> optimized/unoptimized comparison reported as a language result -- and they compared
> medians collected in *different sessions* on a host that drifts by up to 7.4% between
> sessions. Parts 1 and 2 are kept in full because their *within-language* findings and
> their method are still the record of how the optimizations were attributed. The
> corrected, paired, single-session head-to-head is **[Part 3](#part-3-the-retraction-and-the-corrected-head-to-head)**,
> which supersedes them on every cross-language number.

Measured 2026-08-20 on Windows 11 (x86-64), Go 1.25.1, rustc 1.93.1 (release profile,
`opt-level = 3`, `codegen-units = 1`), CPython 3.14.7 with Pillow 12.3.0 and numpy
2.5.2. Every variant is single-threaded.
15 timed iterations after 3 warmup iterations; each iteration re-decodes the PNG
from an in-memory byte slice, transforms, and writes the output file.

## Variants

| ID | Name | Pixel read | Upscale | Output |
|----|------|-----------|---------|--------|
| A | GO-ORIGINAL | `image.Image.At()` -> `color.Color` -> `RGBA()`, `[]float64` buffer | hand-rolled bilinear, closure sampler | PNG, `png.BestSpeed` |
| B | GO-OPTIMIZED | direct `Pix` indexing, type switch on `*image.RGBA` / `*image.NRGBA` / `*image.Gray`, `At()` fallback | `golang.org/x/image/draw` `BiLinear.Scale` | binary PGM (P5) |
| B2 | GO-OPTIMIZED, hand-rolled scaler | same as B | hand-rolled `uint8` bilinear | binary PGM (P5) |
| C | RUST | `png` crate 0.17.16, direct slice indexing | hand-rolled `f64` bilinear | binary PGM (P5) |

B2 is not in the original brief. It was added after B measured *slower* than A on the
scaling fixture; see "The x/image finding" below.

## Fixtures and upscale factor

The upscale factor is the largest integer factor up to 3 whose output stays inside a
20,000,000-pixel budget.

| Fixture | Input px | Bytes | Decoded Go type | Factor | Output px |
|---------|----------|-------|-----------------|--------|-----------|
| `typical.png` | 1366 x 2915 | 510,598 | `*image.RGBA` | 2 | 2732 x 5830 |
| `big.png` | 1366 x 5477 | 2,571,138 | `*image.RGBA` | 1 | 1366 x 5477 |

`big.png` at 7.48 MPix exceeds the budget at both 3x (67.3 MPix) and 2x (29.9 MPix),
so it is preprocessed at 1x. That makes it a clean isolation of the grayscale +
contrast-stretch stage with no scaler in the path.

## Correctness (pixels, vs variant A)

| Comparison | Dimensions | Differing pixels | Max abs delta |
|------------|-----------|------------------|---------------|
| B vs A, `typical` | 2732 x 5830 | 322,393 / 15,927,560 (2.0241%) | 1 |
| B2 vs A, `typical` | 2732 x 5830 | 94,985 / 15,927,560 (0.5964%) | 1 |
| C vs A, `typical` | 2732 x 5830 | 94,985 / 15,927,560 (0.5964%) | 1 |
| B vs A, `big` | 1366 x 5477 | 0 / 7,481,582 (0.0000%) | 0 |
| B2 vs A, `big` | 1366 x 5477 | 0 / 7,481,582 (0.0000%) | 0 |
| C vs A, `big` | 1366 x 5477 | 0 / 7,481,582 (0.0000%) | 0 |

Every port is **byte-identical to A on `big`**, where the scaler is bypassed. That
pins the decode, luma, min/max and contrast-stretch math as exactly equivalent across
all three languages, including the alpha-premultiplication that `At()` performs
implicitly and the `Pix`/`png`-crate paths must perform explicitly.

The residual differences on `typical` are confined to the scaler and are all
**delta = 1**, i.e. one gray level. A interpolates raw luma and stretches the
interpolated value; B/B2/C stretch first and interpolate the stretched 8-bit plane,
which rounds to `uint8` one step earlier. B2 and C produce **identical** output
(94,985 differing pixels, same count), which is expected: they implement the same
sample grid with the same `f64` arithmetic. B's larger 2.02% comes from `x/image`'s
own kernel edge handling and fixed-point rounding. No divergence exceeds one gray
level, so none of this is capable of changing an OCR verdict.

## Timings, milliseconds per image

### typical.png (1366 x 2915, factor 2)

| Variant | Phase | min | median | mean |
|---------|-------|-----|--------|------|
| A | decode | 41.51 | 47.42 | 47.49 |
| A | transform | 367.90 | 375.12 | 375.77 |
| A | encode+write | 131.19 | 141.97 | 140.40 |
| A | **total** | **546.51** | **565.84** | **563.66** |
| B | decode | 41.06 | 43.98 | 44.52 |
| B | transform | 765.91 | 786.90 | 790.42 |
| B | encode+write | 9.28 | 10.62 | 10.88 |
| B | **total** | **822.31** | **845.56** | **845.82** |
| B2 | decode | 41.15 | 43.23 | 43.88 |
| B2 | transform | 201.14 | 209.92 | 209.82 |
| B2 | encode+write | 0.00 | 11.07 | 11.22 |
| B2 | **total** | **257.33** | **263.13** | **264.92** |
| C | decode | 12.78 | 13.51 | 13.89 |
| C | transform | 219.76 | 231.09 | 230.89 |
| C | encode+write | 7.89 | 8.83 | 8.95 |
| C | **total** | **240.74** | **253.58** | **253.73** |

Output: A 878,538 bytes (PNG); B / B2 / C 15,927,577 bytes (PGM), 2732 x 5830.

### big.png (1366 x 5477, factor 1)

| Variant | Phase | min | median | mean |
|---------|-------|-----|--------|------|
| A | decode | 135.67 | 145.92 | 145.55 |
| A | transform | 348.67 | 364.37 | 363.86 |
| A | encode+write | 107.59 | 111.58 | 112.68 |
| A | **total** | **602.62** | **622.61** | **622.09** |
| B | decode | 135.33 | 144.18 | 144.09 |
| B | transform | 53.02 | 62.41 | 61.98 |
| B | encode+write | 0.00 | 5.22 | 5.06 |
| B | **total** | **203.23** | **209.93** | **211.13** |
| B2 | decode | 135.58 | 143.55 | 143.49 |
| B2 | transform | 52.01 | 60.29 | 59.82 |
| B2 | encode+write | 0.00 | 5.70 | 6.15 |
| B2 | **total** | **199.79** | **208.64** | **209.45** |
| C | decode | 44.00 | 47.13 | 46.98 |
| C | transform | 61.10 | 63.07 | 63.36 |
| C | encode+write | 4.14 | 4.48 | 4.59 |
| C | **total** | **111.02** | **114.83** | **114.92** |

Output: A 1,059,333 bytes (PNG); B / B2 / C 7,481,599 bytes (PGM), 1366 x 5477.

A 0.00 ms minimum in the encode+write row is the OS write cache absorbing the write
on a lucky iteration; the median is the number to read.

## Verdict

### How much of the win comes from dropping PNG encode (A -> B encode phase)?

A large, cheap, and completely safe chunk.

| Fixture | A encode (median) | B encode (median) | Saved | Share of A total |
|---------|------------------|-------------------|-------|------------------|
| `typical` | 141.97 ms | 10.62 ms | 131.35 ms | 23.2% |
| `big` | 111.58 ms | 5.22 ms | 106.36 ms | 17.1% |

Deflate at `BestSpeed` still costs 111-142 ms per image to produce a file that exists
only to be handed to the next process and then deleted. PGM costs 5-11 ms. The price
is disk footprint: 15.9 MB instead of 0.88 MB on `typical`, an 18x expansion for a
transient file. This is the single highest-value change in the whole comparison,
requires no new dependency, and cannot alter a pixel.

### How much from Pix indexing + x/image Scale (A -> B transform phase)?

Split the two, because they point in opposite directions.

**Pix indexing: 5.8x.** `big.png` runs at factor 1, so its transform is grayscale +
stretch with no scaler. 364.37 ms -> 62.41 ms median. That is the cost of `At()`
returning a boxed `color.Color` through an interface, plus `RGBA()`'s 16-bit
expansion, per pixel, for 7.5 million pixels. Removing it is nearly a 6x win on that
stage, and it is byte-for-byte identical output.

**x/image `BiLinear.Scale`: a 2.1x regression.** On `typical`, B's transform is
786.90 ms against A's 375.12 ms. The cause is in `golang.org/x/image/draw`'s
generated code: `BiLinear` is a `Kernel`, and `kernelScaler` has generated fast paths
only for a `*image.RGBA` destination. A `*image.Gray` destination falls back to
`scaleY_RGBA64Image_Src`, which walks the destination through the `RGBA64Image`
interface, four `float64` channels per pixel, for a one-channel image. Using
`x/image` here is slower than the naive hand-rolled loop it was supposed to replace.

Variant B2 keeps the `Pix` type switch and replaces the `x/image` call with a
hand-rolled `uint8` bilinear: transform 209.92 ms median, 1.79x faster than A and
3.75x faster than B. **B2 is the correct optimized Go implementation.**

End to end, best Go against the original: `typical` 565.84 -> 263.13 ms (2.15x),
`big` 622.61 -> 208.64 ms (2.98x).

### Does Rust beat optimized Go on the transform phase, and by what factor?

> **RETRACTED.** "Rust" here is variant C, an unoptimized reference port. See Part 3.

No. Rust is marginally slower on transform, in both fixtures:

| Fixture | Go B2 transform | Rust transform | Ratio |
|---------|-----------------|----------------|-------|
| `typical` | 209.92 ms | 231.09 ms | Go 1.10x faster |
| `big` | 60.29 ms | 63.07 ms | Go 1.05x faster |

Both compile the same `f64` arithmetic over the same flat `u8`/`f64` buffers to
comparable machine code. There is no language margin on this workload; the two are
within noise of each other, with Go slightly ahead.

Rust's one real advantage is **PNG decode**, where the `png` crate beats Go's
`image/png` by roughly 3x:

| Fixture | Go decode | Rust decode | Ratio |
|---------|-----------|-------------|-------|
| `typical` | 43.23 ms | 13.51 ms | 3.20x |
| `big` | 143.55 ms | 47.13 ms | 3.05x |

That is a decoder-quality difference (`fdeflate`'s specialized inflate plus a faster
unfilter), not a language difference. It carries the end-to-end totals: Rust 253.58
vs Go B2 263.13 ms on `typical` (1.04x), and 114.83 vs 208.64 ms on `big` (1.82x,
essentially all of it decode).

### Is the Rust margin large enough to justify a Rust toolchain?

**No. Recommendation: implement variant B2 in Go and stop there.**

The reasoning, in order of size:

1. **The whole win is available in Go.** 2.15x-2.98x end to end, from two changes
   with no new language and no new build stage: drop PNG encode for PGM, and read
   pixels through `Pix` instead of `At()`.
2. **Rust loses the phase it was hypothesized to win.** The transform is the part
   that was profiled as dominant, and Rust is 5-10% *slower* there than optimized Go.
3. **What is left is a library swap, not a language swap.** The residual Rust margin
   is entirely PNG decode. If decode later becomes the bottleneck, the proportionate
   move is a faster Go PNG decoder, not a second toolchain.
4. **The cost is structural, not incremental.** Introducing Rust means a rustc/cargo
   layer in the container image, a second dependency-audit surface (`png` pulls in
   `fdeflate`, `flate2`, `miniz_oxide`, `crc32fast`, `simd-adler32`, `adler2`,
   `bitflags`, `cfg-if`), a cross-language calling boundary or a subprocess, and a
   second set of build-cache and reproducibility rules. That is a permanent tax paid
   against a margin of 1.04x on one fixture.

The one caveat worth stating: `x/image/draw` was assumed to be the fast path and is
not, for a `*image.Gray` destination. Any future use of it for single-channel work
should be measured before it is adopted.

---

# Part 2: pprof-guided optimization of B2 (variants D and E)

Measured 2026-08-20 on the same host. Everything below is a **fresh re-measurement**
of every variant, old and new, in one interleaved session (`bench_all.ps1 -Reps 5`),
because the host is noisy enough that numbers from different sessions are not
comparable. Variants A, B, B2 and C are unchanged code; their Part 1 numbers stand as
published, and the Part 2 table re-measures them only to give D and E a same-session
baseline. Rust was re-measured in the same session and reproduced its Part 1 numbers
(typical 13.43 / 221.44 / 9.64; big 46.96 / 62.75 / 4.85), which confirms the two
sessions are comparable to within a few percent.

## Noise band -- read this before reading any delta

Each `bench.exe` invocation is 15 timed iterations after 3 warmups and reports the
median. Repeating that whole invocation 5 times, interleaved across variants, gives a
median-of-medians and a spread. **The spread is large**: on the transform phase it is
routinely 5-15% of the median, and on decode it reached 40% (big.png, variant A:
144.6-175.5 ms). Every table below carries the `[min-max]` of the five per-run
medians. A difference smaller than roughly 8% on transform, or 15% on decode, is
inside the noise and is reported as such rather than claimed as a win.

## The B2 profile -- where the time actually was

`CPUPROFILE=... bench.exe B2 ...`, then `go tool pprof -top`. Top entries, flat:

**typical.png (factor 2), 5.41 s of samples:**

```
      flat  flat%   sum%        cum   cum%
     3.54s 65.43% 65.43%      3.60s 66.54%  main.upscaleBilinearGray
     0.38s  7.02% 72.46%      0.92s 17.01%  image/png.(*decoder).readImagePass
     0.33s  6.10% 78.56%      0.61s 11.28%  main.grayStretchFast
     0.20s  3.70% 82.26%      0.20s  3.70%  runtime.memclrNoHeapPointers
     0.15s  2.77% 85.03%      0.15s  2.77%  runtime.cgocall
     0.13s  2.40% 87.43%      0.13s  2.40%  main.grayStretchFast.func1 (inline)
     0.12s  2.22% 89.65%      0.18s  3.33%  image/png.filterPaeth
     0.11s  2.03% 91.68%      0.11s  2.03%  runtime.memmove
     0.09s  1.66% 93.35%      0.09s  1.66%  hash/adler32.update
     0.06s  1.11% 94.45%      0.09s  1.66%  compress/flate.(*decompressor).huffSym
     0.06s  1.11% 96.67%      0.06s  1.11%  main.luma (inline)
```

**big.png (factor 1), 3.74 s of samples:**

```
      flat  flat%   sum%        cum   cum%
     650ms 17.38% 17.38%     1030ms 27.54%  main.grayStretchFast
     610ms 16.31% 33.69%      810ms 21.66%  image/png.filterPaeth
     530ms 14.17% 47.86%     2570ms 68.72%  image/png.(*decoder).readImagePass
     300ms  8.02% 55.88%      390ms 10.43%  compress/flate.(*decompressor).huffSym
     200ms  5.35% 61.23%      200ms  5.35%  image/png.abs (inline)
     200ms  5.35% 66.58%      200ms  5.35%  runtime.memclrNoHeapPointers
     190ms  5.08% 71.66%      190ms  5.08%  runtime.memmove
     180ms  4.81% 76.47%      990ms 26.47%  compress/flate.(*decompressor).huffmanBlock
     140ms  3.74% 80.21%      140ms  3.74%  hash/adler32.update
     140ms  3.74% 83.96%      140ms  3.74%  main.luma (inline)
     110ms  2.94% 90.37%      110ms  2.94%  main.grayStretchFast.func1 (inline)
```

Three things fall straight out of this and set the whole agenda:

1. On the scaling fixture the transform is **one function**: `upscaleBilinearGray` is
   65.4% of all samples, and its own cumulative is only 0.06 s above its flat time,
   so it is not calling anything expensive -- the cost is its own inner loop.
2. `runtime.memclrNoHeapPointers` at 3.7-5.4% is the Go runtime **zeroing the
   `[]float64` luma buffer** before it is written. That is pure waste, and it made the
   buffer the obvious first target.
3. `grayStretchFast.func1` -- the `put` closure that stores luma and updates min/max --
   appears separately at 2.4-2.9% flat despite being marked inline.

## New variants

| ID | Stretch | Scaler | Byte-identical to A at factor 1? |
|----|---------|--------|----------------------------------|
| **D** | bit-exact float64, no buffer (`grayStretchNoBuf`) | separable 2-pass, 16.16 fixed point, precomputed axis tables | **yes** |
| **E** | integer luma + 255 KB byte LUT (`grayStretchLUT`) | same as D | no (77 px, delta 1) |
| D1 | as D | B2's original float64 4-tap scaler | yes |
| D2 | B2's original (float64 buffer) | single-pass 4-tap, 16.16 fixed point | yes |
| D3 | as D | single-pass 4-tap, 16.16 fixed point | yes |
| D4 | as E | single-pass 4-tap, 16.16 fixed point | no (77 px, delta 1) |
| F64 | as D | separable, tables, **float64** arithmetic | yes |
| F32 | as D | separable, tables, **float32** arithmetic | yes |

D is the recommendation. E is faster still and is reported honestly as the fastest
thing measured, but it fails the factor-1 byte-identity gate (see Correctness) and so
is not the recommendation. D1-D4/F* exist only to attribute the hypotheses.

## Timings, milliseconds per image (median of 5 runs, `[min-max]` of those 5)

### typical.png (1366 x 2915, factor 2)

| Variant | decode | transform | encode+write | total |
|---------|--------|-----------|--------------|-------|
| A  | 51.49 [44.9-56.4] | 393.09 [366.0-466.9] | 141.44 [136.1-169.1] | **583.81** [551.1-696.7] |
| B  | 46.33 [44.1-54.2] | 790.28 [758.2-919.4] | 12.27 [11.3-14.5] | **853.98** [812.3-988.8] |
| B2 | 47.06 [43.4-61.4] | 224.39 [203.8-262.6] | 11.72 [11.0-14.6] | **284.79** [258.2-336.6] |
| D1 | 49.71 [46.0-51.4] | 212.11 [204.5-224.3] | 11.80 [11.2-12.0] | **273.12** [264.5-288.1] |
| D2 | 43.82 [43.6-53.6] | 102.78 [101.9-123.4] | 11.82 [10.8-13.7] | **157.21** [156.5-189.7] |
| D3 | 46.23 [45.9-56.2] | 102.45 [98.4-118.8] | 11.44 [10.6-13.4] | **159.67** [156.4-187.0] |
| D4 | 48.35 [47.1-56.5] | 93.72 [89.2-112.1] | 11.08 [10.7-13.8] | **151.58** [146.2-182.5] |
| **D** | 45.28 [43.4-52.2] | **79.82** [76.2-91.9] | 11.55 [10.8-12.4] | **135.28** [132.2-156.8] |
| **E** | 44.97 [43.2-48.1] | **72.00** [70.0-75.4] | 11.10 [10.5-12.8] | **128.19** [122.6-135.9] |
| C RUST | 13.43 | 221.44 | 9.64 | **246.26** |

### big.png (1366 x 5477, factor 1)

| Variant | decode | transform | encode+write | total |
|---------|--------|-----------|--------------|-------|
| A  | 146.20 [144.6-175.5] | 370.48 [349.0-426.1] | 113.43 [107.9-133.9] | **640.34** [605.3-738.0] |
| B  | 144.34 [141.6-184.0] | 62.59 [59.7-76.3] | 6.19 [5.7-8.4] | **215.67** [208.7-268.5] |
| B2 | 144.11 [139.1-201.5] | 61.77 [60.4-79.3] | 6.07 [5.6-7.7] | **212.83** [208.2-292.1] |
| D1 | 152.20 [144.5-179.2] | 59.24 [56.9-70.5] | 6.10 [4.2-6.1] | **217.48** [204.2-256.1] |
| D2 | 150.05 [146.7-170.9] | 66.85 [62.6-73.4] | 5.53 [5.1-6.7] | **220.64** [217.7-248.6] |
| D3 | 154.92 [146.0-175.7] | 60.13 [57.9-70.6] | 5.85 [4.8-6.7] | **219.52** [208.7-254.0] |
| D4 | 151.67 [145.0-157.2] | 37.20 [36.3-41.0] | 5.28 [4.6-6.2] | **194.29** [185.8-201.3] |
| **D** | 150.31 [142.7-174.6] | **58.05** [56.8-69.9] | 6.11 [5.9-6.9] | **214.10** [206.4-250.8] |
| **E** | 156.17 [140.1-162.7] | **39.78** [35.1-41.1] | 6.05 [5.0-6.6] | **200.84** [180.7-206.2] |
| C RUST | 46.96 | 62.75 | 4.85 | **114.25** |

At factor 1 the scaler is bypassed entirely, so D, D1 and D3 are the *same code path*
there; their 58.05 / 59.24 / 60.13 spread is a direct read-out of the noise floor
(~3.5%) and should be treated as one number.

## Hypotheses: confirmed or refuted

### 1. Kill the `[]float64` luma buffer -- CONFIRMED, but small on its own

`D1 vs B2` isolates it (same scaler, only the stretch changes):

| Fixture | B2 transform | D1 transform | Delta |
|---------|--------------|--------------|-------|
| typical | 224.39 | 212.11 | **-5.5%** |
| big | 61.77 | 59.24 | **-4.1%** |

Real, consistent in direction on both fixtures, and it removes a 9.8 MB allocation and
the `memclrNoHeapPointers` that the profile showed at 3.7-5.4% -- but at 4-6% it is
only just outside the noise band, and it is the *smallest* of the confirmed wins. The
hypothesis that recomputing luma beats storing it is correct; the hypothesis that this
would "cut allocation and memory traffic substantially" is right about the allocation
and overstates what it is worth in time.

Going further, to integer luma plus a byte LUT (`E`/`D4`), is where the stretch
actually gets fast:

| Fixture | D3 (float stretch) | D4 (LUT stretch) | Delta |
|---------|--------------------|------------------|-------|
| typical | 102.45 | 93.72 | -8.5% |
| big | 60.13 | 37.20 | **-38.1%** |

On big.png, where the transform is *nothing but* the stretch, the LUT cuts it by more
than a third. The mechanism is that 0.299/0.587/0.114 are exact thousandths, so
`L = 299r + 587g + 114b` is exactly 1000x the float luma with no representation error;
L spans at most 255,001 values, so the entire per-pixel stretch collapses into one
byte-table load.

**The stated rounding caveat did not survive contact.** The brief predicted "at most
one-gray-level differences, which is acceptable here." The measured divergence is
indeed at most one gray level -- 77 of 7,481,582 pixels on big.png -- but that breaks
the factor-1 byte-identity gate, so it is *not* acceptable, and E is demoted to an
ablation for it. See Correctness below for why no L-indexed LUT can be fixed.

### 2. Exploit the integer upscale factor (precompute per-column x0/x1/weight) -- CONFIRMED, the single biggest win

`D2 vs B2` isolates the scaler (same stretch, only the scaler changes), on typical.png,
the only fixture that scales:

| B2 transform | D2 transform | Delta |
|--------------|--------------|-------|
| 224.39 | 102.78 | **-54.2%, i.e. 2.18x** |

For 15.9 million output pixels the old loop recomputed a float divide, a
`math.Floor`, two clamps and a weight subtraction that between them have only 2732
distinct answers down the x axis and 5830 down the y axis. Hoisting them out is worth
more than every other change combined.

### 3. Integer fixed-point instead of float -- CONFIRMED vs float64, REFUTED vs float32

Holding the tables and the separable structure constant and swapping only the
arithmetic type (typical.png transform, 3 runs each):

| Scaler arithmetic | transform (median of 3) | vs 16.16 fixed |
|-------------------|-------------------------|----------------|
| 16.16 fixed point (D) | 80.54 | -- |
| float64 (F64) | 89.03 | +10.5% slower |
| float32 (F32) | 79.97 | -0.7%, **inside the noise** |

Fixed point beats float64 by about 10%, which is outside the noise band and is a real
result. Fixed point does **not** beat float32 -- the two are indistinguishable. So the
useful part of hypothesis 3 is "stop using float64", not "use integers". Fixed point
is kept anyway because it is *exactly* representable at factor 2 (the weights 0.25 and
0.75 are exact binaries), which is what lets D reproduce B2's output bit-for-bit.

### 4. Bounds-check elimination by reslicing per row -- REFUTED as stated, with one real exception

Checked against `-gcflags=-d=ssa/check_bce/debug=1`, not assumed. Reslicing each row to
its exact length did **not** empty the hot loops:

```
./fast.go:242:16: Found IsSliceInBounds     <- upscaleBilinearFixed inner loop
./fast.go:242:24: Found IsInBounds
./fast.go:246:21: Found IsInBounds
./fast.go:246:35: Found IsInBounds
./fast.go:249:20: Found IsInBounds
./fast.go:276:25: Found IsInBounds          <- separable horizontal pass
./fast.go:276:29: Found IsInBounds
```

The reason is structural: the x indices come out of the precomputed `x0s`/`x1s`
tables, so the compiler has no way to prove them in range no matter how the row is
sliced. Row reslicing removes the *row* bound, which was never the expensive one.

The one place it did work is the three-channel read. Writing
`p := row[x*4 : x*4+3 : x*4+3]` and then `p[0]`, `p[1]`, `p[2]` produces **one**
`IsSliceInBounds` and no per-channel check, where the direct `row[x*4]`, `row[x*4+1]`,
`row[x*4+2]` form produces three `IsInBounds` (`fast.go:73` still shows the latter, in
the min/max pass, and is a known residual). So the technique is real but its scope is
much narrower than the hypothesis claimed, and it is not where the time was.

Escape analysis (`-gcflags=-m`) found nothing to fix: the LUT and the separable
intermediate are both reported as not escaping, and the only heap escapes are the
three per-axis tables, which are built once per image.

### 5. Separable two-pass scaling -- CONFIRMED, contrary to the stated doubt

The brief said this "is not obviously better at these sizes and may lose to cache
effects." It wins, clearly and on both stretch implementations (typical.png transform):

| Pair | single-pass 4-tap | separable 2-pass | Delta |
|------|-------------------|------------------|-------|
| D3 -> D (bit-exact stretch) | 102.45 | 79.82 | **-22.1%** |
| D4 -> E (LUT stretch) | 93.72 | 72.00 | **-23.2%** |

Both at 22-23%, far outside the noise band. The 4-tap loop reads two source rows and
does four multiply-adds per output pixel; the separable version does two multiply-adds
in the horizontal pass over `w*scale*h` intermediate values and two in the vertical,
and the vertical pass reads two *already-horizontally-filtered* rows sequentially. The
intermediate buffer is 31.8 MB on typical.png and it still wins, which is the part
that was genuinely not obvious.

### 6. Factor-1 special case -- CONFIRMED already present, and preserved

`preprocessFast2` (B2) already short-circuits with `if scale <= 1 { return small }`,
and every new variant does the same. big.png never enters a scaler. This is not a new
win; it is a confirmation that no degenerate bilinear is running, and the factor-1
transform numbers (which are pure stretch, and which track the stretch changes exactly)
corroborate it.

## Correctness (pixels, vs variant A)

| Comparison | Dimensions | Differing pixels | Max abs delta |
|------------|-----------|------------------|---------------|
| **D vs A, `typical`** | 2732 x 5830 | 94,985 / 15,927,560 (0.5964%) | **1** |
| **D vs A, `big`** | 1366 x 5477 | **0 / 7,481,582 (0.0000%)** | **0** |
| F64 vs A, `typical` | 2732 x 5830 | 94,985 (0.5964%) | 1 |
| F64 vs A, `big` | 1366 x 5477 | 0 (0.0000%) | 0 |
| F32 vs A, `typical` | 2732 x 5830 | 94,985 (0.5964%) | 1 |
| F32 vs A, `big` | 1366 x 5477 | 0 (0.0000%) | 0 |
| E vs A, `typical` | 2732 x 5830 | 94,998 (0.5964%) | 1 |
| E vs A, `big` | 1366 x 5477 | 77 (0.0010%) | 1 |
| D4 vs A, `big` | 1366 x 5477 | 77 (0.0010%) | 1 |

**D passes both gates exactly.** At factor 1 it is byte-identical to A: 0 differing
pixels, 0 delta. At factor 2 its differing-pixel count is **94,985 -- the identical
number B2 and Rust produce**, i.e. D's 16.16 fixed-point separable scaler is
bit-for-bit the same as B2's float64 4-tap scaler on this fixture, not merely close.
That is not luck: at factor 2 the only bilinear weights are 0.25 and 0.75, which are
exact in binary, so the fixed-point path has no rounding error to accumulate.

**E fails the factor-1 gate, and it was investigated rather than accepted.** A probe
(`go/probe`) recomputed both paths pixel by pixel on big.png:

```
rgb=(168,156,164) L=160500 exact=160.5000000000 A=160 exactRound=161 tieRemainder=255000/510000
rgb=(72,186,235)  L=157500 exact=157.5000000000 A=157 exactRound=158 tieRemainder=255000/510000
rgb=(95,83,91)    L=87500  exact= 87.5000000000 A= 87 exactRound= 88 tieRemainder=255000/510000
differing=77  exact-half-way-ties=77  minI=0 maxI=255000 spanI=255000
```

All 77 divergent pixels -- 77 out of 77 -- are values whose exact stretched result is a
**precise x.5**. A's float64 luma lands a hair below the tie and `uint8(v + 0.5)`
truncates down; exact integer arithmetic rounds half up. Breaking the LUT's ties
downward instead does not fix it: that flips a *different* 180 pixels the other way,
for 257 total. The reason is that A's rounding error at a tie depends on the individual
(r,g,b) triple, and many triples share one L -- so **no table indexed by L alone can
reproduce it**, in principle. That is why D uses the bit-exact float stretch and gives
up the LUT's 38% on the big.png stretch, and why E is reported as an ablation.

No new variant exceeds one gray level of delta anywhere.

## Step 3: the decode gap

`go/decodeprobe` splits Go's PNG decode into inflate and everything-else, tries the
alternative inflate, and measures the floor.

```
fixtures/typical.png                      fixtures/big.png
  IDAT compressed bytes : 509,041           IDAT compressed bytes : 2,563,569
  inflated bytes        : 11,948,585        inflated bytes        : 22,450,223
  png.Decode (full)     :   44.10 ms        png.Decode (full)     :  136.11 ms
  stdlib zlib inflate   :   15.44 ms 35.0%  stdlib zlib inflate   :   56.66 ms 41.6%
  klauspost zlib inflate:   12.62 ms 1.22x  klauspost zlib inflate:   46.83 ms 1.21x
  remainder (unfilter+) :   28.66 ms 65.0%  remainder (unfilter+) :   79.45 ms 58.4%
  fastpng.Decode (full) :   41.47 ms 1.06x  fastpng.Decode (full) :  129.75 ms 1.05x
  raw byte copy floor   :    2.52 ms        raw byte copy floor   :    5.62 ms
```

**Inflate vs unfiltering.** Inflate is the *minority* of Go's decode cost: 35% on
typical, 42% on big. The majority -- 58-65% -- is unfiltering and pixel expansion,
which the B2 profile corroborates independently (`filterPaeth` 610 ms flat plus
`png.abs` 200 ms, against `flate.huffmanBlock` 990 ms cumulative, inside a
`readImagePass` cumulative of 2570 ms on big.png).

**Third-party Go decoders.** The honest finding is that there is essentially nothing
there:

- **`github.com/gameparrot/fastpng`** -- the only pure-Go PNG package found that is
  explicitly a performance fork of `image/png`. It is a copy of the standard decoder
  with `klauspost/compress` swapped in for `compress/zlib`. Measured: **1.06x on
  typical, 1.05x on big.** Real, free, and nowhere near enough. It is also a
  single-author repository with no tagged release (pseudo-version
  `v0.0.0-20250305185850-d72e123a2123`), which is a dependency-audit cost out of all
  proportion to 5%.
- **`github.com/klauspost/compress/zlib`** -- maintained and excellent, measured at
  **1.21-1.22x on the inflate step alone**. But inflate is only 35-42% of decode, so
  the ceiling on total decode is about 8%, and `image/png` exposes no seam to swap its
  zlib without forking the decoder. Forking it is exactly what fastpng did, and 1.06x
  is what it bought.
- **Wuffs** is the decoder that actually closes this gap -- it beats libpng by
  1.22-2.75x, by SIMD unfiltering and by inflating the whole image at once rather than
  row by row -- but it is C, transpiled, not pure Go. Adopting it is the same category
  of decision as adopting Rust, and it was out of scope here.
- Nothing else surfaced. There is no maintained pure-Go PNG decoder competitive with
  `fdeflate`.

**The cheapest available win is upstream, and it is enormous.** The decode phase exists
only because the producing step hands over a PNG. If it emitted an uncompressed format
instead -- raw RGBA, or BMP, or PGM -- the decode phase would collapse to the cost of
moving the bytes: **2.52 ms instead of 44.10 (94.3% saved) on typical, 5.62 ms instead
of 136.11 (95.9% saved) on big.** That is 41.6 ms and 130.5 ms per image, larger than
everything Part 2 won in the transform on `big.png`, and it needs no decoder work at
all. It is not implemented here because it is a change to the *producer*, outside this
repo's scope; it is quantified so the size of the prize is on the record. Note that
the same argument already won the encode phase in Part 1 (PNG -> PGM, 131 ms saved);
this is the identical trade on the other end of the pipeline, and it is bigger.

## Step 4: parallelism -- deliberately not done

The transform is **not** parallelized, and should not be. In the real consumer many
images are preprocessed concurrently across all cores already, so intra-image
parallelism would only oversubscribe the machine: it would improve single-image
latency on an idle box and degrade aggregate throughput on a busy one. **Single-
threaded per-image throughput is the correct metric for this workload**, every number
in this document is single-threaded, and this should not be re-proposed.

## Verdict: does optimized Go beat Rust?

> **RETRACTED IN FULL.** This whole section pits optimized Go against *unoptimized*
> Rust, across two sessions on a drifting host. The 2.77x below is not a language
> result. The corrected paired measurement -- Go 1.51x on typical, 1.26x on big, with
> Rust winning decode by 2.6-2.8x and total time on big.png by 1.58x -- is in Part 3.
> The section is kept, unedited, so the retraction can be checked against what it
> retracts.

**On the transform phase -- yes, decisively, on both fixtures.**

| Fixture | Rust C | Go D | Go E | D vs Rust |
|---------|--------|------|------|-----------|
| typical (2x) | 221.44 | **79.82** | 72.00 | **Go 2.77x faster** |
| big (1x) | 62.75 | **58.05** | 39.78 | Go 1.08x faster |

On typical.png this is not a margin, it is a different cost class: Go D does the
scaling transform in 36% of Rust's time. Note *why* -- Rust's variant C is a faithful
port of the same naive `f64` bilinear that B2 ran, so this is not a language result at
all. It is the result of applying hypotheses 2, 3 and 5 on one side and not the other.
Given the same treatment the Rust scaler would be expected to land in the same place.

On big.png (factor 1, no scaler) D is 1.08x ahead, which is at the edge of the noise
band and should be read as parity; E's 39.78 vs 62.75 (1.58x) is the real result there,
and it is the LUT stretch doing it.

**On total time -- split, and the split is entirely PNG decode.**

| Fixture | Rust C total | Go D total | Result |
|---------|--------------|------------|--------|
| typical (2x) | 246.26 | **135.28** | **Go 1.82x faster** |
| big (1x) | 114.25 | 214.10 | **Rust 1.58x faster** |

On `typical.png`, optimized Go now beats Rust end to end by 1.82x -- the goal is met,
and met by a wide margin. On `big.png` it does not, and cannot be made to by any
amount of further transform work: Go's decode is 150.31 ms against Rust's 46.96 ms, a
103 ms deficit against a 214 ms total, while the entire remaining transform is 58 ms.
Even taking every decode win identified above -- fastpng's 5% -- Go lands near 125-130
ms of decode and still loses that fixture. The only two things that close it are a
Wuffs-class decoder or, far better, not decoding a PNG at all.

**Plain statement:** optimized Go beats Rust on transform alone on both fixtures
(2.77x on typical, parity-to-1.58x on big), and beats it on total time on typical.png
(1.82x) but not on big.png (1.58x behind), where PNG decode -- a decoder-quality gap,
not a language gap -- is 70% of Go's total and 41% of Rust's.

**Recommendation, unchanged in shape from Part 1 but stronger:** implement variant D
in Go. It is 2.10x faster end to end than B2 on typical.png (284.79 -> 135.28) and
byte-identical to A wherever the published gate requires it. Then take the upstream
change and delete the PNG decode entirely, which is worth more than everything in
Part 2 put together.

---

# Part 3: the retraction, and the corrected head-to-head

Measured 2026-08-20, same host, Go 1.25.1, cargo release profile. This part supersedes
the cross-language verdict in Parts 1 and 2.

## RETRACTION

**Part 2's headline -- "optimized Go's transform is 2.77x faster than Rust" -- is
withdrawn. It was not a language result and it should not be cited.**

It compared Go variant D, which had been through four rounds of profile-guided
optimization, against Rust variant C, which was a faithful port of Go's *naive*
variant A: an `f64` luma side-buffer, a fused four-tap `f64` bilinear kernel, no axis
tables, no separable pass. C was written as a correctness reference, and it was
correct; it was never an optimized implementation, and reporting it as "Rust" made the
comparison a measurement of two different algorithms wearing two different languages.

A second, independent defect: the Part 1 and Part 2 numbers for the two languages were
collected in different sessions. This host drifts. The same Go binary running the same
variant on the same fixture moved 82.8 -> 88.9 ms on transform across two sessions --
7.4%, which is larger than several of the differences Part 2 reported as findings.
Two medians from two sessions are not comparable at that resolution, in either
direction.

Both defects are fixed below: each language's four optimizations were ported into the
other, and the final contestants were re-measured **in one interleaved session**.

## Method: paired, interleaved, order-shuffled

`prof/paired_xlang.py` (a cross-language sibling of `prof/paired_ab.py`, which could
only resolve Go binaries) runs every contestant **back to back inside one pair**, with
the order **shuffled per pair**, 11 pairs per fixture. Each invocation is itself the
standard 15 timed iterations after 3 warmups and reports its own median.

The reported statistic is therefore the **median of per-pair ratios**, not the ratio of
two independently drifting medians. Drift that is slow relative to one pair cancels
out. Alongside it is a **sign count**: the number of pairs in which the direction held.

**How to read a sign count.** 11/11 or 0/11 means the direction was consistent in every
pair and the median ratio is a result. A count near 5-6 out of 11 means the two
configurations traded places from pair to pair -- that is a **tie**, and the median
ratio must not be quoted as a margin no matter how far from 1.000 it happens to land.

Contestants:

| Label | Build |
|-------|-------|
| `goK_v1` | Go variant K, `GOAMD64=v1`, `-pgo=off` -- the Go recommendation |
| `rsDB_native` | Rust variant DB, `RUSTFLAGS="-C target-cpu=native"`, `lto="fat"`, `panic="abort"` -- the Rust recommendation |
| `rsDB_baseflags` | Rust variant DB, same source and same `lto`/`panic`, **without** `-C target-cpu=native` |
| `goK_v3` | Go variant K, `GOAMD64=v3`, `-pgo=off` -- a labelled data point, **not** the recommendation (see the 80-pixel note) |

## The corrected head-to-head

Absolute medians of the 11 per-pair medians, milliseconds:

### typical.png (1366 x 2915, factor 2)

| Config | decode | transform | encode+write | total |
|--------|--------|-----------|--------------|-------|
| goK_v1 | 46.77 | **59.45** | 14.07 | 120.06 |
| rsDB_native | **17.05** | 87.69 | **10.41** | **115.66** |
| rsDB_baseflags | 16.58 | 100.91 | 10.80 | 127.49 |
| goK_v3 | 47.29 | 57.71 | 14.31 | 119.49 |

### big.png (1366 x 5477, factor 1 -- no scaler in the path)

| Config | decode | transform | encode+write | total |
|--------|--------|-----------|--------------|-------|
| goK_v1 | 144.31 | **57.51** | 6.56 | 208.84 |
| rsDB_native | **53.94** | 72.32 | **5.42** | **131.77** |
| rsDB_baseflags | 52.67 | 80.20 | 5.29 | 136.79 |
| goK_v3 | 142.77 | 54.18 | 6.71 | 203.09 |

### Per-pair ratios, Go K (v1) over Rust DB (native)

A ratio above 1.000 means Go is slower.

| Fixture | Phase | Median ratio | Sign count | Verdict |
|---------|-------|--------------|-----------|---------|
| typical | decode | 2.752 | 11/11 Rust faster | **Rust 2.75x** |
| typical | transform | 0.662 | 0/11 Rust faster | **Go 1.51x** |
| typical | encode+write | 1.353 | 11/11 Rust faster | Rust 1.35x |
| typical | **total** | 1.031 | 8/11 Rust faster | **tie, leaning Rust** |
| big | decode | 2.640 | 11/11 Rust faster | **Rust 2.64x** |
| big | transform | 0.793 | 1/11 Rust faster | **Go 1.26x** |
| big | encode+write | 1.313 | 9/11 Rust faster | Rust 1.31x |
| big | **total** | 1.582 | 11/11 Rust faster | **Rust 1.58x** |

**The corrected statement.** Optimized Go beats optimized Rust on the transform phase
on both fixtures, by **1.51x on typical and 1.26x on big** -- not by 2.77x. Optimized
Rust beats Go on PNG decode by **2.6-2.8x** and on the PGM write by **1.3x**. End to
end the two are a **tie on typical.png** (median 1.031, but only 8/11 pairs in the same
direction, and the per-pair range 0.938-1.188 straddles 1.0) and **Rust wins big.png
by 1.58x**, entirely on decode.

Every direction reported as a result above held in at least 9 of 11 pairs. The one
number that did not -- typical total -- is reported as a tie rather than as a 3% Rust
win, which is exactly what the sign count is for.

## Is Rust's lead just AVX-512 that the Go build is not allowed to use?

No. Rust at baseline flags (no `-C target-cpu=native`) still wins:

| Fixture | Phase | Go K v1 / Rust DB baseflags | Sign count |
|---------|-------|------------------------------|-----------|
| typical | decode | 2.841 | 11/11 Rust faster |
| typical | transform | 0.617 | 0/11 Rust faster |
| typical | total | 0.948 | 3/11 Rust faster |
| big | decode | 2.713 | 11/11 Rust faster |
| big | transform | 0.728 | 3/11 Rust faster |
| big | total | 1.535 | 11/11 Rust faster |

`target-cpu=native` costs Rust nothing on decode (2.84x -> 2.75x is inside the pair
noise) and buys it 13% on the typical transform (100.91 -> 87.69 ms) and 10% on big
(80.20 -> 72.32 ms). Rust's decode advantage -- which is the whole of its `big.png`
total-time win -- is a property of the `png` crate, not of AVX-512. Conversely, Rust's
transform **loses to Go by more** at baseline flags (0.617 vs 0.662 on typical), so the
native-CPU flag is what narrows Go's transform lead, not what creates Rust's.

The nearest Go analogue, `GOAMD64=v3`, is included as a labelled data point:

| Fixture | Phase | Go K v1 / Go K v3 | Sign count |
|---------|-------|--------------------|-----------|
| typical | transform | 1.024 | 8/11 v3 faster |
| typical | total | 1.009 | 7/11 v3 faster |
| big | transform | 1.036 | 9/11 v3 faster |
| big | total | 1.005 | 7/11 v3 faster |

v3 is worth 2-4% on transform and nothing measurable on total, and it **forfeits
byte-exactness** -- see below. It is not the recommendation.

## GOAMD64=v3: the win is the FMA, and it costs 80 pixels

The v3 build is not byte-identical to the v1 build:

| Fixture | Differing pixels, K@v3 vs K@v1 | Max delta |
|---------|-------------------------------|-----------|
| big | **80** / 7,481,582 (0.0011%) | 1 |
| typical | 23 / 15,927,560 (0.0001%) | 1 |

The mechanism is pinned, not guessed. The only changed instructions in the hot path are
in the luma expression, where v3 contracts `0.299r + 0.587g + 0.114b` into two
`VFMADD231SD`. FMA rounds once instead of twice, so the stretched value lands on the
other side of a rounding boundary for a handful of pixels. Variant `I` is the control:
it is the stretch with each product wrapped in an explicit `float64()` conversion,
which the Go spec forbids the compiler to fuse across; built at v3, `I` gives the win
back.

So `GOAMD64=v3` is a **product decision, not a performance one**: it trades exact
reproducibility against a v1-built reference for 2-4% of one phase. On this pipeline
the byte-identity gate at factor 1 is worth more than the 2-4%, so v1 is recommended.

## PGO is worth nothing here

Building with `go/default.pgo` (collected from this workload's own profile) produces
exactly **two** PGO devirtualizations in the entire program, verified with
`go build -a -gcflags="all=-m=2"`:

```
compress/flate/inflate.go:697:24: PGO devirtualizing interface call f.r.ReadByte to bufio.(*Reader).ReadByte
compress/flate/inflate.go:720:26: PGO devirtualizing interface call f.r.ReadByte to bufio.(*Reader).ReadByte
```

Both are inside `compress/flate`, i.e. inside the PNG decoder. **Not one is in the
pipeline's own code.** That is the expected outcome once variant D removed the
`At()`/`color.Color` interface traffic: PGO's main lever on this program was the
indirect calls that the optimization had already deleted. PGO is a dud on this
workload, and it is reported as such.

## What each optimization actually paid, per language

Four optimizations were ported both ways. **Two of the four do not transfer.**

### 1. Kill the luma side-buffer -- a win in Go, a REGRESSION in Rust

| Language | Effect on transform |
|----------|---------------------|
| Go (D1 vs B2) | **-5.5% typical, -4.1% big** (a win) |
| Rust (D vs DB, native) | **+9.6% typical, +22.4% big** (a loss) |

Same idea, opposite sign, and the cause is a decode-format difference, not a language
difference. Go's `image/png` hands back an `*image.RGBA` whose `Pix` is **already
alpha-premultiplied**, so recomputing luma in pass 2 is three multiplies and two adds.
The Rust `png` crate hands back **non-premultiplied** RGBA bytes, so the reference
semantics require the premultiply to happen in the pipeline -- and a second pass
therefore re-pays **three integer divides per pixel** on top of the luma. Divides are
the most expensive thing in the loop, and paying them twice costs more than the 8 bytes
per pixel of buffer traffic it saves.

This is why the Rust recommendation is `DB` (keep the buffer) while the Go
recommendation is `K` (drop it). Porting the optimization faithfully would have made
Rust slower.

### 2. Precompute the per-column axis tables from the integer factor -- a win in both

The largest single win on both sides. In Go, `D2 vs B2` is **-54.2%, i.e. 2.18x** on
the typical transform. In Rust the same restructuring carries variant `D`/`DB` from
C's 213.7 ms to 58-64 ms on typical.

### 3. Fixed point instead of float -- confirmed in Go against f64, and the "f32 is just as good" result does NOT transfer

| Language | 16.16 fixed | f32 | f64 |
|----------|-------------|-----|-----|
| Go (typical transform) | 80.54 | 79.97 (**-0.7%, a tie**) | 89.03 (+10.5%) |
| Rust (typical transform, native) | 63.7 | 83.2 (**+30.6%, i.e. fixed wins by 23%**) | 86.1 (+35.2%) |

In Go, "stop using `float64`" is the whole finding -- `float32` and 16.16 fixed point
are indistinguishable. In Rust they are **not**: fixed point beats `f32` by 23%. Rust's
autovectorizer packs 16- and 32-bit integer lanes far more densely than `f32` lanes, so
the integer form is not merely equal-cost arithmetic, it is more work per vector
instruction.

### 4. Separable two-pass scaling -- a win in both

Confirmed on both sides, against the stated doubt that cache effects would eat it.

## The SIMD verdict, both sides

**Rust: the autovectorizer wins, and hand-written intrinsics lose to it.** Under
`-C target-cpu=native` on this host the compiler emits AVX-512 for the scaler loops.
Variant `DS`, an explicit hand-written AVX2 vertical pass, was written to test whether
intrinsics beat the autovectorizer. They do not: `DS` is **6.6% slower** than the
plain-Rust `D` at the same flags (67.9 vs 63.7 ms, typical transform). The hand-written
code is pinned to 256-bit lanes; the autovectorizer is not.

**Go: nothing is auto-vectorized, and it did not matter.** The Go compiler emits no
vector code for these loops -- the scaler's assembly is unchanged between `GOAMD64=v1`
and `v3`, and v3's entire delta is the two FMA instructions in the luma expression.
That looks like a structural disadvantage, and the interesting result is that it was
not one on this pipeline: **the SIMD-shaped headroom was collectable in scalar Go.**

Variant `J` measures the ceiling. It is the separable scaler with **all arithmetic
stripped out of both inner loops**, keeping only the loads, stores and loop overhead --
a deliberately wrong output that exists only to put a number on "what if the arithmetic
were free". Against `J`, variant `D` was 1.45x slower, i.e. roughly half the scaler's
time was arithmetic and there was real headroom to chase.

Variant `K` collects it without a single vector instruction, by exploiting that the
factor is not merely an integer but is 2 (the pixel budget caps it at 3, and 1 is a
no-op). At factor 2 the bilinear weights are exactly 1/4 and 3/4, so the whole 16.16
apparatus collapses to shift-and-add on 16-bit data: `mid = 3a+b` and `a+3b`
horizontally, `out = (3*M0 + M1 + 8) >> 4` vertically. Two 32-bit and two 64-bit
multiplies per pixel disappear, and the intermediate halves from `uint32` to `uint16`
(31.8 MB -> 15.9 MB on typical.png).

Paired measurement, 7 pairs, typical transform:

| Comparison | Median ratio | Sign count | Verdict |
|------------|--------------|-----------|---------|
| K vs D | 0.723 | 0/7 D faster | **K is 1.38x faster than D** |
| K vs J (the arithmetic-free ceiling) | 0.994 | 3/7 J faster | **tie -- K is AT the ceiling** |

K does not merely approach the memory floor, it reaches it: 3/7 is a coin flip, so the
remaining arithmetic in K is free relative to the load/store traffic. There is nothing
left for SIMD to collect in this loop, which is why Go's lack of an autovectorizer
costs it nothing here. And K is **bit-identical to D by construction**, not by luck --
1/4 and 3/4 are exact in binary, and `(3*M0+M1+8)>>4` is the same round-half-up of the
same rational value that D's single final rounding produces.

## Negative results, published as results

These are findings. They cost time, they are true, and they belong in the record.

- **Bounds-check elimination by reslicing rows -- REFUTED.** Verified against
  `-gcflags=-d=ssa/check_bce/debug=1` rather than assumed. Reslicing each row to its
  exact length did not empty the hot loops: table-driven indices are unprovable to the
  compiler, so the checks stay.
- **Variant G, removing the horizontal gather -- DEAD END.** D's horizontal pass reads
  `x0s[ox]`, `x1s[ox]`, `row[x0s[ox]]`, `row[x1s[ox]]` -- five bounds checks and two
  data-dependent loads. Because the factor is an integer, `x0` is constant across runs
  of exactly `scale` output columns, so G walks runs and hoists both source loads into
  registers. It is bit-identical to D by construction and it did not pay. The gather
  was not the bottleneck.
- **Variant H, a 32-bit vertical pass via an 8.8 intermediate -- DEAD END, and not
  free.** The vertical pass is the bigger of the two and D runs it in 64-bit. Rounding
  the intermediate to 8.8 makes it fit in 32-bit lanes. It did not pay enough to be
  worth its cost, and unlike G it is **not bit-exact** -- it adds a rounding step. A
  non-exact change that is also not faster is an easy call.
- **PGO -- A DUD.** Two devirtualizations, both in `compress/flate`, none in the
  pipeline's own code. See above.
- **Hand-written AVX2 in Rust (variant DS) -- LOST to the autovectorizer** by 6.6%.
- **Porting "kill the luma buffer" into Rust -- ACTIVELY HARMFUL**, +10%/+22%. See
  above.

## Profiling coverage, stated plainly

**Go: sampled profiles exist.** `prof/*.flame.svg` are flamegraphs rendered from
`runtime/pprof` CPU profiles of the full warm+timed loop --
`prof/k_typical.flame.svg`, `prof/k_big.flame.svg`, `prof/d_typical.flame.svg`,
`prof/d_big.flame.svg`, plus `prof/k_typical.callgraph.svg`. Every Go attribution in
this report traces to one of them.

**Rust: NO sampling profiler was available on this host, and none was used.** `wpr`/ETW
refuses to start without elevation; there is no `blondie` or `dtrace` backend for
`cargo-flamegraph` on Windows, and no Superluminal or VTune. There is **no Rust
flamegraph** and nothing in this report should be read as if there were. The Rust
attribution in `prof/rust_stages.txt` is **manual `Instant` instrumentation** of stage
boundaries -- median of 15 iterations, coarse-grained by construction, and unable to
see inside a stage. Where the Go and Rust attributions are compared, that asymmetry in
method is the reason to prefer the paired end-to-end numbers over either attribution.

## Correctness cross-check: Go K vs Rust DB vs the naive reference

Freshly regenerated from the same binaries used in the paired session, compared with
`bin/compare.exe` and cross-checked with `md5sum`:

| Comparison | Fixture | Dimensions | Differing pixels | Max delta |
|------------|---------|-----------|------------------|-----------|
| **Go K vs Rust DB** | typical | 2732 x 5830 | **0 / 15,927,560 (0.0000%)** | **0** |
| **Go K vs Rust DB** | big | 1366 x 5477 | **0 / 7,481,582 (0.0000%)** | **0** |
| Go K vs A (naive) | typical | 2732 x 5830 | 94,985 (0.5964%) | 1 |
| Go K vs A (naive) | big | 1366 x 5477 | **0 (0.0000%)** | **0** |
| Rust DB vs A (naive) | typical | 2732 x 5830 | 94,985 (0.5964%) | 1 |
| Rust DB vs A (naive) | big | 1366 x 5477 | **0 (0.0000%)** | **0** |

The two recommended implementations are **byte-identical to each other on both
fixtures** -- identical MD5, not merely a zero pixel diff. The Rust agent's earlier
finding that `typical_rsDB.pgm` matched Go D byte for byte therefore still holds for K,
as it must: K is bit-identical to D by construction.

Both are byte-identical to the naive reference on `big.png`, the factor-1 fixture,
which pins the decode, luma, min/max and contrast-stretch math as exactly equivalent
across both languages -- including the alpha premultiplication, which Go's `At()` path
performs implicitly and both optimized paths must perform explicitly.

The residual 94,985 pixels on `typical.png` are confined to the scaler, are all
**delta = 1** (one gray level), and are a property of the algorithm, not of either
language: A interpolates raw luma and then stretches, while K and DB stretch first and
interpolate the stretched 8-bit plane, rounding to `uint8` one step earlier. Both
optimized implementations make that choice identically, which is why they agree with
each other exactly and disagree with A identically.

## Part 3 verdict

- **Transform, optimized vs optimized: Go wins, by 1.51x (typical) and 1.26x (big).**
  Not 2.77x. That number is retracted.
- **PNG decode: Rust wins by 2.6-2.8x**, and that is a decoder-quality gap (`png` crate
  vs `image/png`), not a language gap. It is unchanged by `target-cpu=native`.
- **End to end: a tie on `typical.png`; Rust by 1.58x on `big.png`**, and the whole of
  the `big.png` deficit is decode. Go's decode is 144 ms of a 209 ms total there,
  against a 58 ms transform.
- **The lever that dominates both languages is not the language.** It is the PNG
  decode, and above it, the decision to hand this pipeline a PNG at all. Removing the
  PNG from the input side is worth more than every optimization in Parts 2 and 3
  combined, in either language.
- **Recommendation for this pipeline: Go variant K at `GOAMD64=v1`, no PGO.** It is
  byte-identical to the naive reference wherever the gate requires it, byte-identical
  to the Rust recommendation everywhere, and it keeps the existing toolchain. The
  transform is at its measured memory floor, so further transform work in either
  language is not where the remaining time is.


---

# Part 4: variant K on both sides, and Python as a third contestant

Part 3 corrected Parts 1 and 2 but repeated their structural error, inverted. Go
variant K carries the factor-2 exact-weight collapse (weights `1/4` and `3/4` become
shift-adds, worth ~1.4x on the transform); Rust DB did not have it, because K was
discovered after the Rust work was briefed. So Part 3's "Go wins transform by 1.51x"
compared Go-with-five-optimizations against Rust-with-four.

K has now been ported to Rust (`rust/src/main.rs`, `upscale_2x` / `upscale_k`) and the
same paired harness re-run. **Part 3's transform verdict is superseded by this
section.** Part 3's decode and end-to-end findings are unaffected — on `big.png` the
factor is 1, where K reduces to D and the two rounds measure the same code.

A Python contestant (`py/bench_py.py`) was added on the same 3-warmup + 15-timed
protocol and the same external correctness gate.

## Leaderboard, ms per image (median of 3 reps x 15 iters, single-threaded)

### typical.png (factor 2)

| impl | decode | transform | encode | total | exact vs A? |
|---|---|---|---|---|---|
| Rust K @ native | 14.17 | 46.41 | 9.87 | **70.27** | yes |
| Rust K @ lto | 13.62 | 53.16 | 9.20 | 75.80 | yes |
| Rust DS @ native (hand AVX2) | 13.84 | 64.09 | 9.63 | 87.76 | yes |
| Rust D @ lto | 13.88 | 76.59 | 10.00 | 100.25 | yes |
| Go K @ v3+PGO | 45.74 | 55.54 | 11.65 | 112.46 | **no (FMA)** |
| Go K @ v1 | 48.31 | 59.63 | 11.19 | 121.05 | yes |
| Go D @ v1 | 45.92 | 81.47 | 10.67 | 139.69 | yes |
| Python Pillow | 25.41 | 135.36 | 9.60 | 173.19 | delta 1 on 1.70% px |
| Rust C @ base (naive) | 16.44 | 238.64 | 10.20 | 265.93 | yes |
| Python numpy | 25.84 | 1005.78 | 10.77 | 1040.28 | delta 1 on 47 px |

### big.png (factor 1, no scaler in the path)

| impl | decode | transform | encode | total | exact vs A? |
|---|---|---|---|---|---|
| Rust C @ native | 50.11 | 47.86 | 4.97 | **104.63** | byte-exact |
| Rust K @ native | 48.87 | 56.43 | 4.66 | 110.07 | byte-exact |
| Python Pillow | 90.28 | 39.16 | 4.47 | 134.55 | delta 1 on 129 px |
| Go E @ v1 | 146.38 | 36.64 | 5.65 | 189.05 | delta 1 on 77 px |
| Go K/D @ v1 | 144.69 | 58.60 | 5.59 | 211.16 | byte-exact |

## Paired, interleaved, order-shuffled (median of per-pair ratios, sign count)

| pair | decode | transform | total |
|---|---|---|---|
| Go K vs Rust K, typical | Rust 3.34x (7/7) | Rust 1.25x (7/7) | **Rust 1.67x (7/7)** |
| Go K vs Rust K, big | Rust 2.76x (7/7) | Go 1.03x (2/7 -- parity) | **Rust 1.80x (7/7)** |
| Go K vs Python Pillow, typical | Python 1.84x (5/5) | Go 2.34x (5/5) | Go 1.44x (5/5) |
| Go K vs Python Pillow, big | Python 1.46x (5/5) | Python 1.39x (5/5) | Python 1.41x (5/5) |

Go K and Rust K produce byte-identical output (0 / 15,927,560 differing), which is what
makes the timing comparison legitimate.

## What changed, and what did not

- **Transform, K on both sides: Rust by 1.25x on `typical`, parity on `big`** (2/7
  pairs, inside noise). Part 3's "Go by 1.51x" was the missing K, and almost nothing
  else. Both the 2.77x of Part 1 and the 1.51x of Part 3 are retracted.
- **The transform gap is the small part.** End to end Rust wins 1.67x / 1.80x, and the
  whole of it is decode: the `png` crate is 2.76-3.34x faster than `image/png`, 7/7
  pairs on both fixtures. Part 3 reached this conclusion and it stands.
- **`GOAMD64=v3` still moves 80 pixels on `big.png`** via FMA contraction; Rust's
  `-C target-cpu=native` is bit-identical on both fixtures while being the larger win.
  Same flag category, opposite safety, because Rust forbids FP contraction by default.
- **The SIMD negative result is now cross-validated from the other side.** Rust's
  hand-written AVX2 vertical pass (DS) loses to scalar K, 64.09 ms against 46.41 ms on
  `typical` -- 38% slower. Explicit intrinsics in a language with first-class SIMD lose
  to getting the algorithm right.

## Python

Pillow beats fully-optimized Go end to end on `big.png` by 1.41x (5/5 pairs) and beats
Go's decode on both fixtures (1.46-1.84x). Go's `image/png` is the weakest single
component in this entire comparison -- slower than libpng and slower than the Rust
crate. Python loses `typical.png` (Go by 1.44x total) only because `resize(BILINEAR)`
is a general support-based filter and cannot exploit the integer factor the way K does.

Interpreter cost, for scale: the pure-Python probe runs at ~1350 ns per output pixel,
which extrapolates to ~21.5 s on `typical.png`, ~360x slower than Go K. Every Python
number above is C code driven by roughly five interpreter statements per image. numpy
is not the fast answer here (1006 ms): whole-array fancy-indexing over 15.9M float64
pixels loses to Pillow's fused C loops.

## Part 4 verdict

1. **Stop decoding PNG at the producer.** Worth **87-92% of decode** as measured in the
   handoff-format ladder below (not the ~95% estimated in Part 2, which omitted the
   file-read syscall), which is still larger than every other finding in this document
   combined.
2. **If the format is fixed, Go's decoder is the problem.** Rust and Pillow both solve
   it today; no amount of transform work in Go can reach it.
3. **Ship variant K regardless of language.** A free 1.4-1.5x on the transform in both
   Go and Rust, with byte-identical output.

## The handoff-format ladder, measured

Part 4's first recommendation is "stop decoding PNG at the producer". It was measured
rather than asserted, and **the cheap-looking version of it does not work.**

Decode share of total, for scale: Go 45.3 ms (37%) on `typical`, 143.5 ms (**69%**) on
`big` -- more than twice the transform that Parts 2-4 spent their effort on.

| handoff format | file size (big) | Go decode | saved vs PNG |
|---|---|---|---|
| PNG as-is | 2.6 MB | 143.5 ms | -- |
| PNG, `compress_level=0` | 22.5 MB | 92.7 ms | 35% |
| BMP, uncompressed | 22.5 MB | 40.6 ms | 56% |
| raw bytes, no container | 22.4 MB | **12.0 ms** | **92%** |
| raw bytes, `typical` | 11.9 MB | 6.1 ms | 87% |

**Turning DEFLATE off while keeping the PNG container recovers only a third**, because
the row filters, the adler32 pass and the row-by-row buffer plumbing all survive while
the file inflates 8.7x. The win needs the container gone too.

Why decode is expensive, from the Go profile on `big.png`: `filterPaeth` 16.5% flat plus
`png.abs` 5.3% (Paeth predicts each byte from its neighbours, a loop-carried dependency
on every byte -- unvectorizable), `flate.huffmanBlock`/`huffSym`/`dictDecoder` ~15%
(bit-serial Huffman), `adler32.update` 6.3%. None of it is the pipeline's own
computation; it is undoing a compression the producer just paid more CPU to apply.

The trade is size: raw is 8.7x bigger on `big` and 23x on `typical` (510 KB -> 11.9 MB).
Same machine, temp file, pipe or shared memory -- take it. Crosses a network or gets
archived -- PNG stays, and the lever is then Go's decoder specifically, which the `png`
crate beats by 2.8-3.3x and libpng by 1.5-1.8x on the identical file.

**The stronger form: hand over raw 8-bit gray, not RGBA.** The consumer's first act is
to discard colour, so a quarter of the bytes (7.5 MB on `big` -- smaller than the
level-0 PNG *and* faster than every row above) also deletes the luma pass, currently the
largest remaining item in the profile at 25.9% flat. This is the same move already made
on the output side, where PNG -> PGM saved 131 ms of encode; on the input side it is
worth more.

### The producer, identified -- and why the top recommendation does NOT apply to it

The producer was named as unknown above. It has since been identified, by read-only
inspection of the consuming repository (no files written there, no git run):

- **Producer: Chromium, driven by Playwright.** `captureScreenshot` in
  `tests/webapp/regression/goregression/browser.go:3498` calls
  `page.Screenshot(PageScreenshotOptions{Path: ..., FullPage: true})`, which writes a
  full-page PNG to disk.
- **Consumer: the goregression visual/OCR gate.** `runTesseract`
  (`image_heuristics.go:1446`) takes the already-decoded image, calls
  `preprocessForOCR(img, ocrUpscaleFactorFor(img.Bounds()))`, writes a PGM, and hands
  that path to tesseract. This is a CI test gate, not a production request path.

**This kills recommendation 1 for this caller.** "Hand over raw pixels instead of PNG"
assumes the producer is code we control. It is not -- it is a browser, and the Chrome
DevTools Protocol's `Page.captureScreenshot` returns **PNG or JPEG only**. There is no
raw-pixel option to switch to, so the 87-92% decode saving measured above is
**unreachable here**, however real it is in the abstract. The ladder stands as a
general result about handoff formats; it does not stand as advice to this consumer.

What *is* available to this caller, in descending order of safety:

1. **A faster PNG decoder.** The gap is measured and large: the Rust `png` crate is
   2.8-3.3x faster than `image/png` and libpng 1.5-1.8x, on identical bytes. This is
   the one lever that needs no change to the producer and no change to the pixels.
2. **Variant K**, which is free and byte-identical -- though note the real path runs at
   factor 3 for small captures, where K falls back to the general scaler; the factor-2
   collapse only fires when the pixel budget forces factor 2.
3. **JPEG capture** (`Type: "jpeg"`) would decode far faster, but it is lossy: it
   changes the pixels tesseract sees, and this gate has determinism tests. Not
   recommended without re-baselining, and probably not at all.

Two independent confirmations that this benchmark models the real path faithfully:
the consumer already writes a **PGM** rather than a PNG for the tesseract handoff,
which is exactly the encode-side finding of Part 1; and it already fixed a
decode-twice bug (a pprof run there attributed 73.3 s to four `png.Decode` call sites),
which is the same decode-dominance this document measures from the other end.
