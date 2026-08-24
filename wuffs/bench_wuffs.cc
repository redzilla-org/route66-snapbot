// Wuffs contestant: Wuffs PNG decode followed by the same variant-K transform
// used by the recommended Go and Rust implementations.

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <fstream>
#include <iomanip>
#include <iostream>
#include <limits>
#include <stdexcept>
#include <string>
#include <vector>

// Pin the generated library to PNG and its dependencies. The auxiliary image
// API decodes directly from the input bytes already resident in memory.
#define WUFFS_IMPLEMENTATION
#define WUFFS_CONFIG__STATIC_FUNCTIONS
#define WUFFS_CONFIG__MODULES
#define WUFFS_CONFIG__MODULE__AUX__BASE
#define WUFFS_CONFIG__MODULE__AUX__IMAGE
#define WUFFS_CONFIG__MODULE__ADLER32
#define WUFFS_CONFIG__MODULE__BASE
#define WUFFS_CONFIG__MODULE__CRC32
#define WUFFS_CONFIG__MODULE__DEFLATE
#define WUFFS_CONFIG__MODULE__PNG
#define WUFFS_CONFIG__MODULE__ZLIB
#include "upstream/release/c/wuffs-v0.3.c"

namespace {

using Clock = std::chrono::steady_clock;
constexpr int kWarmups = 3;
constexpr auto kTimedFor = std::chrono::milliseconds(250);
constexpr uint64_t kPixelBudget = 20'000'000;
constexpr uint32_t kFixShift = 16;
constexpr uint32_t kFixOne = 1U << kFixShift;

struct GrayImage {
  size_t width = 0;
  size_t height = 0;
  std::vector<uint8_t> pixels;
};

struct Timing {
  double decode = 0;
  double transform = 0;
  double encode = 0;
  double total = 0;
};

struct Stats {
  double min = 0;
  double med = 0;
  double mean = 0;
};

struct Axis {
  std::vector<int32_t> i0;
  std::vector<int32_t> i1;
  std::vector<uint32_t> weight;
};

double milliseconds(Clock::time_point start, Clock::time_point end) {
  return std::chrono::duration<double, std::milli>(end - start).count();
}

std::vector<uint8_t> read_file(const std::string& path) {
  std::ifstream file(path, std::ios::binary | std::ios::ate);
  if (!file) {
    throw std::runtime_error("open input: " + path);
  }
  const std::streamsize size = file.tellg();
  std::vector<uint8_t> bytes(static_cast<size_t>(size));
  file.seekg(0);
  if (size && !file.read(reinterpret_cast<char*>(bytes.data()), size)) {
    throw std::runtime_error("read input: " + path);
  }
  return bytes;
}

int scale_for(size_t width, size_t height) {
  const uint64_t pixels = static_cast<uint64_t>(width) * height;
  for (int scale = 3; scale >= 2; --scale) {
    if (!pixels || pixels * static_cast<uint64_t>(scale) * scale <= kPixelBudget) {
      return scale;
    }
  }
  return 1;
}

GrayImage gray_stretch(wuffs_base__pixel_buffer& pixbuf) {
  if (pixbuf.pixcfg.pixel_format().repr != WUFFS_BASE__PIXEL_FORMAT__BGRA_PREMUL) {
    throw std::runtime_error("unexpected Wuffs pixel format");
  }
  const size_t width = pixbuf.pixcfg.width();
  const size_t height = pixbuf.pixcfg.height();
  const wuffs_base__table_u8 table = pixbuf.plane(0);
  if (!table.ptr || table.width < (width * 4) || table.stride < (width * 4)) {
    throw std::runtime_error("invalid Wuffs pixel plane");
  }

  // Wuffs' default BGRA_PREMUL output has the same premultiplied channel
  // semantics as Go image.RGBA. -ffp-contract=off keeps the luma expression's
  // rounding identical to GOAMD64=v1.
  const auto luma = [](const uint8_t* pixel) {
    return (0.299 * static_cast<double>(pixel[2])) +
           (0.587 * static_cast<double>(pixel[1])) +
           (0.114 * static_cast<double>(pixel[0]));
  };

  double lo = std::numeric_limits<double>::max();
  double hi = -std::numeric_limits<double>::max();
  for (size_t y = 0; y < height; ++y) {
    const uint8_t* row = table.ptr + (y * table.stride);
    for (size_t x = 0; x < width; ++x) {
      const double value = luma(row + (x * 4));
      lo = std::min(lo, value);
      hi = std::max(hi, value);
    }
  }

  double span = hi - lo;
  if (span < 1e-6) {
    span = 1.0;
  }
  GrayImage output{width, height, std::vector<uint8_t>(width * height)};
  for (size_t y = 0; y < height; ++y) {
    const uint8_t* row = table.ptr + (y * table.stride);
    uint8_t* out = output.pixels.data() + (y * width);
    for (size_t x = 0; x < width; ++x) {
      double value = (luma(row + (x * 4)) - lo) / span * 255.0;
      value = std::clamp(value, 0.0, 255.0);
      out[x] = static_cast<uint8_t>(value + 0.5);
    }
  }
  return output;
}

GrayImage gray_stretch_lut(wuffs_base__pixel_buffer& pixbuf) {
  if (pixbuf.pixcfg.pixel_format().repr != WUFFS_BASE__PIXEL_FORMAT__BGRA_PREMUL) {
    throw std::runtime_error("unexpected Wuffs pixel format");
  }
  const size_t width = pixbuf.pixcfg.width();
  const size_t height = pixbuf.pixcfg.height();
  const wuffs_base__table_u8 table = pixbuf.plane(0);
  if (!table.ptr || table.width < (width * 4) || table.stride < (width * 4)) {
    throw std::runtime_error("invalid Wuffs pixel plane");
  }

  // Wuffs already supplies premultiplied BGRA, so the accepted integer-luma key
  // is three multiplies and two adds with no alpha conversion.
  const auto luma = [](const uint8_t* pixel) {
    return (299U * pixel[2]) + (587U * pixel[1]) + (114U * pixel[0]);
  };
  uint32_t lo = std::numeric_limits<uint32_t>::max();
  uint32_t hi = 0;
  for (size_t y = 0; y < height; ++y) {
    const uint8_t* row = table.ptr + (y * table.stride);
    for (size_t x = 0; x < width; ++x) {
      const uint32_t value = luma(row + (x * 4));
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

  GrayImage output{width, height, std::vector<uint8_t>(width * height)};
  for (size_t y = 0; y < height; ++y) {
    const uint8_t* row = table.ptr + (y * table.stride);
    uint8_t* out = output.pixels.data() + (y * width);
    for (size_t x = 0; x < width; ++x) {
      out[x] = lut[luma(row + (x * 4)) - lo];
    }
  }
  return output;
}

Axis make_axis(size_t size, int scale) {
  const size_t output_size = size * static_cast<size_t>(scale);
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

GrayImage upscale_general(const GrayImage& source, int scale) {
  const size_t width = source.width * scale;
  const size_t height = source.height * scale;
  const Axis x_axis = make_axis(source.width, scale);
  const Axis y_axis = make_axis(source.height, scale);
  std::vector<uint32_t> mid(width * source.height);

  for (size_t y = 0; y < source.height; ++y) {
    const uint8_t* row = source.pixels.data() + (y * source.width);
    uint32_t* out = mid.data() + (y * width);
    for (size_t x = 0; x < width; ++x) {
      const uint32_t weight = x_axis.weight[x];
      out[x] = (static_cast<uint32_t>(row[x_axis.i0[x]]) * (kFixOne - weight)) +
               (static_cast<uint32_t>(row[x_axis.i1[x]]) * weight);
    }
  }

  GrayImage output{width, height, std::vector<uint8_t>(width * height)};
  for (size_t y = 0; y < height; ++y) {
    const uint64_t weight = y_axis.weight[y];
    const uint64_t inverse = kFixOne - weight;
    const uint32_t* row0 = mid.data() + (static_cast<size_t>(y_axis.i0[y]) * width);
    const uint32_t* row1 = mid.data() + (static_cast<size_t>(y_axis.i1[y]) * width);
    uint8_t* out = output.pixels.data() + (y * width);
    for (size_t x = 0; x < width; ++x) {
      out[x] = static_cast<uint8_t>(
          ((static_cast<uint64_t>(row0[x]) * inverse) +
           (static_cast<uint64_t>(row1[x]) * weight) +
           (uint64_t{1} << ((2 * kFixShift) - 1))) >>
          (2 * kFixShift));
    }
  }
  return output;
}

GrayImage upscale_2x(const GrayImage& source) {
  const size_t width = source.width * 2;
  const size_t height = source.height * 2;
  std::vector<uint16_t> mid(width * source.height);
  for (size_t y = 0; y < source.height; ++y) {
    const uint8_t* row = source.pixels.data() + (y * source.width);
    uint16_t* out = mid.data() + (y * width);
    out[0] = static_cast<uint16_t>(row[0]) * 4;
    out[width - 1] = static_cast<uint16_t>(row[source.width - 1]) * 4;
    for (size_t x = 0; x + 1 < source.width; ++x) {
      const uint16_t a = row[x];
      const uint16_t b = row[x + 1];
      out[(2 * x) + 1] = static_cast<uint16_t>((3 * a) + b);
      out[(2 * x) + 2] = static_cast<uint16_t>(a + (3 * b));
    }
  }

  GrayImage output{width, height, std::vector<uint8_t>(width * height)};
  const auto write_row = [&](size_t y, const uint16_t* a, const uint16_t* b,
                             uint32_t ca, uint32_t cb) {
    uint8_t* out = output.pixels.data() + (y * width);
    for (size_t x = 0; x < width; ++x) {
      out[x] = static_cast<uint8_t>(((ca * a[x]) + (cb * b[x]) + 8) >> 4);
    }
  };
  const uint16_t* first = mid.data();
  const uint16_t* last = mid.data() + ((source.height - 1) * width);
  write_row(0, first, first, 2, 2);
  write_row(height - 1, last, last, 2, 2);
  for (size_t y = 0; y + 1 < source.height; ++y) {
    const uint16_t* row0 = mid.data() + (y * width);
    const uint16_t* row1 = row0 + width;
    write_row((2 * y) + 1, row0, row1, 3, 1);
    write_row((2 * y) + 2, row0, row1, 1, 3);
  }
  return output;
}

GrayImage transform_k(wuffs_base__pixel_buffer& pixbuf, int scale) {
  GrayImage gray = gray_stretch(pixbuf);
  if (scale <= 1) {
    return gray;
  }
  return scale == 2 ? upscale_2x(gray) : upscale_general(gray, scale);
}

GrayImage transform_klut(wuffs_base__pixel_buffer& pixbuf, int scale) {
  GrayImage gray = gray_stretch_lut(pixbuf);
  if (scale <= 1) {
    return gray;
  }
  return scale == 2 ? upscale_2x(gray) : upscale_general(gray, scale);
}

void write_pgm(const std::string& path, const GrayImage& image) {
  FILE* file = std::fopen(path.c_str(), "wb");
  if (!file) {
    throw std::runtime_error("create output: " + path);
  }
  std::vector<char> buffer(1 << 20);
  std::setvbuf(file, buffer.data(), _IOFBF, buffer.size());
  bool failed = std::fprintf(file, "P5\n%zu %zu\n255\n", image.width, image.height) < 0;
  if (!failed) {
    failed = std::fwrite(image.pixels.data(), 1, image.pixels.size(), file) !=
             image.pixels.size();
  }
  failed = (std::fclose(file) != 0) || failed;
  if (failed) {
    throw std::runtime_error("write output: " + path);
  }
}

Stats summarize(std::vector<double> values) {
  std::sort(values.begin(), values.end());
  const size_t middle = values.size() / 2;
  const double median = values.size() % 2
                            ? values[middle]
                            : (values[middle - 1] + values[middle]) / 2.0;
  double sum = 0;
  for (double value : values) {
    sum += value;
  }
  return Stats{values.front(), median, sum / values.size()};
}

void print_stats(const char* name, const Stats& stats) {
  std::cout << '"' << name << "\":{\"min\":" << stats.min
            << ",\"med\":" << stats.med << ",\"mean\":" << stats.mean << '}';
}

}  // namespace

int main(int argc, char** argv) {
  try {
    if (argc != 4) {
      std::cerr << "usage: wuffsbench K|KLUT|DECODE <input.png> <output.pgm>\n";
      return 2;
    }
    const std::string variant = argv[1];
    const bool decode_only = variant == "DECODE";
    if (!decode_only && variant != "K" && variant != "KLUT") {
      throw std::runtime_error("variant must be K, KLUT or DECODE");
    }
    const std::string input_path = argv[2];
    const std::string output_path = argv[3];
    const std::vector<uint8_t> raw = read_file(input_path);
    std::vector<Timing> timings;
    GrayImage output;
    int scale = 0;
    uint32_t decoded_width = 0;
    uint32_t decoded_height = 0;

	Clock::time_point timed_start;
	for (int iteration = 0;; ++iteration) {
	  const auto t0 = Clock::now();
	  if (iteration == kWarmups) {
		timed_start = t0;
	  }
      wuffs_aux::DecodeImageCallbacks callbacks;
      wuffs_aux::sync_io::MemoryInput input(raw.data(), raw.size());
      auto decoded = wuffs_aux::DecodeImage(callbacks, input);
      if (!decoded.error_message.empty() || !decoded.pixbuf.pixcfg.is_valid()) {
        throw std::runtime_error("Wuffs decode: " + decoded.error_message);
      }
      const auto t1 = Clock::now();
      decoded_width = decoded.pixbuf.pixcfg.width();
      decoded_height = decoded.pixbuf.pixcfg.height();
      scale = scale_for(decoded_width, decoded_height);
      if (!decode_only) {
        output = variant == "KLUT" ? transform_klut(decoded.pixbuf, scale)
                                    : transform_k(decoded.pixbuf, scale);
      }
      const auto t2 = Clock::now();
      if (!decode_only) {
        write_pgm(output_path, output);
      }
      const auto t3 = Clock::now();
	  if (iteration >= kWarmups) {
		timings.push_back(Timing{milliseconds(t0, t1), milliseconds(t1, t2),
							 milliseconds(t2, t3), milliseconds(t0, t3)});
		if ((t3 - timed_start) >= kTimedFor) {
		  break;
		}
	  }
	}

    const auto collect = [&](double Timing::*field) {
      std::vector<double> values;
      for (const Timing& timing : timings) {
        values.push_back(timing.*field);
      }
      return summarize(std::move(values));
    };
    uint64_t output_bytes = 0;
    if (!decode_only) {
      std::ifstream file(output_path, std::ios::binary | std::ios::ate);
      output_bytes = static_cast<uint64_t>(file.tellg());
    }
    std::cout << std::fixed << std::setprecision(6)
              << "{\"variant\":\"" << variant
              << "\",\"implementation\":\"wuffs-0.3.5\",\"input\":\""
              << input_path << "\",\"decodedW\":" << decoded_width
              << ",\"decodedH\":" << decoded_height << ",\"scale\":" << scale
              << ",\"outW\":" << output.width << ",\"outH\":" << output.height
              << ",\"outBytes\":" << output_bytes << ",\"iters\":"
              << timings.size() << ',';
    print_stats("decode", collect(&Timing::decode));
    std::cout << ',';
    print_stats("transform", collect(&Timing::transform));
    std::cout << ',';
    print_stats("encode", collect(&Timing::encode));
    std::cout << ',';
    print_stats("total", collect(&Timing::total));
    std::cout << "}\n";
    return 0;
  } catch (const std::exception& error) {
    std::cerr << "wuffsbench: " << error.what() << '\n';
    return 1;
  }
}
