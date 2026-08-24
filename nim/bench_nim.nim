# Pure-Nim end-to-end benchmark: nimPNG decode, KLUT transform, PGM write.
import std/[algorithm, monotimes, os, strformat, strutils, times]
import nimpng/nimPNG
import transform

const
  Warmups = 3
  TimedNanoseconds = 250_000_000
  PixelBudget = 20_000_000

proc scaleFor(width, height: int): int =
  var scale = 3
  while scale > 1:
    if width * height * scale * scale <= PixelBudget:
      return scale
    dec scale
  1

proc writePgm(path: string, pixels: seq[uint8], width, height: int): int =
  let header = &"P5\n{width} {height}\n255\n"
  var output = open(path, fmWrite)
  defer: output.close()
  output.write(header)
  if pixels.len > 0:
    let written = output.writeBuffer(unsafeAddr pixels[0], pixels.len)
    if written != pixels.len:
      raise newException(IOError, "short PGM write")
  header.len + pixels.len

proc transformKlut(source: string, width, height, scale: int): seq[uint8] =
  # The exported Nim kernel receives Nim-owned buffers here; no foreign runtime
  # participates in this pure pipeline.
  let pixels = width * height
  result = newSeq[uint8](pixels * scale * scale)
  var gray = newSeq[uint8](pixels)
  # Keep one inert element at scale 1 so the C ABI receives a valid non-dereferenced pointer.
  var middle = if scale == 2: newSeq[uint16](width * 2 * height) else: newSeq[uint16](1)
  var lut = newSeq[uint8](255001)
  let status = r66_transform_nim(
    cast[ptr UncheckedArray[uint8]](unsafeAddr source[0]), csize_t(source.len),
    csize_t(width), csize_t(height), 3'u32, uint32(scale),
    cast[ptr UncheckedArray[uint8]](addr result[0]), csize_t(result.len),
    cast[ptr UncheckedArray[uint8]](addr gray[0]), csize_t(gray.len),
    cast[ptr UncheckedArray[uint16]](addr middle[0]), csize_t(middle.len),
    cast[ptr UncheckedArray[uint8]](addr lut[0]), csize_t(lut.len))
  if status != 0:
    raise newException(ValueError, "Nim KLUT transform failed")

proc elapsedMs(started, finished: MonoTime): float =
  float((finished - started).inNanoseconds) / 1_000_000.0

proc median(values: var seq[float]): float =
  values.sort()
  values[values.len div 2]

# Use the fastest observed sample for contender rankings while keeping the
# median in the JSON so run-to-run noise remains visible.
proc minimum(values: openArray[float]): float =
  result = values[0]
  for index in 1 ..< values.len:
    result = min(result, values[index])

proc main() =
  if paramCount() != 3 or paramStr(1) != "KLUT":
    raise newException(ValueError, "usage: nimbench KLUT input.png output.pgm")
  let inputPath = paramStr(2)
  let outputPath = paramStr(3)
  let raw = readFile(inputPath)
  var decodeTimes, transformTimes, encodeTimes, totalTimes: seq[float]
  var width, height, scale, outputBytes: int
  var iteration = 0
  var timedStart: MonoTime

  while true:
    let t0 = getMonoTime()
    if iteration == Warmups:
      timedStart = t0
    let image = decodePNG24(raw)
    if image.isNil:
      raise newException(ValueError, "nimPNG decode failed")
    let t1 = getMonoTime()
    width = image.width
    height = image.height
    scale = scaleFor(width, height)
    let output = transformKlut(image.data, width, height, scale)
    let t2 = getMonoTime()
    outputBytes = writePgm(outputPath, output, width * scale, height * scale)
    let t3 = getMonoTime()
    if iteration >= Warmups:
      decodeTimes.add(elapsedMs(t0, t1))
      transformTimes.add(elapsedMs(t1, t2))
      encodeTimes.add(elapsedMs(t2, t3))
      totalTimes.add(elapsedMs(t0, t3))
      if (t3 - timedStart).inNanoseconds >= TimedNanoseconds:
        break
    inc iteration

  let escapedInput = inputPath.replace("\\", "\\\\")
  let minimumDecode = minimum(decodeTimes)
  let minimumTransform = minimum(transformTimes)
  let minimumEncode = minimum(encodeTimes)
  let minimumTotal = minimum(totalTimes)
  echo &"{{\"variant\":\"KLUT\",\"input\":\"{escapedInput}\",\"scale\":{scale}," &
    &"\"outW\":{width * scale},\"outH\":{height * scale},\"outBytes\":{outputBytes}," &
    &"\"iters\":{totalTimes.len},\"decode\":{{\"min\":{minimumDecode},\"med\":{median(decodeTimes)}}}," &
    &"\"transform\":{{\"min\":{minimumTransform},\"med\":{median(transformTimes)}}}," &
    &"\"encode\":{{\"min\":{minimumEncode},\"med\":{median(encodeTimes)}}}," &
    &"\"total\":{{\"min\":{minimumTotal},\"med\":{median(totalTimes)}}}}}"

main()
