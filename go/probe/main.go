// Throwaway probe: for every pixel where variant A's float64 stretch disagrees
// with the exact-integer stretch, report whether the exact value is a half-way
// tie (i.e. A's disagreement is float rounding error, not a logic difference).
package main

import (
	"bytes"
	"fmt"
	"image"
	"image/png"
	"os"
)

func main() {
	raw, _ := os.ReadFile(os.Args[1])
	im, err := png.Decode(bytes.NewReader(raw))
	if err != nil {
		panic(err)
	}
	src := im.(*image.RGBA)
	b := src.Bounds()
	w, h := b.Dx(), b.Dy()
	minF, maxF := 1e300, -1e300
	minI, maxI := uint64(1<<40), uint64(0)
	for y := 0; y < h; y++ {
		row := src.Pix[y*src.Stride:]
		for x := 0; x < w; x++ {
			p := row[x*4:]
			l := 0.299*float64(p[0]) + 0.587*float64(p[1]) + 0.114*float64(p[2])
			if l < minF {
				minF = l
			}
			if l > maxF {
				maxF = l
			}
			li := 299*uint64(p[0]) + 587*uint64(p[1]) + 114*uint64(p[2])
			if li < minI {
				minI = li
			}
			if li > maxI {
				maxI = li
			}
		}
	}
	span := maxF - minF
	spanI := maxI - minI
	diff, ties := 0, 0
	shown := 0
	for y := 0; y < h; y++ {
		row := src.Pix[y*src.Stride:]
		for x := 0; x < w; x++ {
			p := row[x*4:]
			l := 0.299*float64(p[0]) + 0.587*float64(p[1]) + 0.114*float64(p[2])
			va := uint8((l-minF)/span*255 + 0.5)
			li := 299*uint64(p[0]) + 587*uint64(p[1]) + 114*uint64(p[2])
			i := li - minI
			vb := uint8((2*i*255 + spanI) / (2 * spanI))
			if va != vb {
				diff++
				if (2*i*255)%(2*spanI) == spanI {
					ties++
				}
				if shown < 5 {
					shown++
					fmt.Printf("rgb=(%d,%d,%d) L=%d exact=%.10f A=%d exactRound=%d tieRemainder=%d/%d\n",
						p[0], p[1], p[2], li, float64(i)*255/float64(spanI), va, vb, (2*i*255)%(2*spanI), 2*spanI)
				}
			}
		}
	}
	fmt.Printf("differing=%d  exact-half-way-ties=%d  minI=%d maxI=%d spanI=%d\n", diff, ties, minI, maxI, spanI)
}
