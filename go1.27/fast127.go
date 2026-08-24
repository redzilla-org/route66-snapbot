//go:build go1.27 && goexperiment.simd

// fast127.go owns the independent Go 1.27 SIMD contestant. It deliberately
// contains no handwritten assembly: every vector instruction comes from the
// experimental simd APIs, while dependency-bound loops remain unsafe Go.
package main

import (
	"bytes"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"image"
	"os"
	"runtime/pprof"
	"simd"
	"simd/archsimd"
	"sort"
	"strconv"
	"time"
	"unsafe"
)

const (
	ocrUpscaleFactor      = 3
	ocrUpscalePixelBudget = 20_000_000
)

// decodedLuma retains the 8.8 fixed-point plane so global contrast stretch and
// factor selection stay byte-identical to the assembly-assisted contestant.
type decodedLuma struct {
	width, height int
	values        []uint16
	min, max      uint16
}

// These 128-bit constants reproduce the existing SSSE3 luma layout. The API
// lowers PermuteOrZero to VPSHUFB and DotProductPairsSaturated to VPMADDUBSW.
var (
	rbShuffle127 = archsimd.LoadInt8x16([]int8{0, 2, 3, 5, 6, 8, 9, 11, -1, -1, -1, -1, -1, -1, -1, -1})
	gShuffle127  = archsimd.LoadInt8x16([]int8{1, -1, 4, -1, 7, -1, 10, -1, -1, -1, -1, -1, -1, -1, -1, -1})
	rbWeights127 = archsimd.LoadInt8x16([]int8{77, 29, 77, 29, 77, 29, 77, 29, 0, 0, 0, 0, 0, 0, 0, 0})
	gWeights127  = archsimd.LoadInt8x16([]int8{75, 0, 75, 0, 75, 0, 75, 0, 0, 0, 0, 0, 0, 0, 0, 0})
	packBytes127 = archsimd.LoadInt8x16([]int8{0, 2, 4, 6, 8, 10, 12, 14, -1, -1, -1, -1, -1, -1, -1, -1})
)

// ocrUpscaleFactorFor applies the same 20-million-output-pixel budget as every
// other contender, yielding factor 2 for typical and factor 1 for big.
func ocrUpscaleFactorFor(bounds image.Rectangle) int {
	pixels := bounds.Dx() * bounds.Dy()
	if pixels <= 0 {
		return ocrUpscaleFactor
	}
	for factor := ocrUpscaleFactor; factor > 1; factor-- {
		if pixels*factor*factor <= ocrUpscalePixelBudget {
			return factor
		}
	}
	return 1
}

// paeth127 implements PNG's exact predictor. Tie priority is left, up, then
// upper-left; changing it would corrupt the remainder of the filtered row.
func paeth127(left, up, upperLeft byte) byte {
	p := int(left) + int(up) - int(upperLeft)
	pa, pb, pc := p-int(left), p-int(up), p-int(upperLeft)
	if pa < 0 {
		pa = -pa
	}
	if pb < 0 {
		pb = -pb
	}
	if pc < 0 {
		pc = -pc
	}
	if pa <= pb && pa <= pc {
		return left
	}
	if pb <= pc {
		return up
	}
	return upperLeft
}

// filterUp127 is the portable portion of the experiment. The compiler emits a
// vector-width-specific implementation and dispatches to the host's best SIMD
// width; the scalar tail handles arbitrary PNG row lengths.
func filterUp127(current, previous []byte) {
	var probe simd.Uint8s
	lanes := probe.Len()
	i := 0
	for ; i+lanes <= len(current); i += lanes {
		simd.LoadUint8s(current[i:]).Add(simd.LoadUint8s(previous[i:])).Store(current[i:])
	}
	currentBase := unsafe.Pointer(unsafe.SliceData(current))
	previousBase := unsafe.Pointer(unsafe.SliceData(previous))
	for ; i < len(current); i++ {
		value := *(*byte)(unsafe.Add(currentBase, i)) + *(*byte)(unsafe.Add(previousBase, i))
		*(*byte)(unsafe.Add(currentBase, i)) = value
	}
}

