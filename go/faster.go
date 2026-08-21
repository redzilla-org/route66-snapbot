// Variants G and H: the post-D round. D's profile puts 29% of ALL samples (and
// ~85% of the transform) in upscaleBilinearSeparable, so this file attacks that
// one function and nothing else.
//
// Two independent changes, kept separable so each is attributable:
//
//	G. Run-structured horizontal pass + bounds-check hoisting.
//	   D's horizontal pass is table-driven and GATHERS: per output pixel it does
//	   wxs[ox], x0s[ox], x1s[ox], row[x0s[ox]], row[x1s[ox]] -- five bounds
//	   checks (confirmed with -d=ssa/check_bce/debug=1 at fast.go:286-287) and
//	   two data-dependent loads whose addresses the CPU cannot predict from the
//	   loop counter. But the factor is an INTEGER, so x0 is constant across a run
//	   of exactly `scale` consecutive output columns. Walking runs instead of
//	   pixels hoists both source loads into registers (`a` and `b`) and leaves a
//	   single sequential weight table read. This is the same observation
//	   hypothesis 2 already exploited for the WEIGHTS; G extends it to the
//	   INDICES, which is where the remaining gather was.
//	   Arithmetic is unchanged, operation for operation, so G is bit-identical
//	   to D by construction.
//
//	H. 8.8 intermediate so the vertical pass is 32-bit.
//	   The vertical pass is the bigger of the two (ow*oh vs ow*h pixels) and D
//	   runs it in 64-bit: mid is 16.16 (24 significant bits), so mid*weight
//	   needs a 64x64 multiply. Rounding the intermediate to 8.8 puts it in 16
//	   bits, and 8.8 * 0.8 fits in 32. This is NOT bit-exact -- it adds one
//	   rounding step -- so it ships as an ablation with a measured pixel diff,
//	   not as the recommended path. See the correctness table in the report.
package main

import "image"

// bilinearRuns turns bilinearAxis's per-output-column tables into runs of
// constant (i0, i1). Returned as parallel arrays: run r covers output columns
// [starts[r], starts[r]+lens[r]) and reads source i0s[r], i1s[r].
//
// WHY it is derived from the table rather than computed in closed form: the
// clamped edge columns break the regular period (the first and last partial
// runs read i0 == i1), and reconstructing that arithmetic separately would be a
// second place for the edge convention to drift out of sync with bilinearAxis.
func bilinearRuns(i0s, i1s []int32) (starts, lens []int32, r0s, r1s []int32) {
	n := len(i0s)
	for o := 0; o < n; {
		e := o + 1
		for e < n && i0s[e] == i0s[o] && i1s[e] == i1s[o] {
			e++
		}
		starts = append(starts, int32(o))
		lens = append(lens, int32(e-o))
		r0s = append(r0s, i0s[o])
		r1s = append(r1s, i1s[o])
		o = e
	}
	return
}

// upscaleBilinearRuns is D's separable scaler with the horizontal gather
// replaced by run iteration and every hoistable bounds check hoisted. Every
// arithmetic operation, and its order, is identical to upscaleBilinearSeparable.
func upscaleBilinearRuns(src *image.Gray, scale int) *image.Gray {
	w, h := src.Bounds().Dx(), src.Bounds().Dy()
	ow, oh := w*scale, h*scale
	dst := image.NewGray(image.Rect(0, 0, ow, oh))

	x0s, x1s, wxs := bilinearAxis(w, scale)
	y0s, y1s, wys := bilinearAxis(h, scale)
	rs, rl, ra, rb := bilinearRuns(x0s, x1s)

	mid := make([]uint32, ow*h)
	for y := 0; y < h; y++ {
		row := src.Pix[y*src.Stride : y*src.Stride+w]
		mrow := mid[y*ow : y*ow+ow]
		for r := range rs {
			// The two source samples for this whole run live in registers.
			a := uint32(row[ra[r]])
			b := uint32(row[rb[r]])
			s, e := int(rs[r]), int(rs[r])+int(rl[r])
			out := mrow[s:e]
			// Re-slicing the weight table to the SAME length as the output
			// slice is what lets the compiler drop the wxs bounds check.
			ws := wxs[s:e:e]
			for i := range out {
				wx := ws[i]
				out[i] = a*(uint32(fixOne)-wx) + b*wx
			}
		}
	}

	for oy := 0; oy < oh; oy++ {
		wy := uint64(wys[oy])
		iwy := uint64(fixOne) - wy
		m0 := mid[int(y0s[oy])*ow : int(y0s[oy])*ow+ow]
		m1 := mid[int(y1s[oy])*ow : int(y1s[oy])*ow+ow]
		out := dst.Pix[oy*dst.Stride : oy*dst.Stride+ow]
		m0, m1 = m0[:len(out)], m1[:len(out)] // hoists both per-pixel checks
		for ox := range out {
			out[ox] = uint8((uint64(m0[ox])*iwy + uint64(m1[ox])*wy + (1 << (2*fixShift - 1))) >> (2 * fixShift))
		}
	}
	return dst
}

