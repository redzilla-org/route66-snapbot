// Benchmark harness for the Go side of the image preprocessing comparison.
// Variant A is the naive implementation, variant B the optimized one, and B2 a
// variant of B with a hand-rolled bilinear upscale. All are single-threaded and
// phase-timed (decode / transform / encode+write).
package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"image"
	"image/color"
	"image/png"
	"math"
	"os"
	"runtime/pprof"
	"sort"
	"time"

	xdraw "golang.org/x/image/draw"
)

// ---- pipeline constants ----
const ocrUpscaleFactor = 3
const ocrUpscalePixelBudget = 20_000_000

func ocrUpscaleFactorFor(b image.Rectangle) int {
	px := b.Dx() * b.Dy()
	if px <= 0 {
		return ocrUpscaleFactor
	}
	for f := ocrUpscaleFactor; f > 1; f-- {
		if px*f*f <= ocrUpscalePixelBudget {
			return f
		}
	}
	return 1
}

func rgb8(c interface{ RGBA() (r, g, b, a uint32) }) (uint8, uint8, uint8) {
	r, g, b, _ := c.RGBA()
	return uint8(r >> 8), uint8(g >> 8), uint8(b >> 8)
}

func luma(r, g, b uint8) float64 {
	return 0.299*float64(r) + 0.587*float64(g) + 0.114*float64(b)
}

