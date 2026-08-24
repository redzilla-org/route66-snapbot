# Nim implementation of the accepted RGB KLUT and factor-2 scaler.
# Rust owns every allocation, so this exported procedure uses no Nim GC/runtime data.

{.push checks: off, overflowChecks: off, stackTrace: off, lineTrace: off.}

type
  Bytes = ptr UncheckedArray[uint8]
  Words = ptr UncheckedArray[uint16]

proc luma(source: Bytes, offset: int): uint32 {.inline.} =
  (299'u32 * uint32(source[offset])) +
    (587'u32 * uint32(source[offset + 1])) +
    (114'u32 * uint32(source[offset + 2]))

proc writeRow[C0, C1: static uint32](destination: Bytes, row0, row1: Words,
                                     count: int) {.inline.} =
  # Static coefficients produce separate branch-free loops for edge and interior rows.
  var x = 0
  while x < count:
    destination[x] = uint8((C0 * uint32(row0[x]) + C1 * uint32(row1[x]) + 8) shr 4)
    inc x

proc r66_transform_nim*(source: Bytes, sourceLength: csize_t,
                        width, height: csize_t, channels, scale: uint32,
                        destination: Bytes, destinationLength: csize_t,
                        gray: Bytes, grayLength: csize_t,
                        middle: Words, middleLength: csize_t,
                        lut: Bytes, lutLength: csize_t): cint {.exportc, cdecl.} =
  if width == 0 or height == 0 or channels != 3 or (scale != 1 and scale != 2):
    return 1
  let pixels = int(width * height)
  let outputPixels = pixels * int(scale * scale)
  if sourceLength != csize_t(pixels * 3) or destinationLength != csize_t(outputPixels) or
      grayLength < csize_t(pixels) or lutLength < 255001:
    return 1
  let outputWidth = int(width) * int(scale)
  if scale == 2 and middleLength < csize_t(outputWidth * int(height)):
    return 1

  # RGB is fixed at the ABI boundary, so the min/max reduction has no channel
  # dispatch or bounds checks in the generated C loop.
  var lo = high(uint32)
  var hi = 0'u32
  var i = 0
  while i < pixels:
    let value = luma(source, i * 3)
    lo = min(lo, value)
    hi = max(hi, value)
    inc i

  let valueRange = hi - lo
  let span = max(uint64(valueRange), 1'u64)
  i = 0
  while uint32(i) <= valueRange:
    let value = (2'u64 * uint64(i) * 255'u64 + span) div (2'u64 * span)
    lut[i] = uint8(min(value, 255'u64))
    inc i
  i = 0
  while i < pixels:
    gray[i] = lut[int(luma(source, i * 3) - lo)]
    inc i

  if scale == 1:
    copyMem(destination, gray, pixels)
    return 0

  # The 2x path stores four times the horizontal interpolation in u16 and applies
  # the only rounding after the vertical interpolation, matching Rust and C++.
  let inputWidth = int(width)
  var y = 0
  while y < int(height):
    let inputBase = y * inputWidth
    let middleBase = y * outputWidth
    middle[middleBase] = uint16(gray[inputBase]) * 4
    middle[middleBase + outputWidth - 1] = uint16(gray[inputBase + inputWidth - 1]) * 4
    var x = 0
    while x + 1 < inputWidth:
      let a = uint16(gray[inputBase + x])
      let b = uint16(gray[inputBase + x + 1])
      middle[middleBase + 2 * x + 1] = 3 * a + b
      middle[middleBase + 2 * x + 2] = a + 3 * b
      inc x
    inc y

  writeRow[2'u32, 2'u32](destination, middle, middle, outputWidth)
  let lastBase = (int(height) - 1) * outputWidth
  writeRow[2'u32, 2'u32](cast[Bytes](addr destination[(int(height) * 2 - 1) * outputWidth]),
                         cast[Words](addr middle[lastBase]), cast[Words](addr middle[lastBase]),
                         outputWidth)
  y = 0
  while y + 1 < int(height):
    let row0 = y * outputWidth
    let row1 = row0 + outputWidth
    writeRow[3'u32, 1'u32](cast[Bytes](addr destination[(2 * y + 1) * outputWidth]),
                           cast[Words](addr middle[row0]), cast[Words](addr middle[row1]), outputWidth)
    writeRow[1'u32, 3'u32](cast[Bytes](addr destination[(2 * y + 2) * outputWidth]),
                           cast[Words](addr middle[row0]), cast[Words](addr middle[row1]), outputWidth)
    inc y
  return 0

{.pop.}