// upscaleBilinearRuns88 is upscaleBilinearRuns with the intermediate rounded to
// 8.8 so the vertical pass is a 32-bit multiply-add. NOT bit-exact vs D.
func upscaleBilinearRuns88(src *image.Gray, scale int) *image.Gray {
	w, h := src.Bounds().Dx(), src.Bounds().Dy()
	ow, oh := w*scale, h*scale
	dst := image.NewGray(image.Rect(0, 0, ow, oh))

	x0s, x1s, wxs := bilinearAxis(w, scale)
	y0s, y1s, wys := bilinearAxis(h, scale)
	rs, rl, ra, rb := bilinearRuns(x0s, x1s)

	// 8.8 weights: fixOne>>8 == 256, so a weight fits a uint16 and the product
	// with a 16-bit intermediate stays under 2^24.
	wx8 := make([]uint32, ow)
	for i, v := range wxs {
		wx8[i] = (v + 128) >> 8
	}

	mid := make([]uint16, ow*h)
	for y := 0; y < h; y++ {
		row := src.Pix[y*src.Stride : y*src.Stride+w]
		mrow := mid[y*ow : y*ow+ow]
		for r := range rs {
			a := uint32(row[ra[r]])
			b := uint32(row[rb[r]])
			s, e := int(rs[r]), int(rs[r])+int(rl[r])
			out := mrow[s:e]
			ws := wx8[s:e:e]
			for i := range out {
				wx := ws[i]
				out[i] = uint16(a*(256-wx) + b*wx) // 8.8, max 255*256
			}
		}
	}

	for oy := 0; oy < oh; oy++ {
		wy := (wys[oy] + 128) >> 8
		iwy := uint32(256) - wy
		m0 := mid[int(y0s[oy])*ow : int(y0s[oy])*ow+ow]
		m1 := mid[int(y1s[oy])*ow : int(y1s[oy])*ow+ow]
		out := dst.Pix[oy*dst.Stride : oy*dst.Stride+ow]
		m0, m1 = m0[:len(out)], m1[:len(out)]
		for ox := range out {
			out[ox] = uint8((uint32(m0[ox])*iwy + uint32(m1[ox])*wy + (1 << 15)) >> 16)
		}
	}
	return dst
}

// G: variant D with the run-structured scaler. Bit-identical to D everywhere.
func preprocessG(img image.Image, scale int) *image.Gray {
	small := grayStretchNoBuf(img)
	if scale <= 1 {
		return small
	}
	return upscaleBilinearRuns(small, scale)
}

// H: G with the 8.8 intermediate. Faster, not bit-exact.
func preprocessH(img image.Image, scale int) *image.Gray {
	small := grayStretchNoBuf(img)
	if scale <= 1 {
		return small
	}
	return upscaleBilinearRuns88(small, scale)
}

