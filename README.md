# ocr-daemon

A small OCR service that keeps Tesseract's language models resident in memory,
and two independent implementations of it — one Rust, one Go — that speak the
same wire protocol.

## Why this exists

The usual way to OCR a batch of images is to run the `tesseract` CLI once per
image. That costs a process spawn plus a full model load before the engine reads
a single pixel — measured here at **413 ms** on a 20x20 image where there is
nothing to read at all. Against a real 1.23 MPix screenshot at 829 ms, the fixed
cost is *half the work*. A job with hundreds of images spends minutes of CPU on
nothing, and that CPU is taken from whatever else is running on the box.

`ocrd` pays the model-load cost once per machine lifetime instead of once per
image. It binds a loopback TCP port and outlives its clients, so a second
client's first request is as warm as the first client's hundredth. The port bind
doubles as the singleton lock: several clients may race to spawn a daemon,
exactly one wins the bind, the losers exit 0 and connect to the winner.

## Protocol

Newline-delimited JSON over TCP, default `127.0.0.1:40066`. Loopback only — the
daemon has no authentication and must never be reachable off-box.

Request, one JSON object per line:

```json
{"id":"1","path":"/abs/path/page.png","psm":3,"lang":"eng","dpi":300,"upscale":1,"pixel_budget":20000000}
```

| Field | Default | Meaning |
|---|---|---|
| `id` | required | Opaque correlation token, echoed back |
| `path` | required | Absolute path to a PNG the daemon can read |
| `psm` | `3` | Tesseract page-segmentation mode |
| `lang` | `eng` | Tesseract language / traineddata name |
| `dpi` | `300` | Source resolution hint passed to the engine |
| `upscale` | `1` | Maximum integer upscale factor |
| `pixel_budget` | `20000000` | Ceiling on post-upscale pixels |

Response, one JSON object per line:

```json
{"id":"1","text":"...recognized text...","error":""}
```

**Match replies to requests by `id`, never by position.** The daemon sends each
reply as soon as that image finishes, and images finish in whatever order they
finish — a small one overtakes a large one queued ahead of it. So the third
reply you read is not necessarily the answer to your third request:

```
you send:      A (huge page)    B (small)    C (small)
you read back: B                C            A
```

Nothing is wrong with the stream when this happens. It is TCP, so the bytes
arrive exactly as the daemon wrote them and nothing is shuffled in transit; the
daemon simply chose to write B's reply first because B was done first. That is
the entire reason `id` exists in the protocol.

If you only ever have one request outstanding, replies come back in the order
you sent them and this never comes up. It only shows up once you pipeline —
which you should, since one connection handles any number of concurrent
requests and a connection per image would waste the daemon's whole point.

`error` non-empty means that request failed; the daemon stays up.

### Concurrency belongs to the client

**The daemon does not throttle for you.** A daemon shared by several client
processes cannot see the machine's total load, so the only place a sane budget
can live is in the clients. Send it a thousand requests at once and it will try
to serve a thousand requests.

The two implementations differ here in a way a client can observe, so do not
depend on either shape: `ocrd-rust` runs a shared-queue pool that grows while
every worker is busy and imposes no ceiling at all, while `ocrd-go` caps
concurrent recognitions at `NumCPU` and parks the excess. Both drain the socket
unconditionally — a request is never left unread because the engines are busy.
That last property is load-bearing rather than incidental: a daemon that stops
reading its socket while a client is still writing to it deadlocks the pair,
since the client cannot get to the replies that would free the daemon's queue.

### The scale-1 passthrough

`upscale` and `pixel_budget` together decide an integer scale factor. When that
factor comes out **1**, no upscaling would happen, and the daemon hands Tesseract
the original file untouched rather than running it through the preprocessor.

This is not an optimization, it is a correctness fix, and it is worth knowing
about if you write a competing client. Converting a very wide image to an 8-bit
grayscale plane and handing that to Tesseract can hit a pathological
binarization path. Measured on one 19599x1002 page: **177 s** through a grayscale
plane versus **3.2 s** for the same page as its original RGB file. Controls on
the same image via the CLI: raw RGB 3.2 s, plain grayscale 39.1 s, contrast-
stretched grayscale 40.2 s — so the gray plane itself is the trigger, not any
particular resampling kernel. Planning the scale from the PNG header alone lets
the daemon skip decoding entirely on that path.

