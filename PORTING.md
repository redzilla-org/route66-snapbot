# Handoff: replace `preprocessForOCR` with the optimized version

**Recommendation: do steps 1-5. Do not do anything in "Do not do this".**

Everything below is measured, not estimated. Full evidence in `RESULTS.md`.

---

## What and why, in three lines

`preprocessForOCR` in `tests/webapp/regression/goregression/image_heuristics.go` is
still the original naive implementation. A drop-in replacement runs the same pipeline
**6-7x faster**, saving **~300 ms per OCR'd screenshot**. The replacement file is
written, compiled, and pixel-verified: `port/ocr_preprocess.go`.

| capture | factor | today | after | saved |
|---|---|---|---|---|
| 1366x1400 | 3 | 354.90 ms | **50.26 ms** | -304.6 ms (7.1x) |
| 1366x2915 | 2 | 384.30 ms | **61.09 ms** | -323.2 ms (6.3x) |
| 1366x5477 | 1 | 368.89 ms | **62.19 ms** | -306.7 ms (5.9x) |

Transform phase only, median of 15 iterations after 3 warmups, `GOAMD64=v1`, real
captured screenshots. Encode is excluded because the caller already writes PGM.

---

## Step 1 - copy the file

Copy `port/ocr_preprocess.go` into `tests/webapp/regression/goregression/`.

It is self-contained: stdlib `image` + `math`, no new dependencies, nothing else in the
package is referenced.

## Step 2 - fit it to the package

1. Change `package ocrpre` to the package name used in that directory.
2. Unexport `PreprocessForOCR` -> `preprocessForOCR`.
3. Delete the **body** of the existing `preprocessForOCR` in `image_heuristics.go`
   (keep its doc comment; the semantics it describes are unchanged and still true).
4. If any helper name collides with an existing one in the package, rename the one in
   the new file. Nothing outside the file calls them.

No call sites change. The signature is identical: `(img image.Image, scale int) *image.Gray`.

## Step 3 - verify the pixels

Expect the output to change by at most 1 gray level, and not at all at factor 1. This
is structural, not a bug: the original stretches AFTER interpolation, the fast path
stretches BEFORE. Both are the same affine map rounded at a different point.

| capture | factor | differing px | max delta |
|---|---|---|---|
| 1366x5477 | 1 | **0 / 7,481,582 (byte-exact)** | 0 |
| 1366x2915 | 2 | 94,985 / 15,927,560 (0.596%) | **1** |
| 1366x1400 | 3 | 55,882 / 17,211,600 (0.325%) | **1** |

To reproduce that check on your own capture, build the comparer from this repo and diff
a before/after PGM pair:

```
cd go && go build -o ../bin/compare.exe ./compare
bin/compare.exe before.pgm after.pgm      # expect maxAbsDelta <= 1
```

## Step 4 - run the gates

```
# determinism -- already exists, already asserts byte-identical pixels across two runs
go test ./tests/webapp/regression/goregression -run TestPreprocessForOCRUpscalesDeterministically

# the OCR gate itself -- the ONLY test that can see a keyword regression
```

The pixels are the easy part. **The thing to actually verify is that the extracted OCR
text is unchanged**, because the gate scores keywords, not images. A 1-level gray shift
is very unlikely to change tesseract's output, but "unlikely" is not "verified".

## Step 5 - confirm the fast path is live

The optimization only fires on `*image.RGBA` and `*image.NRGBA`. Anything else silently
falls back to a correct-but-slow generic path with **no error anywhere**, and you get
none of the win.

Screenshots without alpha decode to `*image.RGBA`; **with** alpha they decode to
`*image.NRGBA`. Both are handled. Confirm which one you actually have — log `%T` once,
or assert it in a test. If it is ever something else, add an arm rather than shipping
the fallback.

---

## Do not do this

| Don't | Why |
|---|---|
| Set `GOAMD64=v3` | Worth ~4%, but the compiler contracts the luma expression into `VFMADD231SD`, which moves 80 of 7,481,582 pixels and **destroys the factor-1 byte-exactness**. Not worth it on a gate with determinism tests. |
| Enable PGO | Its entire measured effect on this program is two devirtualizations inside `compress/flate`. Zero decisions in pipeline code; effect flips sign between fixtures. |
| Parallelize the transform | `ocrqueue.go` already runs many images concurrently. Intra-image parallelism would oversubscribe the box and hurt aggregate throughput. |
| Write SIMD for it | Measured dead from both sides: the factor-2 kernel already sits at the arithmetic-free memory floor (coin-flip sign count), and a hand-written AVX2 pass in Rust was 38% **slower** than the scalar kernel. |
| Port the LUT stretch | Fastest stretch measured, but it cannot reproduce the original's per-triple float rounding (77 pixels at factor 1) and would break the byte-exactness factor 1 currently has. |

---

## What this does not fix

After this port, the remaining cost at factor 1 is **PNG decode: 149 ms of a 213 ms
total (70%)**, and it is `image/png` — 2.8-3.3x slower than Rust's `png` crate and
1.5-1.8x slower than libpng on identical bytes. No transform work can reach it.

The usual fix (have the producer hand over raw pixels instead of PNG) is **not available
here**: the producer is Chromium via Playwright, and the DevTools protocol emits only
PNG or JPEG. JPEG would decode far faster but is lossy, changing what tesseract sees on
a gate with determinism tests — not recommended.

So: take the transform win now; treat the decoder as a separate, larger, and harder
piece of work.

---

## Provenance

- Replacement source, compiled and verified: `port/ocr_preprocess.go`
- Verifier (proves the file above is byte-identical to the benchmarked variant):
  `port/cmd/portcheck/main.go`
- Full measurement record, every variant and negative result: `RESULTS.md`
- Benchmark harness and all variants: `go/`

The paste-ready file was compiled standalone and run over all four fixtures; its output
is byte-identical (0 differing pixels) to the variant these timings were measured on,
including the alpha/NRGBA case.
