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
image. It attaches to a NATS server as a queue-group subscriber and outlives its
clients, so a second client's first request is as warm as the first client's
hundredth.

## Protocol

Request/reply over NATS. The daemon takes the server URL and a **subject
prefix** on the command line and answers on two subjects derived from it.

```
<prefix>.ocr.read      queue group "ocrd"
<prefix>.ocr.version   identity
```

### Why NATS and not a loopback port

The previous protocol bound `127.0.0.1:40066` and took a **filesystem path** per
request. Both assumptions break the moment the daemon and its caller are not the
same machine — which on the reference workstation they are not: a Linux ocrd in
a host-networked WSL2 container answers a Windows caller on loopback perfectly
well, and then cannot open a single one of the Windows paths it is handed.

A port also cannot identify itself. Anything listening on 40066 looks like a
healthy daemon: a stale build from an earlier run, or a WSL networking
reservation with no visible owner. Both problems are transport problems and both
are gone here:

- The image travels **inside the message**, base64 in the request. There is no
  `path` field and no path fallback.
- The caller invents a **per-run-unique subject prefix**. Getting an answer on
  `<prefix>.ocr.version` proves the daemon that answered is the one this run
  started — which a port number can never prove.

### `<prefix>.ocr.read`

Request payload:

```json
{"image":"<base64 PNG or JPEG>","psm":6,"lang":"eng","dpi":300,"upscale":2,"pixel_budget":1230000}
```

| Field | Default | Meaning |
|---|---|---|
| `image` | required | The image itself, base64, PNG or JPEG |
| `psm` | `3` | Tesseract page-segmentation mode |
| `lang` | `eng` | Tesseract language / traineddata name |
| `dpi` | `300` | Source resolution hint passed to the engine |
| `upscale` | `1` | Maximum integer upscale factor |
| `pixel_budget` | `20000000` | Ceiling on post-upscale pixels |

Absent **or zero-valued** optional fields take the defaults above: a caller
serialising from a struct sends `0`, not an omitted key, and a `psm` of 0 is an
unset field rather than a request for segmentation mode 0.

Reply payload:

```json
{"text":"...recognized text...","error":""}
```

Both fields are always present. `error` non-empty means that read failed — and
**the reply is still sent**, for every request the daemon receives, including
one whose JSON will not parse. A silently absent OCR result is indistinguishable
from a pass at the caller, which is the failure mode this daemon exists to make
impossible. There is no `id`: NATS correlates a reply with its request through
the inbox subject, so a hand-rolled correlation token is redundant machinery.

### `<prefix>.ocr.version`

Empty request. Reply:

```json
{"version":"ocrd-rust 0.2.0","impl":"rust"}
```

This is the identity handshake, and it is the ONLY place the wire format reveals
which implementation is answering.

### Server `max_payload` must be raised

NATS defaults to a **1 MB** maximum payload. A page screenshot exceeds that on
its own, and base64 inflates it a further ~33%, so a default-configured server
rejects real requests **at the publisher** with a max-payload error. Configure
the server accordingly — the reference caller runs it at 32 MB:

```
max_payload: 32MB
```

Note that `max_payload` is a **configuration-file setting**; `nats-server` has
no command-line flag for it. Neither implementation imposes a cap of its own;
both accept whatever the server advertises, and both log it at startup so a
1 MB server is one line away from being diagnosed.

### Concurrency is the subscriber count

`--workers N` spawns N readers, each of which **independently** joins the queue
group on `<prefix>.ocr.read` and then handles one message at a time to
completion. NATS delivers each request to exactly one member of the group, so at
most N reads are ever in flight. That is the whole bound.

There is deliberately no semaphore, no worker-slot channel and no bounded task
pool inside either daemon. Those are a second scheduler layered on the queue
group's own, with two places to get the bound wrong instead of one — and they
park requests inside one process, where the server can no longer redeliver them
to another daemon that is free.

N defaults to the box's available parallelism. Anything the daemon cannot take
yet stays queued at the SERVER, which is the correct place for it.

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
ocrd --nats <url> --subject-prefix <prefix> [--workers <n>]
ocrd --version    # prints and exits before connecting or loading a model
```

`--nats` and `--subject-prefix` are **required, with no defaults**. A default URL
invites the daemon to attach to whatever server happens to be listening, and a
default prefix throws away the identity guarantee the per-run-unique prefix
exists to provide; both would fail as a silently wrong answer instead of a loud
refusal. `--workers` defaults to the box's available parallelism.

If the NATS connection cannot be established the daemon prints the reason to
stderr and **exits non-zero**. There is no fallback transport and no degraded
mode: the one thing worse than not starting is appearing to have started.

Anything else is a usage error. `ocrd-go` additionally accepts `--tessdata` and
`--lang`, which it needs because its cgo binding requires an explicit datapath
on Windows, where vcpkg installs no language data and sets no environment;
`ocrd-rust` resolves that through `TESSDATA_PREFIX` and its own compiled-in
default.

## The two implementations

Both join the same queue group and answer the same protocol. Either can be
dropped in for the other; the only thing on the wire that reveals which one is
running is the `impl` field of the `.ocr.version` reply, which exists precisely
so an operator can find out on purpose.

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

See [`BUILD.md`](BUILD.md) for the exact static Windows build contract,
language-data staging rules, and publish payload.

```sh
make windows-rust   # native      -> bin/rust/ocrd-windows-amd64.exe
make windows-rust-static
                    # native      -> bin/rust/ocrd-windows-amd64-static.exe
