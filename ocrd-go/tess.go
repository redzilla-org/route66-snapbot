// tess.go — CGO binding to tesseract's C API (capi.h), the ONLY cgo surface in
// this experiment. Links the vcpkg dynamic import libs; the DLLs must be on
// PATH at run time (same deployment posture as the Rust ocrd on Windows).
package main

/*
#cgo windows CFLAGS: -IC:/Users/alexr/vcpkg/installed/x64-windows/include
#cgo windows LDFLAGS: -LC:/Users/alexr/vcpkg/installed/x64-windows/lib -l:tesseract55.lib -l:leptonica-1.87.0.lib
#cgo linux pkg-config: tesseract lept
#include <stdlib.h>
#include <tesseract/capi.h>
#include <leptonica/allheaders.h>
*/
import "C"

import (
	"fmt"
	"image"
	"unsafe"
)

// tessEngine wraps one TessBaseAPI. NOT goroutine-safe; the pool hands each
// engine to one worker at a time.
type tessEngine struct{ h *C.TessBaseAPI }

func newTessEngine(datapath, lang string) (*tessEngine, error) {
	h := C.TessBaseAPICreate()
	cd := C.CString(datapath)
	cl := C.CString(lang)
	defer C.free(unsafe.Pointer(cd))
	defer C.free(unsafe.Pointer(cl))
	if rc := C.TessBaseAPIInit3(h, cd, cl); rc != 0 {
		C.TessBaseAPIDelete(h)
		return nil, fmt.Errorf("TessBaseAPIInit3 rc=%d datapath=%s lang=%s", int(rc), datapath, lang)
	}
	return &tessEngine{h: h}, nil
}

// recognize runs OCR over a preprocessed grayscale plane.
func (e *tessEngine) recognize(g *image.Gray, psm, dpi int) (string, error) {
	C.TessBaseAPISetPageSegMode(e.h, C.TessPageSegMode(psm))
	w, h := g.Rect.Dx(), g.Rect.Dy()
	C.TessBaseAPISetImage(e.h, (*C.uchar)(unsafe.Pointer(&g.Pix[0])), C.int(w), C.int(h), 1, C.int(g.Stride))
	C.TessBaseAPISetSourceResolution(e.h, C.int(dpi))
	txt := C.TessBaseAPIGetUTF8Text(e.h)
	if txt == nil {
		C.TessBaseAPIClear(e.h)
		return "", fmt.Errorf("GetUTF8Text returned null")
	}
	out := C.GoString(txt)
	C.TessDeleteText(txt)
	C.TessBaseAPIClear(e.h)
	return out, nil
}

// recognizeMem hands tesseract the ORIGINAL ENCODED image via leptonica, using
// its fast RGB binarization path. Used when scale==1: preprocessing then does
// no upscaling, and an 8-bit gray plane triggers a pathological tesseract
// binarization on ultra-wide pages (measured 177s vs 3.2s on a 19599x1002
// screenshot; even a plain grayscale of the same page costs 39s via the CLI).
//
// pixReadMem, not pixRead: requests carry the image bytes inline over NATS, so
// there is no file to point leptonica at — and deliberately so, since a path
// only works when daemon and caller share a filesystem, which is exactly the
// assumption the NATS transport exists to delete. leptonica sniffs the format,
// so this is also what makes JPEG input work without a second code path.
func (e *tessEngine) recognizeMem(raw []byte, psm, dpi int) (string, error) {
	if len(raw) == 0 {
		return "", fmt.Errorf("empty image")
	}
	pix := C.pixReadMem((*C.l_uint8)(unsafe.Pointer(&raw[0])), C.size_t(len(raw)))
	if pix == nil {
		return "", fmt.Errorf("pixReadMem failed (%d bytes, unrecognized image)", len(raw))
	}
	defer C.pixDestroy(&pix)
	C.TessBaseAPISetPageSegMode(e.h, C.TessPageSegMode(psm))
	C.TessBaseAPISetImage2(e.h, pix)
	C.TessBaseAPISetSourceResolution(e.h, C.int(dpi))
	txt := C.TessBaseAPIGetUTF8Text(e.h)
	if txt == nil {
		C.TessBaseAPIClear(e.h)
		return "", fmt.Errorf("GetUTF8Text returned null")
	}
	out := C.GoString(txt)
	C.TessDeleteText(txt)
	C.TessBaseAPIClear(e.h)
	return out, nil
}
