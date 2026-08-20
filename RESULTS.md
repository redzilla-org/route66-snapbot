# Results

Measured 2026-08-20 on Windows 11 (x86-64), Go 1.27.0, cargo 1.93.1 (rustc release
profile, `opt-level = 3`, `codegen-units = 1`). Every variant is single-threaded.
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
