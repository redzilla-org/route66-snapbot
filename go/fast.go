// Variant D and its ablations: the pprof-guided hand optimization of B2.
//
// The B2 profile (prof/b2_*.pprof) attributes the transform to exactly two
// functions: upscaleBilinearGray (65.4% of all samples on typical.png) and
// grayStretchFast (11.3% there, 27.5% on big.png where there is no scaler).
// Everything below targets those two, and nothing else.
//
// Three structural changes:
//
//  1. grayStretchNoBuf removes the []float64 luma buffer entirely. B2 allocates
//     w*h float64s (9.8 MB on big.png) purely so that pass 2 can re-read the
//     luma pass 1 computed -- 8 bytes written and 8 read per pixel, plus the
//     allocation and the runtime zeroing of it (visible in the profile as
//     memclrNoHeapPointers, 3.7-5.4%), to avoid three float multiplies.
//     Recomputing is cheaper than storing: pass 1 keeps only min/max, pass 2
//     recomputes the identical expression. Every arithmetic operation is
//     unchanged, so the output stays bit-for-bit A.
//
//     grayStretchLUT (variant E) goes further -- exact integer luma
//     L = 299r+587g+114b, and a byte LUT indexed by L-minL that replaces the
//     whole per-pixel stretch with one load. Its only measured pixel differences
//     are accepted delta-1 half-way ties; see the comment there.
//
//  2. bilinearAxis exploits the fact that the upscale factor is always an
//     integer. The sample position sx = (ox+0.5)/f - 0.5 has only f distinct
//     fractional parts, so the entire per-output-column (x0, x1, weight) triple
//     is identical on every row. Precomputing it once per image removes the
//     float division, math.Floor, two clamps and the weight subtraction from
//     the inner loop -- for typical.png that is 15.9 million repetitions of
//     work with 2732 distinct answers. What is left runs in 16.16 fixed point.
//     Measured on its own: 2.18x on the transform, the largest single win.
//
//  3. upscaleBilinearSeparable splits the 4-tap kernel into a horizontal pass
//     and a vertical pass. This was the change least expected to help -- the
//     intermediate buffer is 31.8 MB on typical.png -- and it is worth a
//     further 22%, because the vertical pass then walks two already-filtered
//     rows sequentially instead of gathering four taps per output pixel.
//
// Every claim above is a measurement, not an argument; see RESULTS.md Part 2
// for the per-hypothesis attribution and the noise band it has to clear.
package main

import (
	"image"
	"math"
)

// ---- integer luma ----

// lumaMilli is the exact 1000x-scaled luma. 0.299, 0.587 and 0.114 are exact
// thousandths, so this integer carries no rounding error at all, unlike the
// float64 expression it replaces.
const (
	lumaR = 299
	lumaG = 587
	lumaB = 114
)

