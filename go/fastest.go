// fastest.go implements the benchmark's one-shot RFC 1951 decode path.
// PNG provides the exact decompressed size through validated IHDR geometry, so
// decoding directly into that allocation avoids io.Reader dispatch, a 32 KiB
// ring dictionary and a second copy into the filtered raster.
package main

import (
	"encoding/binary"
	"fmt"
	"math/bits"
	"unsafe"
)

const unsafeFlateMaxBits = 15

// unsafeFlateTable expands every Huffman prefix to a full-width lookup entry.
// The 64 KiB table trades modest per-block setup and memory for one unchecked
// load per symbol instead of a branch through a second-level table.
type unsafeFlateTable struct {
	entries [1 << unsafeFlateMaxBits]uint16
	mask    uint32
	bits    uint
}

// unsafeFlateBits keeps prefetched input bytes in the low end of bits. The
// caller owns src for the complete decode, making base stable for unsafe loads.
type unsafeFlateBits struct {
	src      []byte
	base     unsafe.Pointer
	position int
	value    uint64
	count    uint
}

// unsafeFlateReverse reverses a canonical MSB-first Huffman code because RFC
// 1951 transmits each code least-significant bit first.
func unsafeFlateReverse(code uint32, width uint) uint32 {
	return uint32(bits.Reverse16(uint16(code)) >> (16 - width))
}

// build validates canonical code lengths before populating the lookup table.
// Incomplete trees remain partially empty and fail only if the stream selects
// an absent prefix, as required for legal one-symbol distance trees.
func (table *unsafeFlateTable) build(lengths []byte) error {
	var counts [unsafeFlateMaxBits + 1]uint32
	maxBits := uint(0)
	for _, length := range lengths {
		if length > unsafeFlateMaxBits {
			return fmt.Errorf("DEFLATE Huffman code exceeds %d bits", unsafeFlateMaxBits)
		}
		if length != 0 {
			counts[length]++
			if uint(length) > maxBits {
				maxBits = uint(length)
			}
		}
	}
	if maxBits == 0 {
		table.bits, table.mask = 0, 0
		return nil
	}

	// Canonical code-space accounting rejects oversubscribed trees before any
	// table entry can alias another symbol.
	left := int32(1)
	for bits := uint(1); bits <= unsafeFlateMaxBits; bits++ {
		left = left*2 - int32(counts[bits])
		if left < 0 {
			return fmt.Errorf("oversubscribed DEFLATE Huffman tree")
		}
	}

	var next [unsafeFlateMaxBits + 1]uint32
	code := uint32(0)
	for bits := uint(1); bits <= unsafeFlateMaxBits; bits++ {
		code = (code + counts[bits-1]) << 1
		next[bits] = code
	}

	table.bits = maxBits
	table.mask = uint32(1<<maxBits) - 1
	clear(table.entries[:1<<maxBits])
	for symbol, rawLength := range lengths {
		if rawLength == 0 {
			continue
		}
		length := uint(rawLength)
		prefix := unsafeFlateReverse(next[length], length)
		next[length]++
		entry := uint16(symbol<<4) | uint16(length)
		for suffix := uint32(0); suffix < 1<<(maxBits-length); suffix++ {
			table.entries[prefix|(suffix<<length)] = entry
		}
	}
	return nil
}

// ensure refills with an unchecked 16-bit load only when two validated bytes
// remain. The byte tail keeps truncated input on the ordinary error path.
func (reader *unsafeFlateBits) ensure(required uint) bool {
	for reader.count < required {
		remaining := len(reader.src) - reader.position
		if remaining >= 2 {
			word := *(*uint16)(unsafe.Add(reader.base, reader.position))
			reader.value |= uint64(word) << reader.count
			reader.position += 2
			reader.count += 16
			continue
		}
		if remaining == 0 {
			return false
		}
		reader.value |= uint64(*(*byte)(unsafe.Add(reader.base, reader.position))) << reader.count
		reader.position++
		reader.count += 8
	}
	return true
}

// read removes a bounded number of low bits. Every caller uses at most 16 bits,
// keeping shifts defined and the refill register below 32 live bits.
func (reader *unsafeFlateBits) read(bits uint) (uint32, bool) {
	if !reader.ensure(bits) {
		return 0, false
	}
	mask := uint64(1<<bits) - 1
	value := uint32(reader.value & mask)
	reader.value >>= bits
	reader.count -= bits
	return value, true
}

// symbol performs one unchecked table load after ensure has established enough
// source bits and build has bounded mask to the fixed entry allocation.
func (reader *unsafeFlateBits) symbol(table *unsafeFlateTable) (int, bool) {
	if table.bits == 0 || !reader.ensure(table.bits) {
		return 0, false
	}
	index := uint32(reader.value) & table.mask
	entry := *(*uint16)(unsafe.Add(unsafe.Pointer(&table.entries[0]), uintptr(index)*2))
	bits := uint(entry & 15)
	if bits == 0 {
		return 0, false
	}
	reader.value >>= bits
	reader.count -= bits
	return int(entry >> 4), true
}