make rust-tessdata  # native      -> bin/rust/eng.traineddata
make linux-rust     # in a container -> bin/rust/ocrd-linux-amd64
make linux-rust-static
                    # in a container -> bin/rust/ocrd-linux-amd64-static
make windows        # native      -> bin/go/ocrd-windows-amd64.exe
make linux          # in a container -> bin/go/ocrd-linux-amd64
make all            # all four, locally
make publish        # release static Windows ocrd, Linux ocrd, and eng.traineddata
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

Every binary answers `--version`, which prints and exits before connecting to
anything or loading a model — so an image build can prove the binary it just
downloaded actually executes on that userland, with no tessdata present and no
NATS server running. It is a LOADER check only: the identity of a *running*
daemon comes from the `.ocr.version` subject, which proves which process
answered rather than merely that some binary on disk runs.

Both implementations link against a system Tesseract and Leptonica, so those
libraries and their headers must be present:

- **Linux (Alpine):** `apk add gcc musl-dev pkgconf tesseract-ocr-dev leptonica-dev`
- **Windows, dynamic:** vcpkg with `tesseract:x64-windows` installed,
  `VCPKG_ROOT` set, and the vcpkg `installed/x64-windows/bin` directory on
  `PATH` so the DLLs resolve at run time. The Rust build additionally wants
  `VCPKGRS_DYNAMIC=1`; the Go build wants a mingw-w64 `gcc` for cgo.
- **Windows, static:** run `make windows-rust-static`, or run
  `scripts/build-windows-rust-static.ps1` directly. The script installs
  `tesseract:x64-windows-static` if missing, sets `VCPKGRS_TRIPLET` to
  `x64-windows-static`, adds Rust's `+crt-static` target feature, and writes
  `bin/rust/ocrd-windows-amd64-static.exe`.
- **Language data:** set `TESSDATA_PREFIX` to the directory holding
  `eng.traineddata` (or whichever `lang` you request). `make rust-tessdata`
  stages the English model from an explicit `-SourcePath`, from
  `TESSDATA_PREFIX`, or from a standard Windows Tesseract install path, and
  `make publish` uploads that staged `eng.traineddata` beside the static
  Windows executable.

The one-time Windows Tesseract toolchain bootstrap through vcpkg took **~52
minutes** here. That cost is the toolchain's, not either language's — both
implementations pay it identically.

## Client notes

- Both implementations accept `--nats`, `--subject-prefix`, `--workers` and
  `--version` in exactly that double-dash spelling and nothing else,
  deliberately: a client cannot tell which implementation it is starting, so the
  argv has to be identical across both. A second accepted spelling is how two
  CLIs drift apart.
- **Generate a fresh subject prefix per run**, and do not reuse one. It is the
  only thing that distinguishes the daemon you just started from a stale one
  still attached to the same server — the failure a fixed port could never
  detect.
- **Wait for `<prefix>.ocr.version` to answer before sending work.** That
  round trip is the readiness check and the identity proof in one; there is
  nothing else to poll.
- Raise the server's `max_payload` (see above) before sending page images. A
  1 MB default rejects them at the publisher, not at the daemon.
- A client that spawns its own daemon should treat the spawn as
  fire-and-forget: the daemon is *meant* to outlive the client, so start it
  detached and never wait on it. If it cannot reach the server it exits
  non-zero, and the version request simply never answers.
- **`.ocr.version` answering does NOT mean the language model is loadable.**
  Engines are built lazily, on a worker's first read, so a daemon started with
  no usable `TESSDATA_PREFIX` connects, logs normally and answers the version
  request — then fails every read with
  `error: "init tesseract (eng): TessInitError{-1}"`. The failure is per-reply
  and the daemon stays up. If your readiness gate must also prove the model
  loads, send one real `.ocr.read` (any tiny image) and require `error` empty;
  the version request proves identity and reachability only.
- Send the daemon's own stderr to a file rather than a pipe. A pipe to a dead
  parent turns every subsequent diagnostic write inside the daemon into an
  error. Tesseract's own model-load diagnostics ("Error opening data file
  ./eng.traineddata") appear only there, never in a reply.
