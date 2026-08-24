#!/usr/bin/env python3
"""Python side of the image-preprocessing comparison.

Same pipeline as the Go and Rust harnesses -- PNG decode -> luma grayscale ->
global min/max contrast stretch -> integer-factor bilinear upscale -> binary PGM
write -- and the same measurement protocol: 3 warmups, then a 250 ms timed
window, phase-timed into decode / transform / encode, single-threaded.

Three variants, because "how fast is Python" has three genuinely different
answers and collapsing them into one number would be dishonest:

  PIL     Idiomatic Pillow. convert("L") + ImageOps.autocontrast + resize
          (BILINEAR). Every pixel loop runs in Pillow's C code; the Python
          interpreter executes ~5 statements per image. This is what a Python
          developer would actually write, and it is the fair Python entry.

  NUMPY   A faithful port of variant A's arithmetic, vectorized with numpy.
          Exists because Pillow's stretch and resampling are NOT the reference
          algorithm (see the correctness notes below), so PIL alone cannot say
          whether Python is slow or merely different. NUMPY computes the SAME
          numbers as the reference, in whole-array operations.

  PURE    The reference algorithm as a literal per-pixel Python loop, on a
          deliberately tiny crop. It is not run on the full fixtures -- it would
          take minutes per iteration -- and it exists ONLY to quantify the
          interpreter's per-pixel cost, which is the number that explains why
          the other two variants are written the way they are.

Correctness is judged by the same external compare tool as the other languages,
against variant A's output.
"""
import argparse
import json
import statistics
import time
from pathlib import Path

from PIL import Image, ImageOps
import numpy as np

ROOT = Path(__file__).resolve().parent.parent

OCR_UPSCALE_FACTOR = 3
OCR_UPSCALE_PIXEL_BUDGET = 20_000_000


def upscale_factor_for(w, h):
    """Identical pixel-budget rule to the Go and Rust harnesses."""
    px = w * h
    if px <= 0:
        return OCR_UPSCALE_FACTOR
    for f in range(OCR_UPSCALE_FACTOR, 1, -1):
        if px * f * f <= OCR_UPSCALE_PIXEL_BUDGET:
            return f
    return 1


# ---------------------------------------------------------------- PIL variant


def transform_pil(img, scale):
    """Idiomatic Pillow: three library calls, no Python-level pixel access.

    NOTE this is NOT the reference algorithm, and the differences are inherent
    to using the library rather than bugs to be fixed:
      * convert("L") uses the same 299/587/114 coefficients but rounds to an
        integer per pixel BEFORE the stretch, so the stretch operates on
        already-quantized luma.
      * autocontrast(cutoff=0) is a min/max stretch, but it is computed from
        the 256-bin histogram of that quantized luma and applied through a
        256-entry LUT -- the same class of tie-rounding difference the Go LUT
        ablation (variant E) documented.
      * resize(BILINEAR) is a support-based triangle filter over the whole
        image, not the 4-tap kernel with clamped edges the reference uses.
    """
    g = img.convert("L")
    g = ImageOps.autocontrast(g, cutoff=0)
    if scale > 1:
        g = g.resize((g.width * scale, g.height * scale), Image.BILINEAR)
    return g


# -------------------------------------------------------------- NUMPY variant


def transform_numpy(img, scale):
    """Faithful port of variant A's arithmetic in whole-array numpy operations.

    Every step matches the reference: float64 luma with the exact coefficients,
    a global min/max stretch over the UNQUANTIZED luma, and a 4-tap bilinear
    kernel with clamped edges evaluated at the same sample positions. The only
    thing that changes is that the loops live in numpy's C code.
    """
    a = np.asarray(img.convert("RGB"), dtype=np.float64)
    lum = 0.299 * a[:, :, 0] + 0.587 * a[:, :, 1] + 0.114 * a[:, :, 2]

    lo, hi = lum.min(), lum.max()
    span = hi - lo
    if span < 1e-6:
        span = 1.0
    st = np.clip((lum - lo) / span * 255.0, 0.0, 255.0)

    h, w = st.shape
    if scale <= 1:
        return (st + 0.5).astype(np.uint8)

    # Sample positions are the reference's (o + 0.5)/scale - 0.5, and the taps
    # are clamped to the edge exactly as sample() does.
    def axis(n):
        s = (np.arange(n * scale) + 0.5) / scale - 0.5
        i0 = np.floor(s).astype(np.int64)
        f = s - i0
        return np.clip(i0, 0, n - 1), np.clip(i0 + 1, 0, n - 1), f

    x0, x1, fx = axis(w)
    y0, y1, fy = axis(h)

    # Horizontal then vertical, so the intermediate is (h x ow) rather than
    # materializing four full-size tap planes at once.
    top = st[:, x0] * (1.0 - fx) + st[:, x1] * fx
    out = top[y0, :] * (1.0 - fy)[:, None] + top[y1, :] * fy[:, None]
    return (out + 0.5).astype(np.uint8)