// alignStored returns prefetched whole bytes to the source cursor after
// discarding the current block's padding bits. Stored blocks can then use one
// checked bulk copy instead of eight bit reads per byte.
func (reader *unsafeFlateBits) alignStored() {
	drop := reader.count & 7
	reader.value >>= drop
	reader.count -= drop
	reader.position -= int(reader.count / 8)
	reader.value, reader.count = 0, 0
}

var unsafeFlateFixedLiteral, unsafeFlateFixedDistance unsafeFlateTable

// Fixed trees are process constants. Building them during init keeps setup out
// of every timed image iteration while retaining the same validated builder.
func init() {
	literalLengths := make([]byte, 288)
	for i := 0; i <= 143; i++ {
		literalLengths[i] = 8
	}
	for i := 144; i <= 255; i++ {
		literalLengths[i] = 9
	}
	for i := 256; i <= 279; i++ {
		literalLengths[i] = 7
	}
	for i := 280; i <= 287; i++ {
		literalLengths[i] = 8
	}
	distanceLengths := make([]byte, 32)
	for i := range distanceLengths {
		distanceLengths[i] = 5
	}
	if err := unsafeFlateFixedLiteral.build(literalLengths); err != nil {
		panic(err)
	}
	if err := unsafeFlateFixedDistance.build(distanceLengths); err != nil {
		panic(err)
	}
}

var unsafeFlateCodeLengthOrder = [...]byte{16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15}

// unsafeFlateDynamic reads RFC 1951's run-length encoded code-length alphabet.
// All repeat counts are checked against the declared combined tree size before
// writes, so malformed streams never reach unchecked symbol loads with bad data.
func unsafeFlateDynamic(reader *unsafeFlateBits, literal, distance *unsafeFlateTable) error {
	hlitRaw, ok := reader.read(5)
	if !ok {
		return fmt.Errorf("truncated DEFLATE dynamic header")
	}
	hdistRaw, ok := reader.read(5)
	if !ok {
		return fmt.Errorf("truncated DEFLATE dynamic header")
	}
	hclenRaw, ok := reader.read(4)
	if !ok {
		return fmt.Errorf("truncated DEFLATE dynamic header")
	}
	hlit, hdist, hclen := int(hlitRaw)+257, int(hdistRaw)+1, int(hclenRaw)+4
	if hlit > 286 || hdist > 32 {
		return fmt.Errorf("invalid DEFLATE tree dimensions")
	}

	var codeLengths [19]byte
	for i := 0; i < hclen; i++ {
		value, ok := reader.read(3)
		if !ok {
			return fmt.Errorf("truncated DEFLATE code-length tree")
		}
		codeLengths[unsafeFlateCodeLengthOrder[i]] = byte(value)
	}
	var codeTable unsafeFlateTable
	if err := codeTable.build(codeLengths[:]); err != nil {
		return err
	}

	lengths := make([]byte, hlit+hdist)
	for position := 0; position < len(lengths); {
		symbol, ok := reader.symbol(&codeTable)
		if !ok {
			return fmt.Errorf("invalid DEFLATE code-length symbol")
		}
		switch {
		case symbol <= 15:
			lengths[position] = byte(symbol)
			position++
		case symbol == 16:
			if position == 0 {
				return fmt.Errorf("DEFLATE repeat has no previous length")
			}
			extra, ok := reader.read(2)
			if !ok {
				return fmt.Errorf("truncated DEFLATE repeat")
			}
			count := int(extra) + 3
			if count > len(lengths)-position {
				return fmt.Errorf("DEFLATE repeat exceeds tree")
			}
			value := lengths[position-1]
			for range count {
				lengths[position] = value
				position++
			}
		case symbol == 17:
			extra, ok := reader.read(3)
			if !ok {
				return fmt.Errorf("truncated DEFLATE zero repeat")
			}
			count := int(extra) + 3
			if count > len(lengths)-position {
				return fmt.Errorf("DEFLATE zero repeat exceeds tree")
			}
			position += count
		case symbol == 18:
			extra, ok := reader.read(7)
			if !ok {
				return fmt.Errorf("truncated DEFLATE long zero repeat")
			}
			count := int(extra) + 11
			if count > len(lengths)-position {
				return fmt.Errorf("DEFLATE long zero repeat exceeds tree")
			}
			position += count
		default:
			return fmt.Errorf("invalid DEFLATE code-length symbol %d", symbol)
		}
	}
	if lengths[256] == 0 {
		return fmt.Errorf("DEFLATE literal tree lacks end marker")
	}
	if err := literal.build(lengths[:hlit]); err != nil {
		return err
	}
	return distance.build(lengths[hlit:])
}

