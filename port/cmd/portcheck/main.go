// portcheck runs the paste-ready package over a fixture and writes a PGM, so the
// exact file the developer will paste can be diffed byte-for-byte against the
// variant that was actually benchmarked. A porting guide whose code was never
// compiled is a liability; this is what makes it verified rather than transcribed.
package main

import (
	"bufio"
	"fmt"
	"image/png"
	"os"

	ocrpre "imgscalingbench/port"
)

func main() {
	f, err := os.Open(os.Args[1])
	if err != nil {
		panic(err)
	}
	img, err := png.Decode(bufio.NewReaderSize(f, 1<<20))
	f.Close()
	if err != nil {
		panic(err)
	}
	b := img.Bounds()
	scale := ocrUpscaleFactorFor(b.Dx() * b.Dy())
	g := ocrpre.PreprocessForOCR(img, scale)

	o, err := os.Create(os.Args[2])
	if err != nil {
		panic(err)
	}
	w := bufio.NewWriterSize(o, 1<<20)
	gb := g.Bounds()
	fmt.Fprintf(w, "P5\n%d %d\n255\n", gb.Dx(), gb.Dy())
	for y := 0; y < gb.Dy(); y++ {
		if _, err := w.Write(g.Pix[y*g.Stride : y*g.Stride+gb.Dx()]); err != nil {
			panic(err)
		}
	}
	w.Flush()
	o.Close()
	fmt.Printf("%s type=%T scale=%d out=%dx%d\n", os.Args[1], img, scale, gb.Dx(), gb.Dy())
}

// ocrUpscaleFactorFor mirrors the real pixel-budget rule in image_heuristics.go.
func ocrUpscaleFactorFor(px int) int {
	if px <= 0 {
		return 3
	}
	for f := 3; f > 1; f-- {
		if px*f*f <= 20_000_000 {
			return f
		}
	}
	return 1
}