// grayStretchLUT is the pass-1/pass-2 replacement for grayStretchFast. It makes
// two passes over the SOURCE pixels rather than one pass over the source and one
// over a float64 side buffer; the source is 4 bytes per pixel, so even read
// twice that is less memory traffic than 4 bytes read plus 8 written plus 8 read.
func grayStretchLUT(img image.Image) *image.Gray {
	b := img.Bounds()
	w, h := b.Dx(), b.Dy()
	out := image.NewGray(image.Rect(0, 0, w, h))

	src, ok := img.(*image.RGBA)
	if !ok {
		// Only the fixture's concrete type gets the fast path; anything else
		// falls back to the already-correct B implementation rather than
		// growing a second copy of every branch.
		return grayStretchFast(img)
	}

	minL, maxL := uint32(1<<31-1), uint32(0)
	base := (b.Min.Y-src.Rect.Min.Y)*src.Stride + (b.Min.X-src.Rect.Min.X)*4
	for y := 0; y < h; y++ {
		// Reslicing to an exact-length row is what lets the compiler prove the
		// x*4+3 indexes are in range; a flat pix[y*stride+x*4] form does not.
		row := src.Pix[base+y*src.Stride : base+y*src.Stride+w*4]
		for x := 0; x+4 <= len(row); x += 4 {
			l := lumaR*uint32(row[x]) + lumaG*uint32(row[x+1]) + lumaB*uint32(row[x+2])
			if l < minL {
				minL = l
			}
			if l > maxL {
				maxL = l
			}
		}
	}

	// The stretch is a monotone function of L over a bounded integer range, so
	// it collapses to a byte table. Built once per image, at most 255001 entries.
	// Filled by EXACT integer arithmetic, no float anywhere in the stretch:
	// floor(i*255/span + 1/2) == (2*i*255 + span) / (2*span).
	//
	// The LUT cannot be byte-identical to A at factor 1: on big.png it differs on
	// 77 of 7,481,582 pixels, and all 77
	// are values whose exact stretched result is a precise x.5 (verified tie
	// remainder 255000/510000 on every one), where A's float64 luma lands a hair
	// below the tie and truncates down. Breaking ties downward instead does not
	// fix it -- that flips a different 180 pixels the other way, for 257 total --
	// because A's error direction depends on the individual (r,g,b) triple, not
	// on L. No table indexed by L alone can reproduce per-triple float noise.
	// The product gate accepts these sparse delta-1 half-way ties.
	span := uint64(maxL - minL)
	if span == 0 {
		span = 1
	}
	lut := make([]uint8, int(maxL-minL)+1)
	for i := range lut {
		v := (2*uint64(i)*255 + span) / (2 * span)
		if v > 255 {
			v = 255
		}
		lut[i] = uint8(v)
	}

	for y := 0; y < h; y++ {
		row := src.Pix[base+y*src.Stride : base+y*src.Stride+w*4]
		orow := out.Pix[y*out.Stride : y*out.Stride+w]
		for x := range orow {
			p := row[x*4 : x*4+3 : x*4+3]
			l := lumaR*uint32(p[0]) + lumaG*uint32(p[1]) + lumaB*uint32(p[2])
			orow[x] = lut[l-minL]
		}
	}
	return out
}

// grayStretchNoBuf is the variant-D stretch. It applies hypothesis 1 -- delete
// the w*h []float64 -- WITHOUT changing a single arithmetic operation: pass 1
// computes A's exact float64 luma and keeps only min/max, pass 2 recomputes the
// identical expression and applies A's exact stretch. Recomputing three
// multiplies is cheaper than storing and reloading 8 bytes per pixel, and the
// result is bit-for-bit A.
//
// The integer/LUT stretch in grayStretchLUT is faster still and has an accepted
// maximum delta of one gray level -- see the comment there.
func grayStretchNoBuf(img image.Image) *image.Gray {
	b := img.Bounds()
	w, h := b.Dx(), b.Dy()
	out := image.NewGray(image.Rect(0, 0, w, h))

	// TWO fast paths, not one. A screenshot WITHOUT an alpha channel decodes to
	// *image.RGBA; one WITH alpha decodes to *image.NRGBA. Handling only the
	// former means an alpha-carrying capture silently takes the generic At()
	// fallback and loses most of the win with no error anywhere -- verified on
	// fixtures/alpha.png, which reports *image.NRGBA.
	//
	// NRGBA's Pix is NOT alpha-premultiplied while At().RGBA() premultiplies, so
	// the NRGBA arm premultiplies with the SAME integer division the reference
	// path performs, keeping both arms bit-identical to variant A.
	var pix []byte
	var stride, base int
	premul := false
	switch src := img.(type) {
	case *image.RGBA:
		pix, stride = src.Pix, src.Stride
		base = (b.Min.Y-src.Rect.Min.Y)*src.Stride + (b.Min.X-src.Rect.Min.X)*4
	case *image.NRGBA:
		pix, stride, premul = src.Pix, src.Stride, true
		base = (b.Min.Y-src.Rect.Min.Y)*src.Stride + (b.Min.X-src.Rect.Min.X)*4
	default:
		return grayStretchFast(img)
	}

	// lumaOf is the one place the two arms differ; it is a tiny leaf and inlines.
	lumaOf := func(p []byte) float64 {
		r8, g8, b8 := p[0], p[1], p[2]
		if premul {
			a := uint32(p[3])
			r8 = uint8(uint32(p[0]) * a / 255)
			g8 = uint8(uint32(p[1]) * a / 255)
			b8 = uint8(uint32(p[2]) * a / 255)
		}
		return 0.299*float64(r8) + 0.587*float64(g8) + 0.114*float64(b8)
	}

	minL, maxL := math.MaxFloat64, -math.MaxFloat64
	for y := 0; y < h; y++ {
		row := pix[base+y*stride : base+y*stride+w*4]
		for x := 0; x+4 <= len(row); x += 4 {
			l := lumaOf(row[x : x+4 : x+4])
			if l < minL {
				minL = l
			}
			if l > maxL {
				maxL = l
			}
		}
	}
	span := maxL - minL
	if span < 1e-6 {
		span = 1
	}
	for y := 0; y < h; y++ {
		row := pix[base+y*stride : base+y*stride+w*4]
		orow := out.Pix[y*out.Stride : y*out.Stride+w]
		for x := range orow {
			l := lumaOf(row[x*4 : x*4+4 : x*4+4])
			// Kept as A's exact `(l-minL)/span*255`: folding span into a
			// reciprocal multiply is a different float64 rounding and would
			// break the bit-identity this function exists to preserve.
			v := (l - minL) / span * 255
			if v < 0 {
				v = 0
			} else if v > 255 {
				v = 255
			}
			orow[x] = uint8(v + 0.5)
		}
	}
	return out
}

