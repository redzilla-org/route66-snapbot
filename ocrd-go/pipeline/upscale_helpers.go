package pipeline

const fixShift = 16
const fixOne = 1 << fixShift

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