// filterPaethRGB127 keeps the three independent RGB dependency chains in SIMD
// lanes. It advances one pixel at a time because PNG Paeth depends on the
// reconstructed pixel to its left, but computes all predictor distances and
// tie-priority selections together through Go 1.27 archsimd operations.
func filterPaethRGB127(current, previous []byte) {
	currentBase := unsafe.Pointer(unsafe.SliceData(current))
	previousBase := unsafe.Pointer(unsafe.SliceData(previous))
	byteMask := archsimd.BroadcastUint16x8(255)
	var left, upperLeft archsimd.Uint16x8
	for i := 0; i < len(current); i += 3 {
		upBytes := archsimd.LoadUint8x16Array((*[16]uint8)(unsafe.Add(previousBase, i)))
		up := upBytes.ExtendLo8ToUint16()
		upMinusUpperLeft := up.Sub(upperLeft).BitsToInt16()
		leftMinusUpperLeft := left.Sub(upperLeft).BitsToInt16()
		leftDistance := upMinusUpperLeft.Abs()
		upDistance := leftMinusUpperLeft.Abs()
		upperLeftDistance := upMinusUpperLeft.Add(leftMinusUpperLeft).Abs()
		minimum := leftDistance.Min(upDistance).Min(upperLeftDistance)

		// Start with upper-left, select up on its winning distance, then select
		// left last so equal distances preserve PNG's required tie priority.
		predictor := up.IfElse(minimum.Equal(upDistance), upperLeft)
		predictor = left.IfElse(minimum.Equal(leftDistance), predictor)
		residualBytes := archsimd.LoadUint8x16Array((*[16]uint8)(unsafe.Add(currentBase, i)))
		reconstructed := residualBytes.ExtendLo8ToUint16().Add(predictor).And(byteMask)
		packed := reconstructed.ReshapeToUint8s()
		*(*byte)(unsafe.Add(currentBase, i)) = packed.GetElem(0)
		*(*byte)(unsafe.Add(currentBase, i+1)) = packed.GetElem(2)
		*(*byte)(unsafe.Add(currentBase, i+2)) = packed.GetElem(4)
		left, upperLeft = reconstructed, up
	}
}

// reconstructRow127 reverses filters in place. Sub, Average and Paeth carry a
// horizontal dependency, so unsafe scalar pointer walks avoid bounds checks
// without pretending those filters are row-parallel.
func reconstructRow127(current, previous []byte, channels int, filter byte) error {
	if filter == 2 {
		filterUp127(current, previous)
		return nil
	}
	if filter == 4 && channels == 3 {
		filterPaethRGB127(current, previous)
		return nil
	}
	currentBase := unsafe.Pointer(unsafe.SliceData(current))
	previousBase := unsafe.Pointer(unsafe.SliceData(previous))
	switch filter {
	case 0:
		return nil
	case 1:
		for i := channels; i < len(current); i++ {
			value := *(*byte)(unsafe.Add(currentBase, i)) + *(*byte)(unsafe.Add(currentBase, i-channels))
			*(*byte)(unsafe.Add(currentBase, i)) = value
		}
	case 3:
		for i := 0; i < len(current); i++ {
			var left byte
			if i >= channels {
				left = *(*byte)(unsafe.Add(currentBase, i-channels))
			}
			up := *(*byte)(unsafe.Add(previousBase, i))
			*(*byte)(unsafe.Add(currentBase, i)) += byte((uint16(left) + uint16(up)) >> 1)
		}
	case 4:
		for i := 0; i < len(current); i++ {
			var left, upperLeft byte
			if i >= channels {
				left = *(*byte)(unsafe.Add(currentBase, i-channels))
				upperLeft = *(*byte)(unsafe.Add(previousBase, i-channels))
			}
			up := *(*byte)(unsafe.Add(previousBase, i))
			*(*byte)(unsafe.Add(currentBase, i)) += paeth127(left, up, upperLeft)
		}
	default:
		return fmt.Errorf("unsupported PNG filter %d", filter)
	}
	return nil
}

