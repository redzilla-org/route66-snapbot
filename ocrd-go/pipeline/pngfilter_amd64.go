package pipeline

import (
	"unsafe"

	"golang.org/x/sys/cpu"
)

// The assembly kernels receive pointers only after IHDR geometry and equal row
// lengths have been validated. noescape keeps those temporary row buffers on
// their existing allocation path instead of conservatively leaking them.
//
//go:noescape
func filterUpAVX2(current, previous unsafe.Pointer, length uintptr)

//go:noescape
func filterPaethRGBSSE41(current, previous unsafe.Pointer, pixels uintptr)

//go:noescape
func rgbLuma8p8SSSE3(current, output unsafe.Pointer, pixels uintptr)

//go:noescape
func minMaxUint16SSE41(values unsafe.Pointer, length uintptr) uint32

//go:noescape
func luma8p8ToGraySSE2(values, output unsafe.Pointer, length uintptr)

//go:noescape
func horizontalLumaFull2xSSE2(values, output unsafe.Pointer, width uintptr)

//go:noescape
func verticalRows2xSSE2(current, next, upper, lower unsafe.Pointer, length uintptr)

// reconstructRGBFast selects SIMD only where the host advertises the exact
// instruction set. Less common filters retain the generic, exact Go path.
func reconstructRGBFast(current, previous []byte, filter byte) error {
	switch {
	case filter == 2 && cpu.X86.HasAVX2:
		filterUpAVX2(unsafe.Pointer(unsafe.SliceData(current)), unsafe.Pointer(unsafe.SliceData(previous)), uintptr(len(current)))
		return nil
	case filter == 4 && cpu.X86.HasSSE41:
		filterPaethRGBSSE41(unsafe.Pointer(unsafe.SliceData(current)), unsafe.Pointer(unsafe.SliceData(previous)), uintptr(len(current)/3))
		return nil
	default:
		return reconstructPNGRow(current, previous, 3, filter)
	}
}

// extractLumaRGBFast converts four RGB pixels per SSSE3 iteration. The scalar
// fallback reuses the fused Go kernel with filter 0 on already-reconstructed data.
func extractLumaRGBFast(current []byte, output []uint16) {
	if cpu.X86.HasSSSE3 {
		rgbLuma8p8SSSE3(unsafe.Pointer(unsafe.SliceData(current)), unsafe.Pointer(unsafe.SliceData(output)), uintptr(len(output)))
		return
	}
	_, _, err := reconstructLumaRGB(current, current, output, 0)
	if err != nil {
		panic(err)
	}
}

// minMaxLumaFast performs one image-wide reduction after decode. Separating it
// from row reconstruction removes two branches from every luma emission.
func minMaxLumaFast(values []uint16) (uint16, uint16) {
	if cpu.X86.HasSSE41 {
		packed := minMaxUint16SSE41(unsafe.Pointer(unsafe.SliceData(values)), uintptr(len(values)))
		return uint16(packed), uint16(packed >> 16)
	}
	minimum, maximum := ^uint16(0), uint16(0)
	for _, value := range values {
		if value < minimum {
			minimum = value
		}
		if value > maximum {
			maximum = value
		}
	}
	return minimum, maximum
}

// lumaToGrayFullRange converts full-range 8.8 luma to round-half-up bytes.
// SSE2 is universal on amd64, so no runtime feature branch is required.
func lumaToGrayFullRange(values []uint16, output []byte) {
	luma8p8ToGraySSE2(unsafe.Pointer(unsafe.SliceData(values)), unsafe.Pointer(unsafe.SliceData(output)), uintptr(len(values)))
}

// horizontalFullRange2xSIMD emits the exact 3:1/1:3 horizontal intermediate.
func horizontalFullRange2xSIMD(values []uint16, output []uint16) {
	horizontalLumaFull2xSSE2(unsafe.Pointer(unsafe.SliceData(values)), unsafe.Pointer(unsafe.SliceData(output)), uintptr(len(values)))
}

// verticalRows2xSIMD consumes two horizontal rows once and emits both weighted
// output rows, avoiding a second read of the same intermediates.
func verticalRows2xSIMD(current, next []uint16, upper, lower []byte) {
	verticalRows2xSSE2(
		unsafe.Pointer(unsafe.SliceData(current)),
		unsafe.Pointer(unsafe.SliceData(next)),
		unsafe.Pointer(unsafe.SliceData(upper)),
		unsafe.Pointer(unsafe.SliceData(lower)),
		uintptr(len(current)),
	)
}
