// Package ocrpre is the drop-in replacement for goregression's preprocessForOCR.
//
// PASTE-READY: this file is self-contained (stdlib `image` + `math` only) and is
// byte-for-byte verified against the benchmarked variant N. Copy it into
// tests/webapp/regression/goregression/, change the package clause to match the
// package there, unexport the names, and delete the old preprocessForOCR body.
//
// Pipeline, unchanged in meaning from the original: luma grayscale -> global
// min/max contrast stretch -> integer-factor bilinear upscale.
//
// The optimization is threefold and each part was measured independently:
//  1. no []float64 luma side buffer (recompute is cheaper than store + reload),
//  2. the upscale factor is an integer, so the per-column (i0,i1,weight) triple
//     is constant across a run of `scale` output columns and the weights are
//     exact rationals with a tiny denominator,
//  3. separable two-pass scaling instead of a 4-tap gather per output pixel.
//
// Measured against the original on real captures (transform phase, GOAMD64=v1):
// factor 3: 354.90 -> 50.26 ms, factor 2: 384.30 -> 61.09 ms, factor 1:
// 368.89 -> 62.19 ms. Output is within 1 gray level of the original everywhere,
// and byte-identical at factor 1. See PORTING.md for the pixel accounting.
package ocrpre

import (
	"image"
	"math"
)

// PreprocessForOCR replaces the original function of the same name. Signature,
// guard clauses and semantics are preserved exactly.
func PreprocessForOCR(img image.Image, scale int) *image.Gray {
	if scale < 1 {
		scale = 1
	}
	b := img.Bounds()
	if b.Dx() == 0 || b.Dy() == 0 {
		return image.NewGray(image.Rect(0, 0, 0, 0))
	}
	small := grayStretch(img)
	switch {
	case scale <= 1:
		// Factor 1 is a no-op, not a degenerate bilinear: the stretched plane IS
		// the output. This is also the case that stays byte-identical to the
		// original implementation.
		return small
	case scale == 2:
		return upscale2x(small)
	case scale == 3:
		return upscaleRunsExact(small, scale)
	default:
		return upscaleSeparable(small, scale)
	}
}

// ---- stretch ----