## Command line

Identical across both implementations:

```
ocrd [--listen host:port]     # default 127.0.0.1:40066
ocrd --version                # prints and exits before bind or model load
```

Anything else is a usage error. `ocrd-go` additionally accepts `--tessdata` and
`--lang`, which it needs because its cgo binding requires an explicit datapath
on Windows, where vcpkg installs no language data and sets no environment;
`ocrd-rust` resolves that through `TESSDATA_PREFIX` and its own compiled-in
default.

## The two implementations

Both bind the same port and answer the same protocol. Either can be dropped in
for the other; nothing in the protocol reveals which one is running.

| | `ocrd-rust/` | `ocrd-go/` |
|---|---|---|
| Tesseract binding | `tesseract-sys`-style direct linkage | cgo against `tesseract/capi.h` |
| PNG decode | `png` + `fdeflate` | hand-written decoder with amd64 assembly |
| Release profile | fat LTO, 1 codegen unit, `panic=abort` | default |
| Binary | 425 KB, dynamically linked | 5.5 MB, dynamically linked |
| Cold build | ~40 s | ~41 s |
| Incremental build | ~10-14 s | ~2 s |

**`ocrd-rust` is the shipped implementation.** The honest reasoning, because
the numbers do not all point one way:

- **No cgo.** This is the decisive one. The Go implementation links Tesseract
  through cgo, and a cgo package may not contain Go assembly — so its PNG
  pipeline had to be split into a separate package behind an exported seam
  purely to satisfy the toolchain. `CGO_ENABLED=1` also rules out the pure-Go
  cross-compile that makes a single build host produce every platform.
- **Size:** 425 KB against 5.5 MB, about 13x.
- **Runtime is a near-tie, and it stopped mattering.** The pipeline benchmark
  actually favors the Go decoder slightly (27.59 ms against 28.43 ms on the
  typical case, on the strength of a 0.52 ms transform phase against 7.62 ms).
  But the scale-1 passthrough above means the expensive cases skip the
  preprocessing pipeline entirely, so that margin now governs milliseconds on
  small images rather than anything that shows up in a real workload.
- **Build cost is a wash**, and the one number that looks decisive is not: see
  [`BUILD_TIMES.md`](BUILD_TIMES.md). Cold builds are within ~1.5 s of each
  other. Rust's slower incremental build (~10-14 s against ~2 s) is a
  consequence of `lto = "fat"` and `codegen-units = 1` in the release profile,
  not of the language — iterate with a dev profile.

The Go implementation stays in the tree and is still built by `make all`, but it
is **not published**: releases carry `ocrd-rust` binaries only. That way there
is no artifact to install by mistake and no question about which implementation
a given machine ended up running. It is kept because it is the reason the
pipeline choices are known to be sound rather than assumed, and because a
working second implementation is the cheapest available check that this protocol
is implementable from its description rather than only from the Rust source.
Build it from the tree if you want to run it.

See [`BENCHMARK.md`](BENCHMARK.md) for the image-pipeline benchmark across
several languages that informed the decode/resample choices.

The Go implementation carries one structural quirk worth knowing before you edit
it: **a cgo package may not contain Go assembly files.** The PNG pipeline's
amd64 assembly therefore lives in a separate `pipeline/` package with an
exported seam, and `tess.go` is the only file in the tree that touches cgo.

## Building

The `Makefile` builds both implementations for both platforms. `ocrd-rust` is
the one consumers are expected to install; the Go targets are kept so the second
implementation cannot quietly rot.

```sh
make windows-rust   # native      -> bin/ocrd-rust-windows-amd64.exe
make linux-rust     # in a container -> bin/ocrd-rust-linux-amd64
make windows        # native      -> bin/ocrd-go-windows-amd64.exe
make linux          # in a container -> bin/ocrd-go-linux-amd64
make all            # all four, locally
make publish        # release ONLY the two ocrd-rust binaries (VERSION=..., REPO=...)
```