# --------------------------------------------------------------- PURE variant


def transform_pure(img, scale):
    """The reference algorithm as a literal Python loop. Interpreter cost probe."""
    px = img.convert("RGB").load()
    w, h = img.size
    gray = [0.0] * (w * h)
    lo, hi = 1e308, -1e308
    for y in range(h):
        for x in range(w):
            r, g, b = px[x, y]
            l = 0.299 * r + 0.587 * g + 0.114 * b
            gray[y * w + x] = l
            if l < lo:
                lo = l
            if l > hi:
                hi = l
    span = hi - lo
    if span < 1e-6:
        span = 1.0
    ow, oh = w * scale, h * scale
    out = bytearray(ow * oh)
    for oy in range(oh):
        sy = (oy + 0.5) / scale - 0.5
        y0 = int(sy // 1)
        fy = sy - y0
        y0c = min(max(y0, 0), h - 1)
        y1c = min(max(y0 + 1, 0), h - 1)
        for ox in range(ow):
            sx = (ox + 0.5) / scale - 0.5
            x0 = int(sx // 1)
            fx = sx - x0
            x0c = min(max(x0, 0), w - 1)
            x1c = min(max(x0 + 1, 0), w - 1)
            t = gray[y0c * w + x0c] * (1 - fx) + gray[y0c * w + x1c] * fx
            bt = gray[y1c * w + x0c] * (1 - fx) + gray[y1c * w + x1c] * fx
            v = (t * (1 - fy) + bt * fy - lo) / span * 255
            out[oy * ow + ox] = int(min(max(v, 0.0), 255.0) + 0.5)
    return ow, oh, bytes(out)


# --------------------------------------------------------------------- driver


def write_pgm(path, w, h, data):
    with open(path, "wb") as f:
        f.write(b"P5\n%d %d\n255\n" % (w, h))
        f.write(data)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("variant", choices=["PIL", "NUMPY", "PURE"])
    ap.add_argument("input")
    ap.add_argument("output")
    ap.add_argument("--warm", type=int, default=3)
    ap.add_argument("--seconds", type=float, default=0.25)
    ap.add_argument("--crop", type=int, default=0, help="PURE only: crop to NxN first")
    args = ap.parse_args()

    raw = Path(args.input).read_bytes()
    runs = []
    ow = oh = scale = 0
    i = 0
    timed_start = None
    while True:
        import io as _io

        t0 = time.perf_counter()
        if i == args.warm:
            timed_start = t0
        img = Image.open(_io.BytesIO(raw))
        img.load()  # Pillow is lazy; force the decode inside the decode phase
        t1 = time.perf_counter()

        if args.crop:
            img = img.crop((0, 0, args.crop, args.crop))
        scale = upscale_factor_for(*img.size)

        if args.variant == "PIL":
            g = transform_pil(img, scale)
            ow, oh, data = g.width, g.height, g.tobytes()
        elif args.variant == "NUMPY":
            arr = transform_numpy(img, scale)
            oh, ow = arr.shape
            data = arr.tobytes()
        else:
            ow, oh, data = transform_pure(img, scale)
        t2 = time.perf_counter()

        write_pgm(args.output, ow, oh, data)
        t3 = time.perf_counter()

        if i >= args.warm:
            runs.append((t1 - t0, t2 - t1, t3 - t2, t3 - t0))
            if t3 - timed_start >= args.seconds:
                break
        i += 1

    def col(k):
        v = sorted(r[k] * 1000.0 for r in runs)
        return {"min": v[0], "med": statistics.median(v), "mean": statistics.fmean(v)}

    print(json.dumps({
        "variant": args.variant, "input": args.input, "scale": scale,
        "outW": ow, "outH": oh, "iters": len(runs),
        "decode": col(0), "transform": col(1), "encode": col(2), "total": col(3),
        "outBytes": Path(args.output).stat().st_size,
    }))


if __name__ == "__main__":
    main()