// ---- integer-factor bilinear in 16.16 fixed point ----

// fixShift is 16, so weights live in [0, 65536]. For factor 2 the only weights
// are 0.25 and 0.75, which are exact in binary, so the fixed-point scaler is
// bit-identical to the float64 one there. For factor 3 the weights 1/3 and 2/3
// carry at most 1/131072 of error, far below the half-gray-level rounding
// boundary. 16 bits is chosen over 8 for exactly that reason.
const fixShift = 16
const fixOne = 1 << fixShift

// bilinearAxis precomputes, for one axis, the (i0, i1, weight) triple of every
// output position. Because the factor is an integer the pattern repeats with
// period `scale`, but the clamped edges do not, so the table is built at full
// output length -- it is a few tens of KB and read sequentially.
func bilinearAxis(n, scale int) (i0s, i1s []int32, ws []uint32) {
	on := n * scale
	i0s = make([]int32, on)
	i1s = make([]int32, on)
	ws = make([]uint32, on)
	for o := 0; o < on; o++ {
		// Integer form of s = (o+0.5)/scale - 0.5, i.e. s = (2o+1-scale)/(2*scale).
		num := 2*o + 1 - scale
		den := 2 * scale
		f := num / den
		r := num - f*den
		if r < 0 { // Go truncates toward zero; bilinear needs floor.
			f--
			r += den
		}
		a, bb := f, f+1
		if a < 0 {
			a = 0
		} else if a >= n {
			a = n - 1
		}
		if bb < 0 {
			bb = 0
		} else if bb >= n {
			bb = n - 1
		}
		i0s[o], i1s[o] = int32(a), int32(bb)
		ws[o] = uint32((int64(r)*fixOne + int64(den)/2) / int64(den))
	}
	return
}

// upscaleBilinearFixed is the hot loop. Per output pixel it does two 32-bit
// multiply-adds horizontally and one 64-bit multiply-add vertically, and no
// float, no division, no floor and no clamp at all.
func upscaleBilinearFixed(src *image.Gray, scale int) *image.Gray {
	w, h := src.Bounds().Dx(), src.Bounds().Dy()
	ow, oh := w*scale, h*scale
	dst := image.NewGray(image.Rect(0, 0, ow, oh))

	x0s, x1s, wxs := bilinearAxis(w, scale)
	y0s, y1s, wys := bilinearAxis(h, scale)

	for oy := 0; oy < oh; oy++ {
		wy := uint64(wys[oy])
		iwy := uint64(fixOne) - wy
		r0 := src.Pix[int(y0s[oy])*src.Stride : int(y0s[oy])*src.Stride+w]
		r1 := src.Pix[int(y1s[oy])*src.Stride : int(y1s[oy])*src.Stride+w]
		out := dst.Pix[oy*dst.Stride : oy*dst.Stride+ow]
		for ox := range out {
			x0, x1 := int(x0s[ox]), int(x1s[ox])
			wx := wxs[ox]
			iwx := uint32(fixOne) - wx
			top := uint32(r0[x0])*iwx + uint32(r0[x1])*wx
			bot := uint32(r1[x0])*iwx + uint32(r1[x1])*wx
			// One rounding, at the end: +0.5 ulp then shift, which is the same
			// round-half-up the float path's uint8(v+0.5) performs.
			out[ox] = uint8((uint64(top)*iwy + uint64(bot)*wy + (1 << (2*fixShift - 1))) >> (2 * fixShift))
		}
	}
	return dst
}