// extractLumaRGB127 converts four packed RGB pixels per API-vector iteration.
// The unsafe array load may cross the row slice but never the padded filtered
// allocation; shuffled lanes select only the twelve bytes belonging to the row.
func extractLumaRGB127(current []byte, output []uint16) {
	currentBase := unsafe.Pointer(unsafe.SliceData(current))
	outputBase := unsafe.Pointer(unsafe.SliceData(output))
	x := 0
	for ; x+4 <= len(output); x += 4 {
		source := archsimd.LoadUint8x16Array((*[16]uint8)(unsafe.Add(currentBase, x*3)))
		rb := source.PermuteOrZero(rbShuffle127).DotProductPairsSaturated(rbWeights127)
		g := source.PermuteOrZero(gShuffle127).DotProductPairsSaturated(gWeights127)
		luma := rb.Add(g.Add(g)).ToBits()
		if x+8 <= len(output) {
			luma.StoreArray((*[8]uint16)(unsafe.Add(outputBase, x*2)))
		} else {
			luma.StorePart(output[x : x+4])
		}
	}
	for ; x < len(output); x++ {
		i := x * 3
		r := uint16(*(*byte)(unsafe.Add(currentBase, i)))
		g := uint16(*(*byte)(unsafe.Add(currentBase, i+1)))
		b := uint16(*(*byte)(unsafe.Add(currentBase, i+2)))
		*(*uint16)(unsafe.Add(outputBase, x*2)) = 77*r + 150*g + 29*b
	}
}

// minMaxLuma127 uses the portable vector API for the image-wide reduction.
// Storing one final vector of lanes is cheaper than branching per source pixel.
func minMaxLuma127(values []uint16) (uint16, uint16) {
	minimum := simd.BroadcastUint16s(^uint16(0))
	maximum := simd.BroadcastUint16s(0)
	lanes := minimum.Len()
	i := 0
	for ; i+lanes <= len(values); i += lanes {
		value := simd.LoadUint16s(values[i:])
		minimum = minimum.Min(value)
		maximum = maximum.Max(value)
	}
	var minimumLanes, maximumLanes [32]uint16
	minimum.Store(minimumLanes[:lanes])
	maximum.Store(maximumLanes[:lanes])
	minValue, maxValue := minimumLanes[0], maximumLanes[0]
	for lane := 1; lane < lanes; lane++ {
		minValue = min(minValue, minimumLanes[lane])
		maxValue = max(maxValue, maximumLanes[lane])
	}
	for ; i < len(values); i++ {
		minValue = min(minValue, values[i])
		maxValue = max(maxValue, values[i])
	}
	return minValue, maxValue
}