var unsafeFlateLengthBase = [...]uint16{3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258}
var unsafeFlateLengthExtra = [...]byte{0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0}
var unsafeFlateDistanceBase = [...]uint16{1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577}
var unsafeFlateDistanceExtra = [...]byte{0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13}

// unsafeFlateCopy expands one validated LZ77 reference directly in output.
// Doubling an already produced prefix preserves overlap semantics while using
// Go's optimized memmove instead of copying one byte at a time.
func unsafeFlateCopy(output []byte, position, distance, length int) {
	start := position - distance
	first := min(distance, length)
	copy(output[position:position+first], output[start:start+first])
	copied := first
	for copied < length {
		count := min(copied, length-copied)
		copy(output[position+copied:position+copied+count], output[position:position+count])
		copied += count
	}
}

// inflateUnsafe decodes one complete raw DEFLATE stream into an exact-size
// destination. Unsafe accesses happen only after source and destination bounds
// are established; every malformed length, distance and tree returns an error.
func inflateUnsafe(source, output []byte) error {
	if len(source) == 0 {
		return fmt.Errorf("empty DEFLATE stream")
	}
	reader := unsafeFlateBits{src: source, base: unsafe.Pointer(unsafe.SliceData(source))}
	outputBase := unsafe.Pointer(unsafe.SliceData(output))
	position := 0
	final := false
	for !final {
		finalBit, ok := reader.read(1)
		if !ok {
			return fmt.Errorf("truncated DEFLATE block header")
		}
		final = finalBit != 0
		blockType, ok := reader.read(2)
		if !ok {
			return fmt.Errorf("truncated DEFLATE block header")
		}

		var literal, distance *unsafeFlateTable
		var dynamicLiteral, dynamicDistance unsafeFlateTable
		switch blockType {
		case 0:
			reader.alignStored()
			if len(source)-reader.position < 4 {
				return fmt.Errorf("truncated DEFLATE stored header")
			}
			length := int(binary.LittleEndian.Uint16(source[reader.position:]))
			complement := binary.LittleEndian.Uint16(source[reader.position+2:])
			reader.position += 4
			if uint16(length)^complement != 0xffff {
				return fmt.Errorf("invalid DEFLATE stored length")
			}
			if length > len(source)-reader.position || length > len(output)-position {
				return fmt.Errorf("DEFLATE stored block exceeds buffer")
			}
			copy(output[position:position+length], source[reader.position:reader.position+length])
			reader.position += length
			position += length
			continue
		case 1:
			literal, distance = &unsafeFlateFixedLiteral, &unsafeFlateFixedDistance
		case 2:
			if err := unsafeFlateDynamic(&reader, &dynamicLiteral, &dynamicDistance); err != nil {
				return err
			}
			literal, distance = &dynamicLiteral, &dynamicDistance
		default:
			return fmt.Errorf("reserved DEFLATE block type")
		}

		for {
			symbol, ok := reader.symbol(literal)
			if !ok {
				return fmt.Errorf("invalid DEFLATE literal symbol")
			}
			switch {
			case symbol < 256:
				if position >= len(output) {
					return fmt.Errorf("DEFLATE output exceeds expected size")
				}
				*(*byte)(unsafe.Add(outputBase, position)) = byte(symbol)
				position++
			case symbol == 256:
				goto blockDone
			case symbol <= 285:
				lengthIndex := symbol - 257
				length := int(unsafeFlateLengthBase[lengthIndex])
				extraBits := uint(unsafeFlateLengthExtra[lengthIndex])
				if extraBits != 0 {
					extra, ok := reader.read(extraBits)
					if !ok {
						return fmt.Errorf("truncated DEFLATE match length")
					}
					length += int(extra)
				}

				distanceSymbol, ok := reader.symbol(distance)
				if !ok || distanceSymbol >= len(unsafeFlateDistanceBase) {
					return fmt.Errorf("invalid DEFLATE distance symbol")
				}
				back := int(unsafeFlateDistanceBase[distanceSymbol])
				distanceBits := uint(unsafeFlateDistanceExtra[distanceSymbol])
				if distanceBits != 0 {
					extra, ok := reader.read(distanceBits)
					if !ok {
						return fmt.Errorf("truncated DEFLATE distance")
					}
					back += int(extra)
				}
				if back > position || length > len(output)-position {
					return fmt.Errorf("DEFLATE match exceeds history or output")
				}
				unsafeFlateCopy(output, position, back, length)
				position += length
			default:
				return fmt.Errorf("reserved DEFLATE literal symbol %d", symbol)
			}
		}
	blockDone:
	}

	if position != len(output) {
		return fmt.Errorf("DEFLATE output size %d, expected %d", position, len(output))
	}
	// A valid stream may leave only padding bits in its final source byte. Any
	// complete unread byte means data followed the final block and hard-fails.
	if reader.position-int(reader.count/8) != len(source) {
		return fmt.Errorf("DEFLATE stream has trailing data")
	}
	return nil
}