// ---- VARIANT A: faithful copy of preprocessForOCR ----
func preprocessForOCR(img image.Image, scale int) *image.Gray {
	if scale < 1 {
		scale = 1
	}
	b := img.Bounds()
	w, h := b.Dx(), b.Dy()
	if w == 0 || h == 0 {
		return image.NewGray(image.Rect(0, 0, 0, 0))
	}
	gray := make([]float64, w*h)
	minL, maxL := math.MaxFloat64, -math.MaxFloat64
	for y := 0; y < h; y++ {
		for x := 0; x < w; x++ {
			r8, g8, b8 := rgb8(img.At(b.Min.X+x, b.Min.Y+y))
			l := luma(r8, g8, b8)
			gray[y*w+x] = l
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
	stretch := func(l float64) float64 {
		v := (l - minL) / span * 255
		if v < 0 {
			return 0
		}
		if v > 255 {
			return 255
		}
		return v
	}
	sample := func(x, y int) float64 {
		if x < 0 {
			x = 0
		} else if x >= w {
			x = w - 1
		}
		if y < 0 {
			y = 0
		} else if y >= h {
			y = h - 1
		}
		return gray[y*w+x]
	}
	ow, oh := w*scale, h*scale
	out := image.NewGray(image.Rect(0, 0, ow, oh))
	for oy := 0; oy < oh; oy++ {
		sy := (float64(oy)+0.5)/float64(scale) - 0.5
		y0 := int(math.Floor(sy))
		fy := sy - float64(y0)
		for ox := 0; ox < ow; ox++ {
			sx := (float64(ox)+0.5)/float64(scale) - 0.5
			x0 := int(math.Floor(sx))
			fx := sx - float64(x0)
			top := sample(x0, y0)*(1-fx) + sample(x0+1, y0)*fx
			bot := sample(x0, y0+1)*(1-fx) + sample(x0+1, y0+1)*fx
			out.SetGray(ox, oy, color.Gray{Y: uint8(stretch(top*(1-fy)+bot*fy) + 0.5)})
		}
	}
	return out
}

func writePreprocessedPNG(img *image.Gray, path string) error {
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	enc := png.Encoder{CompressionLevel: png.BestSpeed}
	if err := enc.Encode(f, img); err != nil {
		f.Close()
		return err
	}
	return f.Close()
}

// ---- VARIANT B: optimized ----
// grayStretchFast produces the contrast-stretched 8-bit gray at SOURCE resolution,
// reading pixels through direct Pix indexing where the concrete type allows it.
func grayStretchFast(img image.Image) *image.Gray {
	b := img.Bounds()
	w, h := b.Dx(), b.Dy()
	out := image.NewGray(image.Rect(0, 0, w, h))
	lum := make([]float64, w*h)
	minL, maxL := math.MaxFloat64, -math.MaxFloat64
	put := func(i int, l float64) {
		lum[i] = l
		if l < minL {
			minL = l
		}
		if l > maxL {
			maxL = l
		}
	}
	switch src := img.(type) {
	case *image.RGBA:
		// RGBA Pix is alpha-premultiplied and At()->RGBA() returns the same
		// premultiplied values, so indexing Pix matches the reference path exactly.
		for y := 0; y < h; y++ {
			row := src.Pix[(y+b.Min.Y-src.Rect.Min.Y)*src.Stride+(b.Min.X-src.Rect.Min.X)*4:]
			for x := 0; x < w; x++ {
				p := row[x*4:]
				put(y*w+x, luma(p[0], p[1], p[2]))
			}
		}
	case *image.NRGBA:
		// NRGBA Pix is NOT premultiplied but At()->RGBA() premultiplies, so
		// premultiply here to keep luma identical to the reference path.
		for y := 0; y < h; y++ {
			row := src.Pix[(y+b.Min.Y-src.Rect.Min.Y)*src.Stride+(b.Min.X-src.Rect.Min.X)*4:]
			for x := 0; x < w; x++ {
				p := row[x*4:]
				a := uint32(p[3])
				r8 := uint8(uint32(p[0]) * a / 255)
				g8 := uint8(uint32(p[1]) * a / 255)
				b8 := uint8(uint32(p[2]) * a / 255)
				put(y*w+x, luma(r8, g8, b8))
			}
		}
	case *image.Gray:
		for y := 0; y < h; y++ {
			row := src.Pix[(y+b.Min.Y-src.Rect.Min.Y)*src.Stride+(b.Min.X-src.Rect.Min.X):]
			for x := 0; x < w; x++ {
				g := row[x]
				put(y*w+x, luma(g, g, g))
			}
		}
	default:
		for y := 0; y < h; y++ {
			for x := 0; x < w; x++ {
				r8, g8, b8 := rgb8(img.At(b.Min.X+x, b.Min.Y+y))
				put(y*w+x, luma(r8, g8, b8))
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

func preprocessFast(img image.Image, scale int) *image.Gray {
	small := grayStretchFast(img)
	if scale <= 1 {
		return small
	}
	b := small.Bounds()
	dst := image.NewGray(image.Rect(0, 0, b.Dx()*scale, b.Dy()*scale))
	xdraw.BiLinear.Scale(dst, dst.Bounds(), small, b, xdraw.Src, nil)
	return dst
}

// upscaleBilinearGray is a hand-rolled uint8 bilinear upscale over the already
// contrast-stretched gray plane. It exists because x/image's BiLinear.Scale has NO
// generated fast path for a *image.Gray destination (only *image.RGBA); a Gray dst
// falls back to the RGBA64Image interface path, four float64 channels per pixel.
func upscaleBilinearGray(src *image.Gray, scale int) *image.Gray {
	w, h := src.Bounds().Dx(), src.Bounds().Dy()
	ow, oh := w*scale, h*scale
	dst := image.NewGray(image.Rect(0, 0, ow, oh))
	inv := 1.0 / float64(scale)
	for oy := 0; oy < oh; oy++ {
		sy := (float64(oy)+0.5)*inv - 0.5
		y0 := int(math.Floor(sy))
		fy := sy - float64(y0)
		y1 := y0 + 1
		if y0 < 0 {
			y0 = 0
		} else if y0 >= h {
			y0 = h - 1
		}
		if y1 < 0 {
			y1 = 0
		} else if y1 >= h {
			y1 = h - 1
		}
		r0 := src.Pix[y0*src.Stride:]
		r1 := src.Pix[y1*src.Stride:]
		out := dst.Pix[oy*dst.Stride:]
		for ox := 0; ox < ow; ox++ {
			sx := (float64(ox)+0.5)*inv - 0.5
			x0 := int(math.Floor(sx))
			fx := sx - float64(x0)
			x1 := x0 + 1
			if x0 < 0 {
				x0 = 0
			} else if x0 >= w {
				x0 = w - 1
			}
			if x1 < 0 {
				x1 = 0
			} else if x1 >= w {
				x1 = w - 1
			}
			top := float64(r0[x0])*(1-fx) + float64(r0[x1])*fx
			bot := float64(r1[x0])*(1-fx) + float64(r1[x1])*fx
			out[ox] = uint8(top*(1-fy) + bot*fy + 0.5)
		}
	}
	return dst
}

func preprocessFast2(img image.Image, scale int) *image.Gray {
	small := grayStretchFast(img)
	if scale <= 1 {
		return small
	}
	return upscaleBilinearGray(small, scale)
}

func writePGM(img *image.Gray, path string) error {
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	w := bufio.NewWriterSize(f, 1<<20)
	b := img.Bounds()
	fmt.Fprintf(w, "P5\n%d %d\n255\n", b.Dx(), b.Dy())
	for y := 0; y < b.Dy(); y++ {
		if _, err := w.Write(img.Pix[y*img.Stride : y*img.Stride+b.Dx()]); err != nil {
			f.Close()
			return err
		}
	}
	if err := w.Flush(); err != nil {
		f.Close()
		return err
	}
	return f.Close()
}

type run struct {
	Decode, Transform, Encode, Total float64
}

func stats(v []float64) (min, med, mean float64) {
	s := append([]float64(nil), v...)
	sort.Float64s(s)
	min = s[0]
	med = s[len(s)/2]
	if len(s)%2 == 0 {
		med = (s[len(s)/2-1] + s[len(s)/2]) / 2
	}
	var sum float64
	for _, x := range s {
		sum += x
	}
	return min, med, sum / float64(len(s))
}

func ms(d time.Duration) float64 { return float64(d.Nanoseconds()) / 1e6 }

func main() {
	variant := os.Args[1] // "A" or "B"
	in := os.Args[2]
	out := os.Args[3]
	warm, iters := 3, 15
	// CPU profiling is opt-in via CPUPROFILE=<path>; the profile covers the whole
	// warm+timed loop so the phase attribution matches the reported medians.
	if pp := os.Getenv("CPUPROFILE"); pp != "" {
		pf, err := os.Create(pp)
		if err != nil {
			panic(err)
		}
		if err := pprof.StartCPUProfile(pf); err != nil {
			panic(err)
		}
		defer func() { pprof.StopCPUProfile(); pf.Close() }()
	}
	raw, err := os.ReadFile(in)
	if err != nil {
		panic(err)
	}
	var runs []run
	var ow, oh, scale int
	var imgType string
	for i := 0; i < warm+iters; i++ {
		t0 := time.Now()
		img, err := png.Decode(bytes.NewReader(raw))
		if err != nil {
			panic(err)
		}
		t1 := time.Now()
		imgType = fmt.Sprintf("%T", img)
		scale = ocrUpscaleFactorFor(img.Bounds())
		var g *image.Gray
		switch variant {
		case "A":
			g = preprocessForOCR(img, scale)
		case "B":
			g = preprocessFast(img, scale)
		case "B2":
			g = preprocessFast2(img, scale)
		case "D":
			g = preprocessD(img, scale)
		case "D1":
			g = preprocessD1(img, scale)
		case "D2":
			g = preprocessD2(img, scale)
		case "D3":
			g = preprocessD3(img, scale)
		case "D4":
			g = preprocessD4(img, scale)
		case "N":
			g = preprocessN(img, scale)
		case "M":
			g = preprocessM(img, scale)
		case "L":
			g = preprocessL(img, scale)
		case "K":
			g = preprocessK(img, scale)
		case "J":
			g = preprocessJ(img, scale)
		case "I":
			g = preprocessI(img, scale)
		case "G":
			g = preprocessG(img, scale)
		case "H":
			g = preprocessH(img, scale)
		case "E":
			g = preprocessE(img, scale)
		case "F64":
			g = preprocessF64(img, scale)
		case "F32":
			g = preprocessF32(img, scale)
		default:
			panic("unknown variant " + variant)
		}
		t2 := time.Now()
		if variant == "A" {
			err = writePreprocessedPNG(g, out)
		} else {
			err = writePGM(g, out)
		}
		if err != nil {
			panic(err)
		}
		t3 := time.Now()
		ow, oh = g.Bounds().Dx(), g.Bounds().Dy()
		if i >= warm {
			runs = append(runs, run{ms(t1.Sub(t0)), ms(t2.Sub(t1)), ms(t3.Sub(t2)), ms(t3.Sub(t0))})
		}
	}
	col := func(f func(run) float64) []float64 {
		v := make([]float64, len(runs))
		for i, r := range runs {
			v[i] = f(r)
		}
		return v
	}
	res := map[string]any{"variant": variant, "input": in, "imgType": imgType, "scale": scale, "outW": ow, "outH": oh}
	for name, f := range map[string]func(run) float64{
		"decode":    func(r run) float64 { return r.Decode },
		"transform": func(r run) float64 { return r.Transform },
		"encode":    func(r run) float64 { return r.Encode },
		"total":     func(r run) float64 { return r.Total },
	} {
		mn, md, mu := stats(col(f))
		res[name] = map[string]float64{"min": mn, "med": md, "mean": mu}
	}
	st, _ := os.Stat(out)
	res["outBytes"] = st.Size()
	res["iters"] = len(runs)
	b, _ := json.Marshal(res)
	fmt.Println(string(b))
}
