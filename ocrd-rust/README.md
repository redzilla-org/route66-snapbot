# ocrd

A shared, box-wide OCR daemon. Resident Tesseract engines behind a
newline-delimited JSON protocol on loopback TCP (default `127.0.0.1:40066`,
`--listen` to override).

The daemon **outlives its clients**: it binds one fixed loopback port and runs
until the machine or an operator stops it, so its warm engine pool is shared
by every OCR consumer on the box — across concurrent client processes and
across runs. The port bind is the singleton lock: on AddrInUse a second
instance PROBES the port and exits 0 only if something answers — a real
singleton — which makes client-side ensure-running race-free: any client that
finds the port closed may spawn a daemon, exactly one wins the bind, the
losers exit quietly, and everyone connects to the winner. If the port is
reserved but nothing answers (seen on Windows: WSL mirrored networking held
46620-46622 with no visible owner), the daemon exits 1 with a poisoned-port
diagnosis instead of masquerading as a healthy singleton. No pid file, no lock
file, no shutdown handshake.

It is **generic**: it knows nothing about whatever is depicted in the images it
reads. Language, page segmentation mode, DPI, upscale factor and pixel budget
are all per-request parameters, so the caller owns every domain-specific choice.

Compared with shelling out to the `tesseract` CLI per image, it loads the
language model once per worker instead of once per invocation — the CLI's
spawn-plus-model-load overhead is comparable to the cost of reading a typical
page — and it decodes and preprocesses in-process instead of round-tripping a
multi-megabyte preprocessed temp image through the filesystem.

## Protocol

One JSON request per line in, one JSON response per line out, over the TCP
connection. Replies are routed to the connection that sent the request —
concurrent clients never see each other's traffic.

```json
{"id":"a1","path":"/tmp/page.png","psm":3,"lang":"eng","dpi":300,"upscale":3,"pixel_budget":20000000}
```

```json
{"id":"a1","text":"..."}
{"id":"a2","error":"decode /tmp/broken.png: ..."}
```

Only `id` and `path` are required; everything else defaults.

| field | default | meaning |
|---|---|---|
| `psm` | 3 | Tesseract page segmentation mode |
| `lang` | `eng` | model to load |
| `dpi` | 300 | resolution hint — screenshots carry no meaningful DPI |
| `upscale` | 1 | max integer upscale; helps small anti-aliased text |
| `pixel_budget` | 20000000 | ceiling on upscaled size, in pixels |

**Standard thread pool, sized by demand**: requests go onto one shared queue; a
new worker thread is spawned only when every existing worker is busy, and
workers are never torn down. Replies come back **out of order** — each carries
the request's `id`, which is how the caller matches them up. Each worker holds
its own lazily-created Tesseract engine (rebuilt only if `lang` changes), so the
live engine count settles at the caller's peak concurrency and the model-load
cost is paid once per worker, not per request.

The pool imposes NO admission control of its own — no maximum thread count, no
knob for how much of the machine it may take. That is deliberate: a caller
driving OCR at scale already owns a CPU budget for the box, and a service with
its own ceiling would be a second, uncoordinated claim on the same cores.
**Concurrency is bounded entirely by how many requests the caller keeps in
flight**, which leaves the caller's scheduler the only one in play.

Failures are reported as responses, never as service faults: one unreadable
image must not take down a service other pages are queued behind. A malformed
request line is logged and skipped for the same reason.

A client disconnect ends only that client's connection: its in-flight reads
complete and their replies fall on the closed socket, harmlessly. The daemon
itself never exits on client activity — its warm engines are the asset, and
tearing them down with every client would forfeit it.

## Image pipeline

`src/kstream.rs` — PNG bytes → decode (`png` 0.18.1 + fdeflate, configured
for trusted input: checksums and text/iCCP chunks skipped) → fused 8.8
fixed-point luma pass → global contrast stretch → integer bilinear upscale —
is ported from the overall WINNER of `img-scaling-benchmark`: "Pure Rust
KSTREAM, png 0.18.1 + fdeflate, WITHOUT PGO" (28.98/54.77/34.14 ms on the
benchmark's fixtures; PGO measured as a wash, so the build stays non-PGO).
An earlier copy of this module used the benchmark's older KLUT variant and
wrongly credited it as fastest; KSTREAM keeps the luma plane from the min/max
pass (one RGB traversal, not two) and streams the factor-2 upscale through two
rows of storage instead of a full-image intermediate.

The port deviates from the benchmark in one respect, because a benchmark may
abort where a service may not: `decode`, `channels` and the transform return
`Result` instead of panicking.

## Build and distribution

The crate lives at `cloud-compose/docker/worker/ocrd/` and is built as the
**first stage of the CI worker image** (`cloud-compose/docker/worker/Dockerfile`,
stage `ocrd-build`). The final stage copies the release binary to
`/usr/local/bin/ocrd`, so every process that runs in the worker image — CI, and
`local-verify`'s container-routed gates alike — finds the service on `PATH`
with no fetch, no unpacking and no version negotiation.

It is a **separate stage on purpose**: docker's layer cache re-runs the Rust
compile only when a file under `ocrd/` changes, so this crate does not churn
with `entrypoint.sh` or the main stage's package world. `cargo build --release
--locked` uses the checked-in `Cargo.lock`, and crates.io is fetched inside that
stage — the same class of build-time network dependency the rest of that
Dockerfile already takes on apk, npm and `go install`.

There is no artifact publication step: the image IS the distribution. The crate
is outside `go.work` and outside the cloud-compose compile fan-out — nothing but
that one Docker stage builds it.

For **local development** on the crate itself, build it directly; it needs a
Rust toolchain plus Tesseract and Leptonica headers and import libraries:

```sh
# Windows: vcpkg supplies the native dependencies, and the -sys crates look
# for it by default.
vcpkg install tesseract:x64-windows
cargo build --release

# Debian/Ubuntu
apt-get install libtesseract-dev libleptonica-dev
cargo build --release
```

A local `target/` directory is excluded from BOTH the image build context
(`cloud-compose/docker/worker/.dockerignore`) and the worker image's content
hash (`WorkerBuildInputsSHA1`, `cloud-compose/orch/worker_image_freshness.go`),
so building here never perturbs the image's identity.

The release profile (fat LTO, `codegen-units = 1`, `panic = "abort"`) matches
the settings the kernels were measured under.
