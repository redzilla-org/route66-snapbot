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