// decodePNGLuma127 accepts only the browser screenshot contract: 8-bit,
// non-interlaced RGB or RGBA. Unsupported encodings and malformed structure
// fail hard; chunk CRC and Adler validation remain deliberately omitted.
func decodePNGLuma127(raw []byte) (*decodedLuma, error) {
	const signature = "\x89PNG\r\n\x1a\n"
	if len(raw) < len(signature) || !bytes.Equal(raw[:len(signature)], []byte(signature)) {
		return nil, fmt.Errorf("invalid PNG signature")
	}

	// Chunk payloads remain slices of the immutable PNG. Geometry and framing
	// are validated before any exact-size raster allocation or unsafe hot loop.
	var width, height, channels int
	var idat [][]byte
	seenIHDR := false
	for offset := len(signature); ; {
		if offset > len(raw)-12 {
			return nil, fmt.Errorf("truncated PNG chunk header")
		}
		length := int(binary.BigEndian.Uint32(raw[offset : offset+4]))
		if length < 0 || length > len(raw)-offset-12 {
			return nil, fmt.Errorf("truncated PNG chunk payload")
		}
		kind := string(raw[offset+4 : offset+8])
		payload := raw[offset+8 : offset+8+length]
		offset += 12 + length
		switch kind {
		case "IHDR":
			if seenIHDR || length != 13 {
				return nil, fmt.Errorf("invalid PNG IHDR")
			}
			seenIHDR = true
			width = int(binary.BigEndian.Uint32(payload[0:4]))
			height = int(binary.BigEndian.Uint32(payload[4:8]))
			if payload[8] != 8 || payload[10] != 0 || payload[11] != 0 || payload[12] != 0 {
				return nil, fmt.Errorf("Go 1.27 path requires 8-bit non-interlaced input")
			}
			switch payload[9] {
			case 2:
				channels = 3
			case 6:
				channels = 4
			default:
				return nil, fmt.Errorf("Go 1.27 path requires RGB/RGBA, got color type %d", payload[9])
			}
		case "IDAT":
			if !seenIHDR {
				return nil, fmt.Errorf("PNG IDAT precedes IHDR")
			}
			idat = append(idat, payload)
		case "IEND":
			if !seenIHDR || len(idat) == 0 || width <= 0 || height <= 0 {
				return nil, fmt.Errorf("incomplete PNG")
			}
			goto decode
		}
	}

decode:
	maxInt := int(^uint(0) >> 1)
	if width > maxInt/channels || height > maxInt/width {
		return nil, fmt.Errorf("PNG dimensions overflow address space")
	}
	rowBytes := width * channels
	if rowBytes == maxInt || height > maxInt/(rowBytes+1) {
		return nil, fmt.Errorf("PNG filtered raster overflows address space")
	}
	values := make([]uint16, width*height)

	// Concatenating IDAT preserves the same one-shot unsafe inflater contract as
	// GOFAST while keeping the copy inside the measured decode phase.
	compressedLength := 0
	for _, segment := range idat {
		if len(segment) > maxInt-compressedLength {
			return nil, fmt.Errorf("PNG IDAT length overflows address space")
		}
		compressedLength += len(segment)
	}
	compressed := make([]byte, 0, compressedLength)
	for _, segment := range idat {
		compressed = append(compressed, segment...)
	}
	if len(compressed) < 6 {
		return nil, fmt.Errorf("truncated PNG zlib stream")
	}
	cmf, flg := compressed[0], compressed[1]
	if cmf&0x0f != 8 || (uint16(cmf)<<8|uint16(flg))%31 != 0 || flg&0x20 != 0 {
		return nil, fmt.Errorf("unsupported PNG zlib header")
	}
	filteredLength := (rowBytes + 1) * height
	filtered := make([]byte, filteredLength+16)
	if err := inflateUnsafe(compressed[2:len(compressed)-4], filtered[:filteredLength]); err != nil {
		return nil, fmt.Errorf("inflate PNG raster: %w", err)
	}

	// Filtering and luma extraction stay separate to expose their vector shapes
	// cleanly to the experimental compiler and preserve exact phase ownership.
	zeroPrevious := make([]byte, rowBytes)
	for y := 0; y < height; y++ {
		rowStart := y * (rowBytes + 1)
		row := filtered[rowStart+1 : rowStart+1+rowBytes]
		previous := zeroPrevious
		if y > 0 {
			previousStart := (y-1)*(rowBytes+1) + 1
			previous = filtered[previousStart : previousStart+rowBytes]
		}
		if err := reconstructRow127(row, previous, channels, filtered[rowStart]); err != nil {
			return nil, err
		}
		output := values[y*width : (y+1)*width]
		if channels == 3 {
			extractLumaRGB127(row, output)
			continue
		}
		for x := range output {
			i := x * 4
			a := uint32(row[i+3])
			r := uint32(row[i]) * a / 255
			g := uint32(row[i+1]) * a / 255
			b := uint32(row[i+2]) * a / 255
			output[x] = uint16(77*r + 150*g + 29*b)
		}
	}
	minimum, maximum := minMaxLuma127(values)
	return &decodedLuma{width: width, height: height, values: values, min: minimum, max: maximum}, nil
}