// upscaleBilinearSeparable is the two-pass alternative kept only to measure it:
// horizontal into a uint16 intermediate at 8.8, then vertical. Hypothesis 5.
func upscaleBilinearSeparable(src *image.Gray, scale int) *image.Gray {
	w, h := src.Bounds().Dx(), src.Bounds().Dy()
	ow, oh := w*scale, h*scale
	dst := image.NewGray(image.Rect(0, 0, ow, oh))

	x0s, x1s, wxs := bilinearAxis(w, scale)
	y0s, y1s, wys := bilinearAxis(h, scale)

	// Intermediate is ow x h at 16.16-derived 16-bit precision (value<<16 >>16).
	mid := make([]uint32, ow*h)
	for y := 0; y < h; y++ {
		row := src.Pix[y*src.Stride : y*src.Stride+w]
		mrow := mid[y*ow : y*ow+ow]
		for ox := range mrow {
			wx := wxs[ox]
			mrow[ox] = uint32(row[x0s[ox]])*(uint32(fixOne)-wx) + uint32(row[x1s[ox]])*wx
		}
	}
	for oy := 0; oy < oh; oy++ {
		wy := uint64(wys[oy])
		iwy := uint64(fixOne) - wy
		m0 := mid[int(y0s[oy])*ow : int(y0s[oy])*ow+ow]
		m1 := mid[int(y1s[oy])*ow : int(y1s[oy])*ow+ow]
		out := dst.Pix[oy*dst.Stride : oy*dst.Stride+ow]
		for ox := range out {
			out[ox] = uint8((uint64(m0[ox])*iwy + uint64(m1[ox])*wy + (1 << (2*fixShift - 1))) >> (2 * fixShift))
		}
	}
	return dst
}

// ---- float control scalers, to separate hypothesis 2 from hypothesis 3 ----
//
// These are upscaleBilinearSeparable with the SAME precomputed per-column
// tables and the SAME two-pass structure, but carrying the weights and the
// accumulator in float64 / float32 instead of 16.16 fixed point. F64 vs the
// fixed-point version is exactly the cost of the arithmetic type, with the
// precomputation held constant.

func upscaleBilinearSepF64(src *image.Gray, scale int) *image.Gray {
	w, h := src.Bounds().Dx(), src.Bounds().Dy()
	ow, oh := w*scale, h*scale
	dst := image.NewGray(image.Rect(0, 0, ow, oh))
	x0s, x1s, wxs := bilinearAxis(w, scale)
	y0s, y1s, wys := bilinearAxis(h, scale)
	fwx := make([]float64, ow)
	for i, v := range wxs {
		fwx[i] = float64(v) / fixOne
	}
	mid := make([]float64, ow*h)
	for y := 0; y < h; y++ {
		row := src.Pix[y*src.Stride : y*src.Stride+w]
		mrow := mid[y*ow : y*ow+ow]
		for ox := range mrow {
			wx := fwx[ox]
			mrow[ox] = float64(row[x0s[ox]])*(1-wx) + float64(row[x1s[ox]])*wx
		}
	}
	for oy := 0; oy < oh; oy++ {
		wy := float64(wys[oy]) / fixOne
		m0 := mid[int(y0s[oy])*ow : int(y0s[oy])*ow+ow]
		m1 := mid[int(y1s[oy])*ow : int(y1s[oy])*ow+ow]
		out := dst.Pix[oy*dst.Stride : oy*dst.Stride+ow]
		for ox := range out {
			out[ox] = uint8(m0[ox]*(1-wy) + m1[ox]*wy + 0.5)
		}
	}
	return dst
}

