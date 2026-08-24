// fastpng.go owns the optimized Go contestant's complete PNG-to-gray path.
// The browser screenshots in this benchmark have a deliberately narrow format
// contract, so failing hard on anything else is clearer and faster than paying
// image/png's general palette, bit-depth and interlace machinery on every row.
package pipeline

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"image"
	"unsafe"
)

// decodedLuma is an 8.8 fixed-point luma plane emitted during PNG
// reconstruction. Retaining its actual min/max preserves global contrast
// stretch for arbitrary screenshot content without a 32-bit or RGBA image.
type decodedLuma struct {
	width, height int
	values        []uint16
	min, max      uint16
	colorType     byte
}

// paeth returns PNG's exact Paeth predictor. Integer arithmetic and tie order
// match the PNG specification; changing this predictor would corrupt all later
// pixels in a filtered channel, far beyond the permitted quality tolerance.
func paeth(left, up, upperLeft byte) byte {
	p := int(left) + int(up) - int(upperLeft)
	pa := p - int(left)
	if pa < 0 {
		pa = -pa
	}
	pb := p - int(up)
	if pb < 0 {
		pb = -pb
	}
	pc := p - int(upperLeft)
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

// paeth32 is the same predictor over unpacked RGB channel values. The RGB hot
// loop keeps three independent left/up-left dependency chains in registers,
// which gives the CPU useful instruction-level parallelism across channels.
func paeth32(left, up, upperLeft uint32) uint32 {
	p := int32(left + up - upperLeft)
	pa := p - int32(left)
	if pa < 0 {
		pa = -pa
	}
	pb := p - int32(up)
	if pb < 0 {
		pb = -pb
	}
	pc := p - int32(upperLeft)
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

// reconstructLumaRGB fuses filter reversal, grayscale extraction and row
// min/max reduction. All three slices have lengths derived from the validated
// IHDR geometry; unsafe pointer stepping removes bounds checks from this hottest
// loop while every access remains within those established allocations.
func reconstructLumaRGB(current, previous []byte, output []uint16, filter byte) (uint16, uint16, error) {
	currentBase := unsafe.Pointer(unsafe.SliceData(current))
	previousBase := unsafe.Pointer(unsafe.SliceData(previous))
	outputBase := unsafe.Pointer(unsafe.SliceData(output))
	minLuma, maxLuma := ^uint16(0), uint16(0)

	// The coefficients approximate BT.601 in 8.8 fixed point within the owner's
	// delta-1/1% gate. Keeping the unrounded weighted sum preserves sub-gray-level
	// precision when a narrower source range is stretched to the full output.
	emit := func(x int, r, g, b uint32) {
		luma := uint16(77*r + 150*g + 29*b)
		*(*uint16)(unsafe.Add(outputBase, x*2)) = luma
		if luma < minLuma {
			minLuma = luma
		}
		if luma > maxLuma {
			maxLuma = luma
		}
	}

	switch filter {
	case 0:
		for x := range output {
			i := x * 3
			r := uint32(*(*byte)(unsafe.Add(currentBase, i)))
			g := uint32(*(*byte)(unsafe.Add(currentBase, i+1)))
			b := uint32(*(*byte)(unsafe.Add(currentBase, i+2)))
			emit(x, r, g, b)
		}
	case 1:
		var leftR, leftG, leftB uint32
		for x := range output {
			i := x * 3
			leftR = uint32(byte(uint32(*(*byte)(unsafe.Add(currentBase, i))) + leftR))
			leftG = uint32(byte(uint32(*(*byte)(unsafe.Add(currentBase, i+1))) + leftG))
			leftB = uint32(byte(uint32(*(*byte)(unsafe.Add(currentBase, i+2))) + leftB))
			*(*byte)(unsafe.Add(currentBase, i)) = byte(leftR)
			*(*byte)(unsafe.Add(currentBase, i+1)) = byte(leftG)
			*(*byte)(unsafe.Add(currentBase, i+2)) = byte(leftB)
			emit(x, leftR, leftG, leftB)
		}
	case 2:
		for x := range output {
			i := x * 3
			r := byte(uint32(*(*byte)(unsafe.Add(currentBase, i))) + uint32(*(*byte)(unsafe.Add(previousBase, i))))
			g := byte(uint32(*(*byte)(unsafe.Add(currentBase, i+1))) + uint32(*(*byte)(unsafe.Add(previousBase, i+1))))
			b := byte(uint32(*(*byte)(unsafe.Add(currentBase, i+2))) + uint32(*(*byte)(unsafe.Add(previousBase, i+2))))
			*(*byte)(unsafe.Add(currentBase, i)) = r
			*(*byte)(unsafe.Add(currentBase, i+1)) = g
			*(*byte)(unsafe.Add(currentBase, i+2)) = b
			emit(x, uint32(r), uint32(g), uint32(b))
		}
	case 3:
		var leftR, leftG, leftB uint32
		for x := range output {
			i := x * 3
			upR := uint32(*(*byte)(unsafe.Add(previousBase, i)))
			upG := uint32(*(*byte)(unsafe.Add(previousBase, i+1)))
			upB := uint32(*(*byte)(unsafe.Add(previousBase, i+2)))
			leftR = uint32(byte(uint32(*(*byte)(unsafe.Add(currentBase, i))) + ((leftR + upR) >> 1)))
			leftG = uint32(byte(uint32(*(*byte)(unsafe.Add(currentBase, i+1))) + ((leftG + upG) >> 1)))
			leftB = uint32(byte(uint32(*(*byte)(unsafe.Add(currentBase, i+2))) + ((leftB + upB) >> 1)))
			*(*byte)(unsafe.Add(currentBase, i)) = byte(leftR)
			*(*byte)(unsafe.Add(currentBase, i+1)) = byte(leftG)
			*(*byte)(unsafe.Add(currentBase, i+2)) = byte(leftB)
			emit(x, leftR, leftG, leftB)
		}
	case 4:
		var leftR, leftG, leftB, upperLeftR, upperLeftG, upperLeftB uint32
		for x := range output {
			i := x * 3
			upR := uint32(*(*byte)(unsafe.Add(previousBase, i)))
			upG := uint32(*(*byte)(unsafe.Add(previousBase, i+1)))
			upB := uint32(*(*byte)(unsafe.Add(previousBase, i+2)))
			leftR = uint32(byte(uint32(*(*byte)(unsafe.Add(currentBase, i))) + paeth32(leftR, upR, upperLeftR)))
			leftG = uint32(byte(uint32(*(*byte)(unsafe.Add(currentBase, i+1))) + paeth32(leftG, upG, upperLeftG)))
			leftB = uint32(byte(uint32(*(*byte)(unsafe.Add(currentBase, i+2))) + paeth32(leftB, upB, upperLeftB)))
			*(*byte)(unsafe.Add(currentBase, i)) = byte(leftR)
			*(*byte)(unsafe.Add(currentBase, i+1)) = byte(leftG)
			*(*byte)(unsafe.Add(currentBase, i+2)) = byte(leftB)
			upperLeftR, upperLeftG, upperLeftB = upR, upG, upB
			emit(x, leftR, leftG, leftB)
		}
	default:
		return 0, 0, fmt.Errorf("unsupported PNG filter %d", filter)
	}
	return minLuma, maxLuma, nil
}

// reconstructPNGRow reverses one PNG scanline filter in place. The row slices
// have equal, validated lengths and prev is zero-filled for the first row.
func reconstructPNGRow(current, previous []byte, channels int, filter byte) error {
	switch filter {
	case 0:
		// The inflater already produced the reconstructed bytes.
	case 1:
		for i := channels; i < len(current); i++ {
			current[i] += current[i-channels]
		}
	case 2:
		for i := range current {
			current[i] += previous[i]
		}
	case 3:
		for i := range current {
			var left byte
			if i >= channels {
				left = current[i-channels]
			}
			current[i] += byte((uint16(left) + uint16(previous[i])) >> 1)
		}
	case 4:
		for i := range current {
			var left, upperLeft byte
			if i >= channels {
				left = current[i-channels]
				upperLeft = previous[i-channels]
			}
			current[i] += paeth(left, previous[i], upperLeft)
		}
	default:
		return fmt.Errorf("unsupported PNG filter %d", filter)
	}
	return nil
}

// decodePNGLuma accepts the screenshot formats the product emits: 8-bit,
// non-interlaced truecolor RGB or RGBA. It parses PNG framing, inflates IDAT,
// reconstructs rows and computes approximate luma in a single decode phase.
func decodePNGLuma(raw []byte, assembly bool) (*decodedLuma, error) {
	const signature = "\x89PNG\r\n\x1a\n"
	if len(raw) < len(signature) || !bytes.Equal(raw[:len(signature)], []byte(signature)) {
		return nil, fmt.Errorf("invalid PNG signature")
	}

	// Chunk payloads remain slices of the immutable input. Browser-produced local
	// input is trusted, so chunk CRCs are consumed but deliberately not checked.
	var width, height, channels int
	var colorType byte
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
			colorType = payload[9]
			if payload[8] != 8 || payload[10] != 0 || payload[11] != 0 || payload[12] != 0 {
				return nil, fmt.Errorf("fast PNG path requires 8-bit non-interlaced input")
			}
			switch colorType {
			case 2:
				channels = 3
			case 6:
				channels = 4
			default:
				return nil, fmt.Errorf("fast PNG path requires RGB/RGBA, got color type %d", colorType)
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
	// Guard every geometry multiplication before allocating. The row decoder can
	// then use exact-length slices without carrying overflow checks into hot loops.
	maxInt := int(^uint(0) >> 1)
	if width > maxInt/channels || height > maxInt/width {
		return nil, fmt.Errorf("PNG dimensions overflow address space")
	}
	rowBytes := width * channels
	values := make([]uint16, width*height)

	// A contiguous compressed stream lets klauspost/flate select its specialized
	// bytes.Reader Huffman loop instead of the generic buffered-reader path. The
	// small IDAT copy is measured as part of decode and pays back on both fixtures.
	compressedLength := 0
	for _, segment := range idat {
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
	// The final four zlib bytes are Adler-32. Per explicit benchmark policy they
	// are skipped, and the trusted Playwright stream inflates directly into its
	// exact IHDR-derived raster without io.Reader or ring-dictionary plumbing.
	filteredLength := (rowBytes + 1) * height
	filtered := make([]byte, filteredLength+16)
	if err := inflateUnsafe(compressed[2:len(compressed)-4], filtered[:filteredLength]); err != nil {
		return nil, fmt.Errorf("inflate PNG raster: %w", err)
	}
	zeroPrevious := make([]byte, rowBytes)
	for y := 0; y < height; y++ {
		rowStart := y * (rowBytes + 1)
		filter := filtered[rowStart]
		row := filtered[rowStart+1 : rowStart+1+rowBytes]
		previous := zeroPrevious
		if y > 0 {
			previousStart := (y-1)*(rowBytes+1) + 1
			previous = filtered[previousStart : previousStart+rowBytes]
		}
		output := values[y*width : (y+1)*width]
		if channels == 3 {
			if assembly {
				if err := reconstructRGBFast(row, previous, filter); err != nil {
					return nil, err
				}
				extractLumaRGBFast(row, output)
			} else {
				if err := reconstructPNGRow(row, previous, 3, filter); err != nil {
					return nil, err
				}
				if _, _, err := reconstructLumaRGB(row, row, output, 0); err != nil {
					return nil, err
				}
			}
		} else {
			// RGBA is outside the two measured fixtures. Keep its generic filter
			// path and exact premultiplication rather than duplicating a cold loop.
			if err := reconstructPNGRow(row, previous, channels, filter); err != nil {
				return nil, err
			}
			for x := range output {
				i := x * 4
				a := uint32(row[i+3])
				r := uint32(row[i]) * a / 255
				g := uint32(row[i+1]) * a / 255
				b := uint32(row[i+2]) * a / 255
				luma := uint16(77*r + 150*g + 29*b)
				output[x] = luma
			}
		}
	}
	var minLuma, maxLuma uint16
	if assembly {
		minLuma, maxLuma = minMaxLumaFast(values)
	} else {
		minLuma, maxLuma = minMaxLumaScalar(values)
	}
	return &decodedLuma{width: width, height: height, values: values, min: minLuma, max: maxLuma, colorType: colorType}, nil
}

// minMaxLumaScalar is the pure-Go control for the SIMD image-wide reduction.
// Keeping it beside the decoder makes GOUNSAFE a reproducible ablation rather
// than a separately patched benchmark binary.
func minMaxLumaScalar(values []uint16) (uint16, uint16) {
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

// lumaStretchTable applies round-half-up global contrast normalization over the
// actual observed range. The 8.8 luma domain bounds this table to 65281 bytes.
func lumaStretchTable(minLuma, maxLuma uint16) []byte {
	rangeLuma := uint32(maxLuma - minLuma)
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

// horizontalLuma2x combines stretch lookup and horizontal interpolation while
// keeping only two u16 rows live for the vertical pass.
func horizontalLuma2x(source []uint16, minLuma uint16, table []byte, output []uint16) {
	w := len(source)
	output[0] = uint16(table[source[0]-minLuma]) * 4
	output[len(output)-1] = uint16(table[source[w-1]-minLuma]) * 4
	for x := 0; x+1 < w; x++ {
		a := uint16(table[source[x]-minLuma])
		b := uint16(table[source[x+1]-minLuma])
		output[2*x+1] = 3*a + b
		output[2*x+2] = a + 3*b
	}
}

// horizontalFullRange2x is the same interpolation when dynamic min/max proves
// stretch is full-range. Arithmetic replaces four dependent LUT loads per pair.
func horizontalFullRange2x(source []uint16, output []uint16, assembly bool) {
	if assembly {
		horizontalFullRange2xSIMD(source, output)
		return
	}
	output[0] = uint16((source[0]+128)>>8) * 4
	output[len(output)-1] = uint16((source[len(source)-1]+128)>>8) * 4
	for x := 0; x+1 < len(source); x++ {
		a := uint16((source[x] + 128) >> 8)
		b := uint16((source[x+1] + 128) >> 8)
		output[2*x+1] = 3*a + b
		output[2*x+2] = a + 3*b
	}
}

// upscaleDecodedLuma2x streams the separable factor-2 scaler through two row
// buffers. Producing both vertical rows together halves intermediate reads and
// removes the old ow*h middle allocation without changing interpolation math.
func upscaleDecodedLuma2x(decoded *decodedLuma, table []byte, fullRange, assembly bool) *image.Gray {
	w, h := decoded.width, decoded.height
	ow, oh := w*2, h*2
	destination := image.NewGray(image.Rect(0, 0, ow, oh))
	current, next := make([]uint16, ow), make([]uint16, ow)
	if fullRange {
		horizontalFullRange2x(decoded.values[:w], current, assembly)
	} else {
		horizontalLuma2x(decoded.values[:w], decoded.min, table, current)
	}

	// Clamped top and bottom rows reduce exactly to rounding the 4x horizontal
	// intermediate back to a byte.
	top := destination.Pix[:ow]
	for x := range top {
		top[x] = byte((current[x] + 2) >> 2)
	}
	for y := 0; y+1 < h; y++ {
		if fullRange {
			horizontalFullRange2x(decoded.values[(y+1)*w:(y+2)*w], next, assembly)
		} else {
			horizontalLuma2x(decoded.values[(y+1)*w:(y+2)*w], decoded.min, table, next)
		}
		upper := destination.Pix[(2*y+1)*ow : (2*y+2)*ow]
		lower := destination.Pix[(2*y+2)*ow : (2*y+3)*ow]
		if fullRange && assembly {
			verticalRows2xSIMD(current, next, upper, lower)
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

// preprocessDecodedLuma applies global stretch and maps the decoder result to
// the same Gray output contract as every other contestant.
func preprocessDecodedLuma(decoded *decodedLuma, scale int, assembly bool) *image.Gray {
	fullRange := decoded.min == 0 && decoded.max == 255*256
	var table []byte
	if !fullRange {
		table = lumaStretchTable(decoded.min, decoded.max)
	}
	if scale == 2 {
		return upscaleDecodedLuma2x(decoded, table, fullRange, assembly)
	}
	gray := image.NewGray(image.Rect(0, 0, decoded.width, decoded.height))
	if fullRange && assembly {
		lumaToGrayFullRange(decoded.values, gray.Pix)
	} else if fullRange {
		for i, luma := range decoded.values {
			gray.Pix[i] = byte((luma + 128) >> 8)
		}
	} else {
		for i, luma := range decoded.values {
			gray.Pix[i] = table[luma-decoded.min]
		}
	}
	if scale <= 1 {
		return gray
	}
	return upscaleBilinearSeparable(gray, scale)
}
