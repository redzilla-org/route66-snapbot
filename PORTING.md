# Porting guide: the optimized preprocessor into the real OCR path

**Target.** `preprocessForOCR` in `tests/webapp/regression/goregression/image_heuristics.go`
(called from `runTesseract`, which passes `ocrUpscaleFactorFor(img.Bounds())` as `scale`).

**Status of the target.** It is still variant **A**, the naive implementation this
benchmark's variant A is a faithful copy of. Everything measured in `RESULTS.md` is
therefore available to it; nothing has been applied yet.

## What it is worth

Transform phase only, median of 15 iterations after 3 warmups, `GOAMD64=v1`, on real
captured screenshots. The encode column is excluded because the real caller already
writes PGM (`writePreprocessedPGM`), which is the Part 1 encode finding already in place.

| capture | factor | A (today) | N (this port) | saved per image |
|---|---|---|---|---|
| `small` 1366x1400 | 3 | 354.90 ms | **50.26 ms** | **-304.6 ms (7.1x)** |
| `typical` 1366x2915 | 2 | 384.30 ms | **61.09 ms** | **-323.2 ms (6.3x)** |
| `big` 1366x5477 | 1 | 368.89 ms | **62.19 ms** | **-306.7 ms (5.9x)** |

Multiply by the number of OCR'd captures per gate run for the wall-clock effect. Be
honest about the ceiling: this is the *preprocessing* only. Tesseract itself dominates
the OCR gate, and PNG decode (149 ms on `big`) is untouched by this port.

## What to port

Five functions and one dispatch, all pure and dependency-free (`image` + `math` only).
Copy from `go/fast.go` and `go/faster.go` in this repo:

| from | what it does | required |
|---|---|---|
| `grayStretchNoBuf` | luma + global min/max stretch, no `[]float64` side buffer | yes |
| `bilinearAxis` | per-output-column `(i0, i1, weight)` tables | yes (used by the general scaler) |
| `bilinearRuns` | collapses the axis tables into runs of constant `(i0,i1)` | yes (factor 3) |
| `upscaleBilinear2x` | factor-2 shift-and-add kernel | yes (factor 2) |
| `upscaleBilinearRunsExact` | factor-3+ run-structured exact-rational kernel | yes (factor 3) |
| `upscaleBilinearSeparable` | general 16.16 separable scaler | yes (fallback) |

Then replace the body of `preprocessForOCR` with the dispatch, preserving its existing
guard clauses:

```go
func preprocessForOCR(img image.Image, scale int) *image.Gray {
	if scale < 1 {
		scale = 1
	}
	b := img.Bounds()
	if b.Dx() == 0 || b.Dy() == 0 {
		return image.NewGray(image.Rect(0, 0, 0, 0))
	}
	small := grayStretchNoBuf(img)
	switch {
	case scale <= 1:
		return small // factor 1 is a no-op, not a degenerate bilinear
	case scale == 2:
		return upscaleBilinear2x(small)
	case scale == 3:
		return upscaleBilinearRunsExact(small, scale)
	default:
		return upscaleBilinearSeparable(small, scale)
	}
}
```

The per-factor split is not premature: no single kernel is fastest everywhere, and the
gaps are large (K beats M by 1.34x at factor 2; M beats the general scaler by 1.12x at
factor 3, 9/9 pairs each). `ocrUpscaleFactorFor` only ever returns 1, 2 or 3, so the
`default` arm is dead code kept for safety.

## Correctness: what changes, exactly

The output is **not** byte-identical to today's, and the reason is structural, not a bug:
variant A stretches AFTER interpolation, the optimized path stretches BEFORE. Both are
the same affine map; they round at different points.

| capture | factor | differing px vs A | max abs delta |
|---|---|---|---|
| `big` | 1 | **0 / 7,481,582 (byte-exact)** | 0 |
| `typical` | 2 | 94,985 / 15,927,560 (0.596%) | **1** |
| `small` | 3 | 55,882 / 17,211,600 (0.325%) | **1** |

No pixel anywhere moves by more than one gray level, and at factor 1 -- the largest
captures -- the output is byte-identical.

**Determinism is preserved**, which is the property the target's doc comment promises
and its tests assert: every kernel here is integer or deterministic float, with no
parallelism, no map iteration and no allocation-address dependence. The same PNG yields
byte-identical pixels every run. Verify with the existing determinism test rather than
taking this paragraph's word for it.

**The risk to check is the OCR gate's keyword floor, not the pixels.** A one-level gray
change is very unlikely to alter tesseract's output, but "unlikely" is not "verified":
run the gate and compare the extracted text, not just the images.

## Gate to run before merging

```
# 1. pixel gate, per fixture, against the CURRENT implementation's output.
#    Build the comparer from this repo: cd go && go build -o ../bin/compare.exe ./compare
bin/compare.exe  before.pgm  after.pgm        # expect maxAbsDelta <= 1, and 0 at factor 1

# 2. the target's own determinism test -- it already exists and already asserts
#    byte-identical pixel buffers across two runs (image_heuristics_test.go:772)
go test ./tests/webapp/regression/goregression -run TestPreprocessForOCRUpscalesDeterministically

# 3. the OCR gate itself -- the only test that can see a keyword regression
```

## Caveats that will bite

1. **The fast path is `*image.RGBA` only.** `grayStretchNoBuf` type-switches on
   `*image.RGBA` and falls back to the slow generic path otherwise. This repo's
   fixtures are real captures and decode to `*image.RGBA`, but a screenshot WITH an
   alpha channel decodes to `*image.NRGBA`, silently taking the fallback and giving up
   most of the win. **Assert the concrete type once in the port** (log or test it) --
   do not assume. Adding an `*image.NRGBA` arm is mechanical (premultiply as
   `grayStretchFast` already does) if the real captures turn out to carry alpha.

2. **Do NOT set `GOAMD64=v3`.** It is worth ~4% and it silently changes pixels: the
   compiler contracts the luma expression into `VFMADD231SD`, moving 80 of 7,481,582
   pixels on `big.png` and destroying the byte-exactness at factor 1. Not worth it on a
   gate with determinism and baseline tests.

3. **Do not bother with PGO.** Its entire effect on this program is two devirtualizations
   inside `compress/flate`; zero decisions in the pipeline code. Measured effect flips
   sign between fixtures.

4. **Do not parallelize the transform.** The real consumer already runs many images
   concurrently (`ocrqueue.go`); intra-image parallelism would oversubscribe the box.

5. **Do not write SIMD for this.** Measured, twice, from both sides: variant K reaches
   the arithmetic-free memory floor in scalar Go (K vs J: 3/7 sign count, a coin flip),
   and Rust's hand-written AVX2 vertical pass is 38% SLOWER than the scalar factor-2
   kernel.

## What NOT to port

- **Variant E / `grayStretchLUT`** -- fastest stretch measured, but it cannot reproduce
  A's per-triple float rounding (77 pixels at factor 1, all exact half-way ties). It
  would break the byte-exactness that factor 1 currently has.
- **Variants G, H, I, L** -- refuted or superseded; kept in the repo as attribution.
- **Variant J** -- deliberately wrong output, a measurement probe only.

## After this port, the remaining cost is decode

At factor 1, decode is 149 ms of a 213 ms total (**70%**), and it is `image/png`, which
is 2.8-3.3x slower than Rust's `png` crate and 1.5-1.8x slower than libpng on identical
bytes. No further transform work can reach it. The producer is Chromium via Playwright,
which can only emit PNG or JPEG, so the "hand over raw pixels" fix measured in
`RESULTS.md` is NOT available to this caller. See the end of `RESULTS.md` for the
options that are.
