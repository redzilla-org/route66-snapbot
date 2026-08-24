// export.go — exported seam for the main (cgo) package: the GOFAST decode +
// preprocess entry points, re-exported because the asm-bearing package cannot
// itself use cgo.
package pipeline

import "image"

type Decoded = decodedLuma

func Decode(raw []byte) (*Decoded, error) { return decodePNGLuma(raw, true) }
func (d *Decoded) Size() (int, int)       { return d.width, d.height }
func Preprocess(d *Decoded, scale int) *image.Gray {
	return preprocessDecodedLuma(d, scale, true)
}