// lumaStretchTable127 performs round-half-up normalization over the observed
// 8.8 range. A zero range maps to black without dividing by zero.
func lumaStretchTable127(minimum, maximum uint16) []byte {
	rangeLuma := uint32(maximum - minimum)
	span := uint64(rangeLuma)
	if span == 0 {
		span = 1
	}
	table := make([]byte, int(rangeLuma)+1)
	for i := range table {
		value := (2*uint64(i)*255 + span) / (2 * span)
		if value > 255 {
			value = 255
		}
		table[i] = byte(value)
	}
	return table
}

// packLowBytes127 compacts the low byte of eight uint16 lanes with VPSHUFB.
// Full stores deliberately zero the following eight bytes, which the next
// vector overwrites; only the row tail pays for the generic partial-store path.
func packLowBytes127(values archsimd.Uint16x8, output []byte) {
	packed := values.ReshapeToUint8s().PermuteOrZero(packBytes127)
	if len(output) >= 16 {
		packed.StoreArray((*[16]uint8)(unsafe.Pointer(unsafe.SliceData(output))))
		return
	}
	packed.StorePart(output)
}

// lumaToGray127 rounds 8.8 luma and emits eight bytes per API-vector. The tail
// is unsafe scalar Go so arbitrary image widths keep exact output.
func lumaToGray127(values []uint16, output []byte) {
	round := archsimd.BroadcastUint16x8(128)
	i := 0
	for ; i+8 <= len(values); i += 8 {
		shifted := archsimd.LoadUint16x8(values[i:]).Add(round).ShiftAllRight(8)
		packLowBytes127(shifted, output[i:])
	}
	valuesBase := unsafe.Pointer(unsafe.SliceData(values))
	outputBase := unsafe.Pointer(unsafe.SliceData(output))
	for ; i < len(values); i++ {
		value := *(*uint16)(unsafe.Add(valuesBase, i*2))
		*(*byte)(unsafe.Add(outputBase, i)) = byte((value + 128) >> 8)
	}
}

// horizontalLuma2x127 handles non-full-range images through the exact lookup
// table. There is no gather in the portable Go 1.27 API, so this cold path is
// intentionally scalar rather than staging a slower pseudo-vector gather.
func horizontalLuma2x127(source []uint16, minimum uint16, table []byte, output []uint16) {
	output[0] = uint16(table[source[0]-minimum]) * 4
	output[len(output)-1] = uint16(table[source[len(source)-1]-minimum]) * 4
	for x := 0; x+1 < len(source); x++ {
		a := uint16(table[source[x]-minimum])
		b := uint16(table[source[x+1]-minimum])
		output[2*x+1] = 3*a + b
		output[2*x+2] = a + 3*b
	}
}

// horizontalFullRange2x127 maps the current SSE2 kernel directly to official
// intrinsics: rounded loads, weighted adds, and low/high word interleaves.
func horizontalFullRange2x127(source, output []uint16) {
	round := archsimd.BroadcastUint16x8(128)
	output[0] = ((source[0] + 128) >> 8) * 4
	output[len(output)-1] = ((source[len(source)-1] + 128) >> 8) * 4
	x := 0
	for ; x+8 < len(source); x += 8 {
		a := archsimd.LoadUint16x8(source[x:]).Add(round).ShiftAllRight(8)
		b := archsimd.LoadUint16x8(source[x+1:]).Add(round).ShiftAllRight(8)
		threeAPlusB := a.Add(a).Add(a).Add(b)
		aPlusThreeB := b.Add(b).Add(b).Add(a)
		start := 2*x + 1
		threeAPlusB.InterleaveLo(aPlusThreeB).Store(output[start : start+8])
		threeAPlusB.InterleaveHi(aPlusThreeB).Store(output[start+8 : start+16])
	}
	for ; x+1 < len(source); x++ {
		a := (source[x] + 128) >> 8
		b := (source[x+1] + 128) >> 8
		output[2*x+1] = 3*a + b
		output[2*x+2] = a + 3*b
	}
}

