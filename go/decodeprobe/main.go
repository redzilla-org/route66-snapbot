// Decode-phase attribution probe (step 3 of the optimization brief).
//
// Splits Go's PNG decode cost into its two components -- zlib inflate of the
// concatenated IDAT stream, and everything else (unfiltering, allocation,
// pixel expansion) -- and measures the same inflate under klauspost/compress,
// which is the only drop-in-quality alternative inflate available to pure Go.
// It also measures the floor: how long it takes to just read raw bytes, i.e.
// what the decode phase would cost if the producing step emitted an
// uncompressed format.
package main

import (
	"bytes"
	"compress/zlib"
	"encoding/binary"
	"fmt"
	"image/png"
	"io"
	"os"
	"sort"
	"time"

	"github.com/gameparrot/fastpng"
	kzlib "github.com/klauspost/compress/zlib"
)

func idat(raw []byte) []byte {
	var out []byte
	p := 8 // skip signature
	for p+8 <= len(raw) {
		n := int(binary.BigEndian.Uint32(raw[p:]))
		typ := string(raw[p+4 : p+8])
		if typ == "IDAT" {
			out = append(out, raw[p+8:p+8+n]...)
		}
		p += 12 + n
	}
	return out
}

func med(v []float64) float64 {
	sort.Float64s(v)
	return v[len(v)/2]
}

func timeIt(n int, f func()) float64 {
	var v []float64
	for i := 0; i < n+3; i++ {
		t := time.Now()
		f()
		if i >= 3 {
			v = append(v, float64(time.Since(t).Nanoseconds())/1e6)
		}
	}
	return med(v)
}

func main() {
	path := os.Args[1]
	raw, err := os.ReadFile(path)
	if err != nil {
		panic(err)
	}
	comp := idat(raw)

	var inflated int
	full := timeIt(15, func() {
		if _, err := png.Decode(bytes.NewReader(raw)); err != nil {
			panic(err)
		}
	})
	std := timeIt(15, func() {
		r, err := zlib.NewReader(bytes.NewReader(comp))
		if err != nil {
			panic(err)
		}
		n, err := io.Copy(io.Discard, r)
		if err != nil {
			panic(err)
		}
		inflated = int(n)
	})
	kp := timeIt(15, func() {
		r, err := kzlib.NewReader(bytes.NewReader(comp))
		if err != nil {
			panic(err)
		}
		if _, err := io.Copy(io.Discard, r); err != nil {
			panic(err)
		}
	})
	fp := timeIt(15, func() {
		if _, err := fastpng.Decode(bytes.NewReader(raw)); err != nil {
			panic(err)
		}
	})
	// Floor: the cost of moving the same number of decoded bytes with no
	// decompression and no unfiltering at all.
	blob := make([]byte, inflated)
	dstb := make([]byte, inflated)
	rawcopy := timeIt(15, func() { copy(dstb, blob) })

	fmt.Printf("%s\n", path)
	fmt.Printf("  IDAT compressed bytes : %d\n", len(comp))
	fmt.Printf("  inflated bytes        : %d\n", inflated)
	fmt.Printf("  png.Decode (full)     : %7.2f ms\n", full)
	fmt.Printf("  stdlib zlib inflate   : %7.2f ms  (%4.1f%% of decode)\n", std, 100*std/full)
	fmt.Printf("  klauspost zlib inflate: %7.2f ms  (%.2fx vs stdlib)\n", kp, std/kp)
	fmt.Printf("  remainder (unfilter+) : %7.2f ms  (%4.1f%% of decode)\n", full-std, 100*(full-std)/full)
	fmt.Printf("  fastpng.Decode (full) : %7.2f ms  (%.2fx vs image/png)\n", fp, full/fp)
	fmt.Printf("  raw byte copy floor   : %7.2f ms\n", rawcopy)
}