func upscaleBilinearSepF32(src *image.Gray, scale int) *image.Gray {
	w, h := src.Bounds().Dx(), src.Bounds().Dy()
	ow, oh := w*scale, h*scale
	dst := image.NewGray(image.Rect(0, 0, ow, oh))
	x0s, x1s, wxs := bilinearAxis(w, scale)
	y0s, y1s, wys := bilinearAxis(h, scale)
	fwx := make([]float32, ow)
	for i, v := range wxs {
		fwx[i] = float32(v) / fixOne
	}
	mid := make([]float32, ow*h)
	for y := 0; y < h; y++ {
		row := src.Pix[y*src.Stride : y*src.Stride+w]
		mrow := mid[y*ow : y*ow+ow]
		for ox := range mrow {
			wx := fwx[ox]
			mrow[ox] = float32(row[x0s[ox]])*(1-wx) + float32(row[x1s[ox]])*wx
		}
	}
	for oy := 0; oy < oh; oy++ {
		wy := float32(wys[oy]) / fixOne
		m0 := mid[int(y0s[oy])*ow : int(y0s[oy])*ow+ow]
		m1 := mid[int(y1s[oy])*ow : int(y1s[oy])*ow+ow]
		out := dst.Pix[oy*dst.Stride : oy*dst.Stride+ow]
		for ox := range out {
			out[ox] = uint8(m0[ox]*(1-wy) + m1[ox]*wy + 0.5)
		}
	}
	return dst
}

// ---- variant entry points ----
//
// D and E are optimized variants; D1/D2/D3/D4 isolate their component changes.
//
//   D   bit-exact stretch   + separable fixed-point scaler
//   E   integer/LUT stretch + separable fixed-point scaler
//   D1  bit-exact stretch  + B2's float64 scaler            (isolates the stretch)
//   D2  B2's stretch       + single-pass fixed-point scaler (isolates the scaler)
//   D3  bit-exact stretch  + single-pass fixed-point scaler (D minus separability)
//   D4  integer/LUT stretch + single-pass fixed-point scaler

// D retains the byte-identical float stretch for comparisons that require it.
func preprocessD(img image.Image, scale int) *image.Gray {
	small := grayStretchNoBuf(img)
	if scale <= 1 {
		// Factor 1 is not a degenerate bilinear, it is a no-op: the stretched
		// plane IS the output. big.png hits this in production.
		return small
	}
	return upscaleBilinearSeparable(small, scale)
}

// E is D with the accepted integer-luma + LUT stretch. It differs from A on 77
// of 7,481,582 factor-1 pixels, all exact half-way ties with delta 1.
func preprocessE(img image.Image, scale int) *image.Gray {
	small := grayStretchLUT(img)
	if scale <= 1 {
		return small
	}
	return upscaleBilinearSeparable(small, scale)
}

// D1 isolates hypothesis 1: buffer-free stretch, B2's original float64 scaler.
func preprocessD1(img image.Image, scale int) *image.Gray {
	small := grayStretchNoBuf(img)
	if scale <= 1 {
		return small
	}
	return upscaleBilinearGray(small, scale)
}

// D2 isolates hypotheses 2-4: B2's original stretch, new fixed-point scaler.
func preprocessD2(img image.Image, scale int) *image.Gray {
	small := grayStretchFast(img)
	if scale <= 1 {
		return small
	}
	return upscaleBilinearFixed(small, scale)
}

// D3 is D with the single-pass 4-tap scaler instead of the separable one; D vs
// D3 is the whole of hypothesis 5.
func preprocessD3(img image.Image, scale int) *image.Gray {
	small := grayStretchNoBuf(img)
	if scale <= 1 {
		return small
	}
	return upscaleBilinearFixed(small, scale)
}

// D4 is E with the single-pass scaler; D4 vs E is hypothesis 5 again, on the
// LUT stretch, and D4 vs D3 is hypothesis 1's LUT half.
func preprocessD4(img image.Image, scale int) *image.Gray {
	small := grayStretchLUT(img)
	if scale <= 1 {
		return small
	}
	return upscaleBilinearFixed(small, scale)
}

// F64 / F32 are D with the arithmetic type of the scaler swapped, holding the
// precomputed tables and the separable structure constant. Hypothesis 3.
func preprocessF64(img image.Image, scale int) *image.Gray {
	small := grayStretchNoBuf(img)
	if scale <= 1 {
		return small
	}
	return upscaleBilinearSepF64(small, scale)
}

func preprocessF32(img image.Image, scale int) *image.Gray {
	small := grayStretchNoBuf(img)
	if scale <= 1 {
		return small
	}
	return upscaleBilinearSepF32(small, scale)
}