// verticalRows2x127 consumes each pair of horizontal intermediates once. API
// shuffles compact the shifted words without calling a local assembly symbol.
func verticalRows2x127(current, next []uint16, upper, lower []byte) {
	round := archsimd.BroadcastUint16x8(8)
	x := 0
	for ; x+8 <= len(current); x += 8 {
		a := archsimd.LoadUint16x8(current[x:])
		b := archsimd.LoadUint16x8(next[x:])
		upperValues := a.Add(a).Add(a).Add(b).Add(round).ShiftAllRight(4)
		lowerValues := b.Add(b).Add(b).Add(a).Add(round).ShiftAllRight(4)
		packLowBytes127(upperValues, upper[x:])
		packLowBytes127(lowerValues, lower[x:])
	}
	for ; x < len(current); x++ {
		a, b := uint32(current[x]), uint32(next[x])
		upper[x] = byte((3*a + b + 8) >> 4)
		lower[x] = byte((a + 3*b + 8) >> 4)
	}
}

// upscaleDecodedLuma2x127 streams the separable scaler through two uint16 row
// buffers, keeping the same allocation shape and interpolation as GOFAST.
func upscaleDecodedLuma2x127(decoded *decodedLuma, table []byte, fullRange bool) *image.Gray {
	w, h := decoded.width, decoded.height
	ow, oh := w*2, h*2
	destination := image.NewGray(image.Rect(0, 0, ow, oh))
	current, next := make([]uint16, ow), make([]uint16, ow)
	if fullRange {
		horizontalFullRange2x127(decoded.values[:w], current)
	} else {
		horizontalLuma2x127(decoded.values[:w], decoded.min, table, current)
	}
	for x := range destination.Pix[:ow] {
		destination.Pix[x] = byte((current[x] + 2) >> 2)
	}
	for y := 0; y+1 < h; y++ {
		if fullRange {
			horizontalFullRange2x127(decoded.values[(y+1)*w:(y+2)*w], next)
		} else {
			horizontalLuma2x127(decoded.values[(y+1)*w:(y+2)*w], decoded.min, table, next)
		}
		upper := destination.Pix[(2*y+1)*ow : (2*y+2)*ow]
		lower := destination.Pix[(2*y+2)*ow : (2*y+3)*ow]
		if fullRange {
			verticalRows2x127(current, next, upper, lower)
		} else {
			for x := range upper {
				a, b := uint32(current[x]), uint32(next[x])
				upper[x] = byte((3*a + b + 8) >> 4)
				lower[x] = byte((a + 3*b + 8) >> 4)
			}
		}
		current, next = next, current
	}
	bottom := destination.Pix[(oh-1)*ow : oh*ow]
	for x := range bottom {
		bottom[x] = byte((current[x] + 2) >> 2)
	}
	return destination
}

// The generic scaler keeps this contender valid for every integer scale factor
// selected by the shared benchmark contract. The factor-2 production path above
// remains specialized because it can fuse stretch and scaling without this
// intermediate plane.
const fixShift127 = 16
const fixOne127 = 1 << fixShift127

// bilinearAxis127 precomputes source indices and 16.16 weights so the pixel
// loops contain no division, floor, or edge clamping. The coordinate convention
// is identical to the established Go implementation, preserving its output.
func bilinearAxis127(n, scale int) (i0s, i1s []int32, weights []uint32) {
	outputSize := n * scale
	i0s = make([]int32, outputSize)
	i1s = make([]int32, outputSize)
	weights = make([]uint32, outputSize)
	for output := 0; output < outputSize; output++ {
		// This is the integer form of (output+0.5)/scale - 0.5.
		numerator := 2*output + 1 - scale
		denominator := 2 * scale
		floor := numerator / denominator
		remainder := numerator - floor*denominator
		if remainder < 0 {
			// Go division truncates toward zero, while interpolation requires floor.
			floor--
			remainder += denominator
		}
		first, second := floor, floor+1
		if first < 0 {
			first = 0
		} else if first >= n {
			first = n - 1
		}
		if second < 0 {
			second = 0
		} else if second >= n {
			second = n - 1
		}
		i0s[output], i1s[output] = int32(first), int32(second)
		weights[output] = uint32((int64(remainder)*fixOne127 + int64(denominator)/2) / int64(denominator))
	}
	return
}

