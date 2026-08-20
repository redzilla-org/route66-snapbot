// compare decodes two preprocessed outputs (PNG or binary PGM) into 8-bit gray
// buffers and reports how far apart they are: dimension agreement, the percentage
// of differing pixels, and the maximum absolute difference. Byte identity is not
// expected -- the three implementations use different scalers -- but a large
// divergence means the math was ported wrong.
package main

import (
	"bufio"
	"fmt"
	"image"
	"image/png"
	"os"
	"strings"
)

func load(path string) ([]byte, int, int) {
	f, err := os.Open(path)
	if err != nil {
		panic(err)
	}
	defer f.Close()
	if strings.HasSuffix(path, ".pgm") {
		r := bufio.NewReaderSize(f, 1<<20)
		var magic string
		var w, h, maxv int
		if _, err := fmt.Fscan(r, &magic, &w, &h, &maxv); err != nil {
			panic(err)
		}
		if magic != "P5" || maxv != 255 {
			panic("unexpected pgm header " + magic)
		}
		r.ReadByte() // the single whitespace byte after maxval
		pix := make([]byte, w*h)
		if _, err := ioReadFull(r, pix); err != nil {
			panic(err)
		}
		return pix, w, h
	}
	img, err := png.Decode(bufio.NewReaderSize(f, 1<<20))
	if err != nil {
		panic(err)
	}
	g, ok := img.(*image.Gray)
	if !ok {
		panic(fmt.Sprintf("expected *image.Gray, got %T", img))
	}
	w, h := g.Bounds().Dx(), g.Bounds().Dy()
	pix := make([]byte, w*h)
	for y := 0; y < h; y++ {
		copy(pix[y*w:(y+1)*w], g.Pix[y*g.Stride:y*g.Stride+w])
	}
	return pix, w, h
}

func ioReadFull(r *bufio.Reader, b []byte) (int, error) {
	n := 0
	for n < len(b) {
		m, err := r.Read(b[n:])
		n += m
		if err != nil {
			return n, err
		}
	}
	return n, nil
}

func main() {
	a, aw, ah := load(os.Args[1])
	b, bw, bh := load(os.Args[2])
	if aw != bw || ah != bh {
		fmt.Printf("DIMENSION MISMATCH %s=%dx%d %s=%dx%d\n", os.Args[1], aw, ah, os.Args[2], bw, bh)
		os.Exit(1)
	}
	diff, maxAbs := 0, 0
	hist := map[int]int{}
	for i := range a {
		d := int(a[i]) - int(b[i])
		if d < 0 {
			d = -d
		}
		if d != 0 {
			diff++
			hist[d]++
			if d > maxAbs {
				maxAbs = d
			}
		}
	}
	fmt.Printf("%s vs %s: %dx%d  differing=%d/%d (%.4f%%)  maxAbsDelta=%d\n",
		os.Args[1], os.Args[2], aw, ah, diff, len(a), 100*float64(diff)/float64(len(a)), maxAbs)
	for d := 1; d <= maxAbs && d <= 8; d++ {
		if hist[d] > 0 {
			fmt.Printf("   delta=%d: %d px (%.4f%%)\n", d, hist[d], 100*float64(hist[d])/float64(len(a)))
		}
	}
}
