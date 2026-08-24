#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <vector>

namespace {

constexpr uint32_t kFixShift = 16;
constexpr uint32_t kFixOne = 1U << kFixShift;

struct Axis {
  std::vector<int32_t> i0;
  std::vector<int32_t> i1;
  std::vector<uint32_t> weight;
};

uint32_t integer_luma(const uint8_t* pixel, uint32_t channels) {
  uint32_t r = 0;
  uint32_t g = 0;
  uint32_t b = 0;
  switch (channels) {
    case 4: {
      const uint32_t alpha = pixel[3];
      r = (pixel[0] * alpha) / 255;
      g = (pixel[1] * alpha) / 255;
      b = (pixel[2] * alpha) / 255;
      break;
    }
    case 3:
      r = pixel[0];
      g = pixel[1];
      b = pixel[2];
      break;
    case 2:
      r = g = b = (pixel[0] * static_cast<uint32_t>(pixel[1])) / 255;
      break;
    case 1:
      r = g = b = pixel[0];
      break;
  }
  return (299U * r) + (587U * g) + (114U * b);
}

std::vector<uint8_t> gray_stretch_lut(const uint8_t* source,
                                      size_t pixels,
                                      uint32_t channels) {
  uint32_t lo = UINT32_MAX;
  uint32_t hi = 0;
  if (channels == 3) {
    // Both benchmark fixtures are RGB. Hoisting format dispatch out of the pixel
    // loops exposes a straight three-channel kernel to Clang's optimizer.
    for (size_t i = 0; i < pixels; ++i) {
      const uint8_t* pixel = source + (i * 3);
      const uint32_t value =
          (299U * pixel[0]) + (587U * pixel[1]) + (114U * pixel[2]);
      lo = std::min(lo, value);
      hi = std::max(hi, value);
    }
  } else {
    for (size_t i = 0; i < pixels; ++i) {
      const uint32_t value = integer_luma(source + (i * channels), channels);
      lo = std::min(lo, value);
      hi = std::max(hi, value);
    }
  }

  const uint32_t range = hi - lo;
  const uint64_t span = std::max<uint64_t>(range, 1);
  std::vector<uint8_t> lut(static_cast<size_t>(range) + 1);
  for (size_t offset = 0; offset < lut.size(); ++offset) {
    const uint64_t value = ((2 * offset * 255) + span) / (2 * span);
    lut[offset] = static_cast<uint8_t>(std::min<uint64_t>(value, 255));
  }

  std::vector<uint8_t> gray(pixels);
  if (channels == 3) {
    for (size_t i = 0; i < pixels; ++i) {
      const uint8_t* pixel = source + (i * 3);
      const uint32_t value =
          (299U * pixel[0]) + (587U * pixel[1]) + (114U * pixel[2]);
      gray[i] = lut[value - lo];
    }
  } else {
    for (size_t i = 0; i < pixels; ++i) {
      const uint32_t value = integer_luma(source + (i * channels), channels);
      gray[i] = lut[value - lo];
    }
  }
  return gray;
}

Axis make_axis(size_t size, uint32_t scale) {
  const size_t output_size = size * scale;
  Axis axis{std::vector<int32_t>(output_size),
            std::vector<int32_t>(output_size),
            std::vector<uint32_t>(output_size)};
  for (size_t out = 0; out < output_size; ++out) {
    const int64_t numerator = (2 * static_cast<int64_t>(out)) + 1 - scale;
    const int64_t denominator = 2 * scale;
    int64_t floor = numerator / denominator;
    int64_t remainder = numerator - (floor * denominator);
    if (remainder < 0) {
      --floor;
      remainder += denominator;
    }
    const auto clamp = [size](int64_t value) {
      return static_cast<int32_t>(
          value < 0 ? 0 : (value >= static_cast<int64_t>(size) ? size - 1 : value));
    };
    axis.i0[out] = clamp(floor);
    axis.i1[out] = clamp(floor + 1);
    axis.weight[out] = static_cast<uint32_t>(
        ((remainder * kFixOne) + (denominator / 2)) / denominator);
  }
  return axis;
}

void upscale_2x(const std::vector<uint8_t>& source,
                size_t width,
                size_t height,
                uint8_t* destination) {
  const size_t output_width = width * 2;
  const size_t output_height = height * 2;
  std::vector<uint16_t> middle(output_width * height);
  for (size_t y = 0; y < height; ++y) {
    const uint8_t* row = source.data() + (y * width);
    uint16_t* output = middle.data() + (y * output_width);
    output[0] = static_cast<uint16_t>(row[0]) * 4;
    output[output_width - 1] = static_cast<uint16_t>(row[width - 1]) * 4;
    for (size_t x = 0; x + 1 < width; ++x) {
      const uint16_t a = row[x];
      const uint16_t b = row[x + 1];
      output[(2 * x) + 1] = static_cast<uint16_t>((3 * a) + b);
      output[(2 * x) + 2] = static_cast<uint16_t>(a + (3 * b));
    }
  }

  const auto write_row = [&](size_t y, const uint16_t* a, const uint16_t* b,
                             uint32_t ca, uint32_t cb) {
    uint8_t* output = destination + (y * output_width);
    for (size_t x = 0; x < output_width; ++x) {
      output[x] = static_cast<uint8_t>(((ca * a[x]) + (cb * b[x]) + 8) >> 4);
    }
  };
  const uint16_t* first = middle.data();
  const uint16_t* last = middle.data() + ((height - 1) * output_width);
  write_row(0, first, first, 2, 2);
  write_row(output_height - 1, last, last, 2, 2);
  for (size_t y = 0; y + 1 < height; ++y) {
    const uint16_t* row0 = middle.data() + (y * output_width);
    const uint16_t* row1 = row0 + output_width;
    write_row((2 * y) + 1, row0, row1, 3, 1);
    write_row((2 * y) + 2, row0, row1, 1, 3);
  }
}

void upscale_general(const std::vector<uint8_t>& source,
                     size_t width,
                     size_t height,
                     uint32_t scale,
                     uint8_t* destination) {
  const size_t output_width = width * scale;
  const size_t output_height = height * scale;
  const Axis x_axis = make_axis(width, scale);
  const Axis y_axis = make_axis(height, scale);
  std::vector<uint32_t> middle(output_width * height);

  for (size_t y = 0; y < height; ++y) {
    const uint8_t* row = source.data() + (y * width);
    uint32_t* output = middle.data() + (y * output_width);
    for (size_t x = 0; x < output_width; ++x) {
      const uint32_t weight = x_axis.weight[x];
      output[x] = (static_cast<uint32_t>(row[x_axis.i0[x]]) * (kFixOne - weight)) +
                  (static_cast<uint32_t>(row[x_axis.i1[x]]) * weight);
    }
  }

  for (size_t y = 0; y < output_height; ++y) {
    const uint64_t weight = y_axis.weight[y];
    const uint64_t inverse = kFixOne - weight;
    const uint32_t* row0 =
        middle.data() + (static_cast<size_t>(y_axis.i0[y]) * output_width);
    const uint32_t* row1 =
        middle.data() + (static_cast<size_t>(y_axis.i1[y]) * output_width);
    uint8_t* output = destination + (y * output_width);
    for (size_t x = 0; x < output_width; ++x) {
      output[x] = static_cast<uint8_t>(
          ((static_cast<uint64_t>(row0[x]) * inverse) +
           (static_cast<uint64_t>(row1[x]) * weight) +
           (uint64_t{1} << ((2 * kFixShift) - 1))) >>
          (2 * kFixShift));
    }
  }
}

}  // namespace

extern "C" int r66_transform_klut(const uint8_t* source,
                                   size_t source_length,
                                   size_t width,
                                   size_t height,
                                   uint32_t channels,
                                   uint32_t scale,
                                   uint8_t* destination,
                                   size_t destination_length) {
  // Keep exceptions inside C++ and return a hard error to Rust across the C ABI.
  try {
    if (!source || !destination || !width || !height || channels < 1 || channels > 4 ||
        scale < 1 || scale > 3 || source_length != (width * height * channels) ||
        destination_length != (width * height * scale * scale)) {
      return 1;
    }
    std::vector<uint8_t> gray = gray_stretch_lut(source, width * height, channels);
    if (scale == 1) {
      std::copy(gray.begin(), gray.end(), destination);
    } else if (scale == 2) {
      upscale_2x(gray, width, height, destination);
    } else {
      upscale_general(gray, width, height, scale, destination);
    }
    return 0;
  } catch (...) {
    return 2;
  }
}