// upscaleBilinearSeparable127 handles uncommon integer scale factors without
// specializing around the current fixtures. The horizontal intermediate keeps
// full 16.16 precision; rounding happens once after vertical interpolation.
func upscaleBilinearSeparable127(source *image.Gray, scale int) *image.Gray {
	width, height := source.Bounds().Dx(), source.Bounds().Dy()
	outputWidth, outputHeight := width*scale, height*scale
	destination := image.NewGray(image.Rect(0, 0, outputWidth, outputHeight))

	x0s, x1s, xWeights := bilinearAxis127(width, scale)
	y0s, y1s, yWeights := bilinearAxis127(height, scale)

	// Retaining the weighted horizontal sum avoids an intermediate rounding step
	// that could otherwise move a final gray value by one.
	intermediate := make([]uint32, outputWidth*height)
	for y := 0; y < height; y++ {
		input := source.Pix[y*source.Stride : y*source.Stride+width]
		output := intermediate[y*outputWidth : (y+1)*outputWidth]
		for x := range output {
			weight := xWeights[x]
			output[x] = uint32(input[x0s[x]])*(uint32(fixOne127)-weight) + uint32(input[x1s[x]])*weight
		}
	}
	for y := 0; y < outputHeight; y++ {
		weight := uint64(yWeights[y])
		inverseWeight := uint64(fixOne127) - weight
		first := intermediate[int(y0s[y])*outputWidth : (int(y0s[y])+1)*outputWidth]
		second := intermediate[int(y1s[y])*outputWidth : (int(y1s[y])+1)*outputWidth]
		output := destination.Pix[y*destination.Stride : y*destination.Stride+outputWidth]
		for x := range output {
			output[x] = uint8((uint64(first[x])*inverseWeight + uint64(second[x])*weight + (1 << (2*fixShift127 - 1))) >> (2 * fixShift127))
		}
	}
	return destination
}

// preprocessDecodedLuma127 maps the decoded plane to the shared Gray contract.
func preprocessDecodedLuma127(decoded *decodedLuma, scale int) *image.Gray {
	fullRange := decoded.min == 0 && decoded.max == 255*256
	var table []byte
	if !fullRange {
		table = lumaStretchTable127(decoded.min, decoded.max)
	}
	if scale == 2 {
		return upscaleDecodedLuma2x127(decoded, table, fullRange)
	}
	gray := image.NewGray(image.Rect(0, 0, decoded.width, decoded.height))
	if fullRange {
		lumaToGray127(decoded.values, gray.Pix)
	} else {
		for i, luma := range decoded.values {
			gray.Pix[i] = table[luma-decoded.min]
		}
	}
	if scale > 1 {
		return upscaleBilinearSeparable127(gray, scale)
	}
	return gray
}

// writePGM127 writes the tightly packed plane directly. A stride fallback keeps
// the helper correct if a future contender returns an image subregion.
func writePGM127(gray *image.Gray, path string) error {
	file, err := os.Create(path)
	if err != nil {
		return err
	}
	bounds := gray.Bounds()
	header := make([]byte, 0, 32)
	header = append(header, "P5\n"...)
	header = strconv.AppendInt(header, int64(bounds.Dx()), 10)
	header = append(header, ' ')
	header = strconv.AppendInt(header, int64(bounds.Dy()), 10)
	header = append(header, "\n255\n"...)
	if _, err := file.Write(header); err != nil {
		file.Close()
		return err
	}
	if gray.Stride == bounds.Dx() {
		if _, err := file.Write(gray.Pix[:bounds.Dx()*bounds.Dy()]); err != nil {
			file.Close()
			return err
		}
	} else {
		for y := 0; y < bounds.Dy(); y++ {
			if _, err := file.Write(gray.Pix[y*gray.Stride : y*gray.Stride+bounds.Dx()]); err != nil {
				file.Close()
				return err
			}
		}
	}
	return file.Close()
}

// run127 records complete per-iteration phases so the shared paired harness can
// compare this process without summing measurements from separate programs.
type run127 struct {
	decode, transform, encode, total float64
}