// grayStretch produces the contrast-stretched 8-bit gray plane at SOURCE
// resolution in two passes over the source pixels, with no float64 side buffer.
//
// TWO fast paths, deliberately: a screenshot without alpha decodes to
// *image.RGBA, one WITH alpha decodes to *image.NRGBA. Handling only the former
// makes an alpha-carrying capture silently fall back to the generic At() path
// and lose most of the win with no error anywhere.
func grayStretch(img image.Image) *image.Gray {
	b := img.Bounds()
	w, h := b.Dx(), b.Dy()
	out := image.NewGray(image.Rect(0, 0, w, h))

	var pix []byte
	var stride, base int
	premul := false
	switch src := img.(type) {
	case *image.RGBA:
		pix, stride = src.Pix, src.Stride
		base = (b.Min.Y-src.Rect.Min.Y)*src.Stride + (b.Min.X-src.Rect.Min.X)*4
	case *image.NRGBA:
		// NRGBA's Pix is NOT premultiplied but At().RGBA() premultiplies, so
		// premultiply here with the SAME integer division to stay identical.
		pix, stride, premul = src.Pix, src.Stride, true
		base = (b.Min.Y-src.Rect.Min.Y)*src.Stride + (b.Min.X-src.Rect.Min.X)*4
	default:
		return grayStretchGeneric(img)
	}

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
			// Kept as the original's exact (l-minL)/span*255: folding span into
			// a reciprocal multiply is a different float64 rounding.
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

// grayStretchGeneric is the correctness backstop for any other concrete type.
// It is slow (an interface call per pixel) and should never run on a screenshot;
// if it does, the capture's type changed and the fast paths need another arm.
func grayStretchGeneric(img image.Image) *image.Gray {
	b := img.Bounds()
	w, h := b.Dx(), b.Dy()
	out := image.NewGray(image.Rect(0, 0, w, h))
	lum := make([]float64, w*h)
	minL, maxL := math.MaxFloat64, -math.MaxFloat64
	for y := 0; y < h; y++ {
		for x := 0; x < w; x++ {
			r, g, bb, _ := img.At(b.Min.X+x, b.Min.Y+y).RGBA()
			l := 0.299*float64(uint8(r>>8)) + 0.587*float64(uint8(g>>8)) + 0.114*float64(uint8(bb>>8))
			lum[y*w+x] = l
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
	for i, l := range lum {
		v := (l - minL) / span * 255
		if v < 0 {
			v = 0
		} else if v > 255 {
			v = 255
		}
		out.Pix[i] = uint8(v + 0.5)
	}
	return out
}

// ---- axis tables ----

const fixShift = 16
const fixOne = 1 << fixShift

// axisExact returns, per output position, the two source taps and the exact
// rational weight numerator r over the denominator 2*scale.
func axisExact(n, scale int) (i0s, i1s []int32, rs []uint32) {
	on := n * scale
	i0s, i1s, rs = make([]int32, on), make([]int32, on), make([]uint32, on)
	for o := 0; o < on; o++ {
		// s = (o+0.5)/scale - 0.5 = (2o+1-scale)/(2*scale)
		num, d := 2*o+1-scale, 2*scale
		f := num / d
		r := num - f*d
		if r < 0 { // Go truncates toward zero; bilinear needs floor
			f--
			r += d
		}
		a, b := f, f+1
		if a < 0 {
			a = 0
		} else if a >= n {
			a = n - 1
		}
		if b < 0 {
			b = 0
		} else if b >= n {
			b = n - 1
		}
		i0s[o], i1s[o], rs[o] = int32(a), int32(b), uint32(r)
	}
	return
}

// axisFixed is axisExact expressed in 16.16, for the general fallback scaler.
func axisFixed(n, scale int) (i0s, i1s []int32, ws []uint32) {
	i0s, i1s, rs := axisExact(n, scale)
	den := int64(2 * scale)
	ws = make([]uint32, len(rs))
	for i, r := range rs {
		ws[i] = uint32((int64(r)*fixOne + den/2) / den)
	}
	return i0s, i1s, ws
}

// runsOf collapses per-column tables into runs of constant (i0, i1). Because the
// factor is an integer these runs are `scale` long except at the clamped edges,
// which is what lets both source samples hoist into registers.
func runsOf(i0s, i1s []int32) (starts, lens, r0s, r1s []int32) {
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

// ---- scalers ----

// upscale2x is the factor-2 kernel. The weights are exactly 1/4 and 3/4, so the
// whole fixed-point apparatus collapses to shift-and-add on uint16 data:
// horizontally mid = 3a+b and a+3b, vertically out = (3*M0+M1+8)>>4.
func upscale2x(src *image.Gray) *image.Gray {
	w, h := src.Bounds().Dx(), src.Bounds().Dy()
	ow, oh := w*2, h*2
	dst := image.NewGray(image.Rect(0, 0, ow, oh))
	mid := make([]uint16, ow*h)

	for y := 0; y < h; y++ {
		row := src.Pix[y*src.Stride : y*src.Stride+w]
		mrow := mid[y*ow : y*ow+ow]
		// Edges are the clamped i0 == i1 case: both taps are the same pixel.
		mrow[0] = uint16(row[0]) * 4
		mrow[ow-1] = uint16(row[w-1]) * 4
		for x := 0; x+1 < w; x++ {
			a, b := uint16(row[x]), uint16(row[x+1])
			mrow[2*x+1] = 3*a + b
			mrow[2*x+2] = a + 3*b
		}
	}

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

// upscaleRunsExact is the factor-3+ kernel: a run-structured horizontal pass
// (both source taps in registers) with exact rational weights r/(2f), so uint16
// intermediates and a CONSTANT divisor replace 16.16 fixed point.
//
// The divisor must be a compile-time constant. Written as a variable the
// compiler emits a hardware DIV in the hottest loop, measured 1.35x SLOWER than
// the fixed-point scaler this replaces.
func upscaleRunsExact(src *image.Gray, scale int) *image.Gray {
	w, h := src.Bounds().Dx(), src.Bounds().Dy()
	ow, oh := w*scale, h*scale
	dst := image.NewGray(image.Rect(0, 0, ow, oh))
	den := uint32(2 * scale)

	x0s, x1s, rxs := axisExact(w, scale)
	y0s, y1s, rys := axisExact(h, scale)
	rs, rl, ra, rb := runsOf(x0s, x1s)

	mid := make([]uint16, ow*h)
	for y := 0; y < h; y++ {
		row := src.Pix[y*src.Stride : y*src.Stride+w]
		mrow := mid[y*ow : y*ow+ow]
		for k := range rs {
			a := uint32(row[ra[k]])
			b := uint32(row[rb[k]])
			s, e := int(rs[k]), int(rs[k])+int(rl[k])
			out := mrow[s:e]
			ws := rxs[s:e:e] // same length as out -> bounds check hoisted
			for i := range out {
				r := ws[i]
				out[i] = uint16(a*(den-r) + b*r)
			}
		}
	}

	vert := func(half uint32, div func(uint32) uint8) {
		for oy := 0; oy < oh; oy++ {
			ry := rys[oy]
			iry := den - ry
			m0 := mid[int(y0s[oy])*ow : int(y0s[oy])*ow+ow]
			m1 := mid[int(y1s[oy])*ow : int(y1s[oy])*ow+ow]
			out := dst.Pix[oy*dst.Stride : oy*dst.Stride+ow]
			m0, m1 = m0[:len(out)], m1[:len(out)]
			for ox := range out {
				out[ox] = div(uint32(m0[ox])*iry + uint32(m1[ox])*ry + half)
			}
		}
	}
	switch scale {
	case 3:
		vert(18, func(v uint32) uint8 { return uint8(v / 36) })
	case 4:
		vert(32, func(v uint32) uint8 { return uint8(v / 64) })
	default:
		dd := den * den
		vert(dd/2, func(v uint32) uint8 { return uint8(v / dd) })
	}
	return dst
}

// upscaleSeparable is the general 16.16 separable scaler, kept as the fallback
// for factors the specialized kernels do not cover. ocrUpscaleFactorFor only
// ever returns 1, 2 or 3, so in this codebase it is unreachable.
func upscaleSeparable(src *image.Gray, scale int) *image.Gray {
	w, h := src.Bounds().Dx(), src.Bounds().Dy()
	ow, oh := w*scale, h*scale
	dst := image.NewGray(image.Rect(0, 0, ow, oh))

	x0s, x1s, wxs := axisFixed(w, scale)
	y0s, y1s, wys := axisFixed(h, scale)

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
		m0, m1 = m0[:len(out)], m1[:len(out)]
		for ox := range out {
			out[ox] = uint8((uint64(m0[ox])*iwy + uint64(m1[ox])*wy + (1 << (2*fixShift - 1))) >> (2 * fixShift))
		}
	}
	return dst
}
