package pipeline

import "image"

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