// stats127 reports the same minimum, median and mean schema as all contenders.
func stats127(values []float64) (minimum, median, mean float64) {
	sorted := append([]float64(nil), values...)
	sort.Float64s(sorted)
	minimum = sorted[0]
	median = sorted[len(sorted)/2]
	if len(sorted)%2 == 0 {
		median = (sorted[len(sorted)/2-1] + sorted[len(sorted)/2]) / 2
	}
	for _, value := range sorted {
		mean += value
	}
	return minimum, median, mean / float64(len(sorted))
}

func milliseconds127(duration time.Duration) float64 {
	return float64(duration.Nanoseconds()) / 1e6
}

// main executes three warmups followed by complete iterations for a 250 ms
// time box. PNG bytes are read once because filesystem input is outside the
// benchmark's decode-transform-PGM contract.
func main() {
	if len(os.Args) != 4 || os.Args[1] != "GO127" {
		panic("usage: go127bench.exe GO127 input.png output.pgm")
	}
	input, output := os.Args[2], os.Args[3]
	if profile := os.Getenv("CPUPROFILE"); profile != "" {
		file, err := os.Create(profile)
		if err != nil {
			panic(err)
		}
		if err := pprof.StartCPUProfile(file); err != nil {
			panic(err)
		}
		defer func() {
			pprof.StopCPUProfile()
			file.Close()
		}()
	}
	raw, err := os.ReadFile(input)
	if err != nil {
		panic(err)
	}

	// Timed samples include decode, complete transform, and complete PGM write.
	const warmups = 3
	const timedFor = 250 * time.Millisecond
	var runs []run127
	var timedStart time.Time
	var width, height, scale int
	for iteration := 0; ; iteration++ {
		start := time.Now()
		if iteration == warmups {
			timedStart = start
		}
		decoded, err := decodePNGLuma127(raw)
		if err != nil {
			panic(err)
		}
		decodedAt := time.Now()
		scale = ocrUpscaleFactorFor(image.Rect(0, 0, decoded.width, decoded.height))
		gray := preprocessDecodedLuma127(decoded, scale)
		transformedAt := time.Now()
		if err := writePGM127(gray, output); err != nil {
			panic(err)
		}
		finishedAt := time.Now()
		width, height = gray.Bounds().Dx(), gray.Bounds().Dy()
		if iteration >= warmups {
			runs = append(runs, run127{
				decode:    milliseconds127(decodedAt.Sub(start)),
				transform: milliseconds127(transformedAt.Sub(decodedAt)),
				encode:    milliseconds127(finishedAt.Sub(transformedAt)),
				total:     milliseconds127(finishedAt.Sub(start)),
			})
			if finishedAt.Sub(timedStart) >= timedFor {
				break
			}
		}
	}

	// JSON field names intentionally match the shared language harness.
	result := map[string]any{
		"variant":  "GO127",
		"input":    input,
		"imgType":  "unsafe Go + Go 1.27 simd APIs (no handwritten assembly)",
		"simdBits": simd.VectorBitSize(),
		"emulated": simd.Emulated(),
		"scale":    scale,
		"outW":     width,
		"outH":     height,
		"iters":    len(runs),
	}
	for name, selectValue := range map[string]func(run127) float64{
		"decode":    func(run run127) float64 { return run.decode },
		"transform": func(run run127) float64 { return run.transform },
		"encode":    func(run run127) float64 { return run.encode },
		"total":     func(run run127) float64 { return run.total },
	} {
		values := make([]float64, len(runs))
		for i, run := range runs {
			values[i] = selectValue(run)
		}
		minimum, median, mean := stats127(values)
		result[name] = map[string]float64{"min": minimum, "med": median, "mean": mean}
	}
	stat, err := os.Stat(output)
	if err != nil {
		panic(err)
	}
	result["outBytes"] = stat.Size()
	encoded, err := json.Marshal(result)
	if err != nil {
		panic(err)
	}
	fmt.Println(string(encoded))
}