The two `linux*` targets deliberately build in a container and copy the artifact
back out rather than bind-mounting, so they work against a remote Docker daemon
that cannot see the source tree — and, for the Rust target, so the musl userland
that produces the binary is byte-identical to the one that runs it.

### The Rust build is slow — budget for it

This is the one thing to know before you start editing `ocrd-rust`, because it
is easy to mistake for a hang.

| Build | Measured |
|---|---|
| `make linux-rust` (container, cold) | **2 m 49 s** |
| `ocrd-rust` native release, cold | ~35-40 s |
| `ocrd-rust` native release, incremental | **10-14 s** |
| `ocrd-go` native, incremental | ~2 s |

Three costs stack up, and none of them is the language:

1. **bindgen + libclang.** `leptess` generates its bindings by parsing the
   Tesseract and Leptonica headers through libclang at build time. In the
   container that also means installing `clang-dev`/`llvm-dev` first.
2. **`lto = "fat"` with `codegen-units = 1`.** The release profile links the
   whole program, across the `png` crate boundary, as a single unit. This is
   deliberate — the kernels in `src/kstream.rs` were *measured* under exactly
   these settings — but it means every edit pays a whole-program link.
3. **`panic = "abort"`**, so no unwind landing pads are emitted around the hot
   loops. Cheap, but it is part of the same measured configuration.

**Do not iterate on the release profile.** Build with `cargo build` (dev) or add
a profile that drops `lto` and raises `codegen-units` while you are working, and
keep the release profile for the artifact you actually publish and benchmark.
Changing the release profile invalidates the benchmark this implementation was
chosen on, so change it only with fresh measurements.

For the same reason `make linux-rust` has no incremental mode worth using: the
container starts from a clean `/src` every time, so it is always a cold build
plus a fat-LTO link. Publish from it; do not develop against it.

See [`BUILD_TIMES.md`](BUILD_TIMES.md) for the full cross-language comparison,
including why the ~52-minute one-time Windows Tesseract toolchain bootstrap is a
toolchain cost that both implementations pay identically and neither should be
credited or blamed for.

Every binary answers `--version`, which prints and exits before binding a port
or loading a model — so an image build can prove the binary it just downloaded
actually executes on that userland, with no tessdata present and no port free.

Both implementations link against a system Tesseract and Leptonica, so those
libraries and their headers must be present:

- **Linux (Alpine):** `apk add gcc musl-dev pkgconf tesseract-ocr-dev leptonica-dev`
- **Windows:** vcpkg with `tesseract` installed, `VCPKG_ROOT` set, and the vcpkg
  `installed/x64-windows/bin` directory on `PATH` so the DLLs resolve at run
  time. The Rust build additionally wants `VCPKGRS_DYNAMIC=1`; the Go build wants
  a mingw-w64 `gcc` for cgo.
- **Language data:** set `TESSDATA_PREFIX` to the directory holding
  `eng.traineddata` (or whichever `lang` you request).

The one-time Windows Tesseract toolchain bootstrap through vcpkg took **~52
minutes** here. That cost is the toolchain's, not either language's — both
implementations pay it identically.

## Client notes

- Set the address with `--listen host:port`. Both implementations accept
  `--listen` and `--version` in exactly that double-dash spelling and nothing
  else, deliberately: a client cannot tell which implementation it is starting,
  so the argv has to be identical across both. A second accepted spelling is how
  two CLIs drift apart. A client that
  spawns its own daemon should treat the spawn as fire-and-forget: the daemon is
  *meant* to outlive the client, so start it detached, never wait on it, and let
  the port bind resolve the race.
- Send the daemon's own stderr to a file rather than a pipe. A pipe to a dead
  parent turns every subsequent diagnostic write inside the daemon into an
  error.
- Recognized page text routinely exceeds the 64 KB line limit that many default
  line readers impose. Raise it, or you will silently truncate results.