// ---- variant I: FMA-pinned stretch ----
//
// WHY this exists: GOAMD64=v3 was measured 4.4% faster than v1 on big.png's
// transform, and the assembly diff shows the ONLY changed instructions in the
// whole hot path are in grayStretchNoBuf, where v3 contracts
// `0.299*r + 0.587*g + 0.114*b` into two VFMADD231SD. That fusion rounds once
// instead of twice, which is why the v3 build is no longer byte-identical to a
// v1-built reference (80 of 7,481,582 pixels on big.png).
//
// Go's spec lets an implementation fuse only across expressions that are not
// separated by an explicit conversion, so wrapping each product in float64()
// FORBIDS the contraction. Variant I is grayStretchNoBuf with exactly that
// change and nothing else: built under v3 it is the control that says whether
// v3's win IS the FMA (I loses the win) or something else (I keeps it).
func grayStretchNoBufPinned(img image.Image) *image.Gray {
	b := img.Bounds()
	w, h := b.Dx(), b.Dy()
	out := image.NewGray(image.Rect(0, 0, w, h))
	src, ok := img.(*image.RGBA)
	if !ok {
		return grayStretchFast(img)
	}
	minL, maxL := 1.7976931348623157e+308, -1.7976931348623157e+308
	base := (b.Min.Y-src.Rect.Min.Y)*src.Stride + (b.Min.X-src.Rect.Min.X)*4
	for y := 0; y < h; y++ {
		row := src.Pix[base+y*src.Stride : base+y*src.Stride+w*4]
		for x := 0; x+4 <= len(row); x += 4 {
			l := float64(0.299*float64(row[x])) + float64(0.587*float64(row[x+1])) + float64(0.114*float64(row[x+2]))
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
		row := src.Pix[base+y*src.Stride : base+y*src.Stride+w*4]
		orow := out.Pix[y*out.Stride : y*out.Stride+w]
		for x := range orow {
			p := row[x*4 : x*4+3 : x*4+3]
			l := float64(0.299*float64(p[0])) + float64(0.587*float64(p[1])) + float64(0.114*float64(p[2]))
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

// I is D with the FMA-pinned stretch.
func preprocessI(img image.Image, scale int) *image.Gray {
	small := grayStretchNoBufPinned(img)
	if scale <= 1 {
		return small
	}
	return upscaleBilinearSeparable(small, scale)
}

// ---- variant J: MEASUREMENT-ONLY memory floor ----
//
// J is NOT a candidate implementation and its output is deliberately wrong. It
// is upscaleBilinearSeparable with all arithmetic stripped out of both inner
// loops, keeping only the loads, the stores and the loop overhead. It answers
// the only question that decides whether hand-written SIMD is worth writing:
// how much of the scaler's time is arithmetic at all?
//
// SIMD can, at its theoretical best, remove everything J removes and no more.
// D minus J is therefore the hard ceiling on any vectorization of this loop,
// and if that ceiling is small the assembly is not worth writing.
func upscaleBilinearMemFloor(src *image.Gray, scale int) *image.Gray {
	w, h := src.Bounds().Dx(), src.Bounds().Dy()
	ow, oh := w*scale, h*scale
	dst := image.NewGray(image.Rect(0, 0, ow, oh))
	x0s, x1s, wxs := bilinearAxis(w, scale)
	y0s, y1s, _ := bilinearAxis(h, scale)
	_ = x1s
	_ = wxs

	mid := make([]uint32, ow*h)
	for y := 0; y < h; y++ {
		row := src.Pix[y*src.Stride : y*src.Stride+w]
		mrow := mid[y*ow : y*ow+ow]
		for ox := range mrow {
			mrow[ox] = uint32(row[x0s[ox]]) // one gather, no multiply-add
		}
	}
	for oy := 0; oy < oh; oy++ {
		m0 := mid[int(y0s[oy])*ow : int(y0s[oy])*ow+ow]
		m1 := mid[int(y1s[oy])*ow : int(y1s[oy])*ow+ow]
		out := dst.Pix[oy*dst.Stride : oy*dst.Stride+ow]
		m0, m1 = m0[:len(out)], m1[:len(out)]
		for ox := range out {
			out[ox] = uint8(m0[ox] + m1[ox]) // both loads kept, no 64-bit MAC
		}
	}
	return dst
}

// J: measurement-only. Output is NOT the pipeline's output; never a candidate.
func preprocessJ(img image.Image, scale int) *image.Gray {
	small := grayStretchNoBuf(img)
	if scale <= 1 {
		return small
	}
	return upscaleBilinearMemFloor(small, scale)
}

// ---- variant K: factor-2 specialization ----
//
// WHY: variant J (arithmetic stripped, loads and stores kept) runs the transform
// 1.45x faster than D on typical.png, which says roughly half the scaler's time
// IS arithmetic -- so there is real headroom, and the question is whether it
// needs SIMD to reach.
//
// It does not, because the factor is not just an integer, it is 2 or 3 (the
// pixel-budget rule caps it at 3, and 1 is a no-op). At factor 2 the bilinear
// weights are exactly 1/4 and 3/4, so D's whole 16.16 apparatus collapses:
//
//	horizontal: mid = 3a+b  and  a+3b   (max 1020, fits uint16)
//	vertical:   out = (3*M0 + M1 + 8) >> 4
//
// Both are shift-and-add on 16-bit data with a 32-bit accumulator, replacing
// two 32-bit multiplies plus two 64-bit multiplies per pixel, AND halving the
// intermediate from uint32 to uint16 (31.8 MB -> 15.9 MB on typical.png).
//
// It is bit-identical to D by construction, not by luck: 1/4 and 3/4 are exact
// in binary, D applies exactly one rounding at the end of the vertical pass,
// and (3*M0+M1+8)>>4 is that same round-half-up of the same rational value.
// Factors other than 2 fall back to D's general scaler.
func upscaleBilinear2x(src *image.Gray) *image.Gray {
	w, h := src.Bounds().Dx(), src.Bounds().Dy()
	ow, oh := w*2, h*2
	dst := image.NewGray(image.Rect(0, 0, ow, oh))
	mid := make([]uint16, ow*h)

	for y := 0; y < h; y++ {
		row := src.Pix[y*src.Stride : y*src.Stride+w]
		mrow := mid[y*ow : y*ow+ow]
		// Edge columns are the clamped x0 == x1 case: both taps are the same
		// pixel, so the weighted sum is 4*that pixel.
		mrow[0] = uint16(row[0]) * 4
		mrow[ow-1] = uint16(row[w-1]) * 4
		for x := 0; x+1 < w; x++ {
			a := uint16(row[x])
			b := uint16(row[x+1])
			mrow[2*x+1] = 3*a + b
			mrow[2*x+2] = a + 3*b
		}
	}

	// Vertical, same structure: output row 0 and oh-1 are the clamped rows,
	// interior rows pair (y, y+1) with weights 3:1 and 1:3.
	rowOut := func(dsty int, m0, m1 []uint16, c0, c1 uint32) {
		out := dst.Pix[dsty*dst.Stride : dsty*dst.Stride+ow]
		m0, m1 = m0[:len(out)], m1[:len(out)]
		for ox := range out {
			out[ox] = uint8((c0*uint32(m0[ox]) + c1*uint32(m1[ox]) + 8) >> 4)
		}
	}
	first := mid[0:ow]
	rowOut(0, first, first, 2, 2)
	last := mid[(h-1)*ow : h*ow]
	rowOut(oh-1, last, last, 2, 2)
	for y := 0; y+1 < h; y++ {
		m0 := mid[y*ow : (y+1)*ow]
		m1 := mid[(y+1)*ow : (y+2)*ow]
		rowOut(2*y+1, m0, m1, 3, 1)
		rowOut(2*y+2, m0, m1, 1, 3)
	}
	return dst
}

// K: D with the factor-2 specialization. Bit-identical to D.
func preprocessK(img image.Image, scale int) *image.Gray {
	small := grayStretchNoBuf(img)
	if scale <= 1 {
		return small
	}
	if scale == 2 {
		return upscaleBilinear2x(small)
	}
	return upscaleBilinearSeparable(small, scale)
}
