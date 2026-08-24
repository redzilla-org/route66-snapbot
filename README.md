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

**Replies arrive out of order.** Every request is served on its own worker and
they finish at different speeds, so a client pipelines requests onto one
connection and demultiplexes replies by `id`. One TCP stream is enough for any
number of concurrent reads.

`error` non-empty means that request failed; the daemon stays up.

### Concurrency belongs to the client

The daemon runs a shared-queue worker pool that grows while every worker is busy
and imposes no ceiling of its own. The number of images read concurrently is
therefore exactly the number of requests its clients keep in flight. That is
deliberate: a daemon shared by several client processes cannot see the machine's
total load, so the only place a sane budget can live is in the clients. Do not
fire unbounded requests at it and expect it to throttle for you.

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

## The two implementations

Both bind the same port and answer the same protocol. Either can be dropped in
for the other; nothing in the protocol reveals which one is running.

| | `ocrd-rust/` | `ocrd-go/` |
|---|---|---|
| Tesseract binding | `tesseract-sys`-style direct linkage | cgo against `tesseract/capi.h` |
| PNG decode | `png` + `fdeflate` | hand-written decoder with amd64 assembly |
| Release profile | fat LTO, 1 codegen unit, `panic=abort` | default |
| Binary | ~403 KB, dynamically linked | ~5.5 MB, dynamically linked |
| Cold build | ~40 s | ~41 s |
| Incremental build | ~10-14 s | ~2 s |

Neither is obviously the winner, which is why both are here. See
[`BUILD_TIMES.md`](BUILD_TIMES.md) for the measured build-cost comparison and
[`BENCHMARK.md`](BENCHMARK.md) for the image-pipeline benchmark across several
languages that informed the decode/resample choices.

The Go implementation carries one structural quirk worth knowing before you edit
it: **a cgo package may not contain Go assembly files.** The PNG pipeline's
amd64 assembly therefore lives in a separate `pipeline/` package with an
exported seam, and `tess.go` is the only file in the tree that touches cgo.

## Building

The `Makefile` builds only the Go implementation, for two platforms:

```sh
make windows   # native build -> bin/ocrd-go-windows-amd64.exe
make linux     # built inside a pinned golang:alpine -> bin/ocrd-go-linux-amd64
make all       # both
make publish   # attach both to a GitHub release (VERSION=..., REPO=...)
```

`make linux` deliberately builds in a container and copies the artifact back out
rather than bind-mounting, so it works against a remote Docker daemon where the
source tree is not visible to the server.

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

- Set the address with the daemon's `-addr` / `--listen` flag. A client that
  spawns its own daemon should treat the spawn as fire-and-forget: the daemon is
  *meant* to outlive the client, so start it detached, never wait on it, and let
  the port bind resolve the race.
- Send the daemon's own stderr to a file rather than a pipe. A pipe to a dead
  parent turns every subsequent diagnostic write inside the daemon into an
  error.
- Recognized page text routinely exceeds the 64 KB line limit that many default
  line readers impose. Raise it, or you will silently truncate results.
