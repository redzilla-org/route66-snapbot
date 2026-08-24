# ocrd

A shared OCR daemon. Resident Tesseract engines behind NATS request/reply:

```
ocrd --nats <url> --subject-prefix <prefix> [--workers <n>]
```

The daemon **outlives its clients** and runs until the machine or an operator
stops it, so its warm engine pool is shared by every OCR consumer that can reach
the same server — across concurrent client processes and across runs.

It answers on `<prefix>.ocr.read` (queue group `ocrd`) and
`<prefix>.ocr.version`. The prefix is **per run and unique**, which is what
replaced the old loopback port: a port could be held by a stale daemon, or by a
reservation with no visible owner (WSL mirrored networking held 46620-46622 on
the reference Windows box), and nothing about a successful connect could tell
those apart from the daemon you meant to start. An answer on a subject only this
run knows can only have come from this run's daemon. If the server cannot be
reached the daemon exits non-zero — there is no fallback transport and no
degraded mode.

It is **generic**: it knows nothing about whatever is depicted in the images it
reads. Language, page segmentation mode, DPI, upscale factor and pixel budget
are all per-request parameters, so the caller owns every domain-specific choice.

Compared with shelling out to the `tesseract` CLI per image, it loads the
language model once per worker instead of once per invocation — the CLI's
spawn-plus-model-load overhead is comparable to the cost of reading a typical
page — and it decodes and preprocesses in-process instead of round-tripping a
multi-megabyte preprocessed temp image through the filesystem.

## Protocol

Request/reply. `<prefix>.ocr.read` carries the image INLINE, base64 — there is
no `path` field, because a path only works when the daemon and its caller share
a filesystem, which is exactly the assumption this transport removes.

```json
{"image":"<base64 PNG or JPEG>","psm":6,"lang":"eng","dpi":300,"upscale":2,"pixel_budget":1230000}
```

```json
{"text":"...","error":""}
```

Only `image` is required; everything else defaults, and a zero value counts as
absent (a caller serialising from a struct sends `0`, not an omitted key).

| field | default | meaning |
|---|---|---|
| `psm` | 3 | Tesseract page segmentation mode |
| `lang` | `eng` | model to load |
| `dpi` | 300 | resolution hint — screenshots carry no meaningful DPI |
| `upscale` | 1 | max integer upscale; helps small anti-aliased text |
| `pixel_budget` | 20000000 | ceiling on upscaled size, in pixels |

`<prefix>.ocr.version` takes an empty request and answers
`{"version":"ocrd-rust 0.2.0","impl":"rust"}`. That round trip is the readiness
check and the identity proof in one.

**The server's `max_payload` must be raised**: NATS defaults to 1 MB, a page
screenshot exceeds that on its own, and base64 adds ~33%. The reference caller
configures 32 MB. This client sets no cap of its own and logs the server's limit
at startup.

**Concurrency is the subscriber count.** `--workers N` spawns N tasks, each of
which independently joins the queue group and handles one message at a time to
completion; NATS gives each request to exactly one member, so at most N reads are
in flight. There is no semaphore and no bounded pool — a second scheduler on top
of the queue group's own would only add a second place to get the bound wrong,
and it would park requests inside this process where the server can no longer
redeliver them. Each worker holds its own lazily-created Tesseract engine
(rebuilt only if `lang` changes), so the model-load cost is paid once per worker,
not per request. Work the daemon cannot take yet stays queued at the SERVER.

Failures are reported as replies, never as service faults: one unreadable image
must not take down a service other pages are queued behind, and a request that
goes unanswered looks exactly like a pass at the caller. Every received message
gets a reply, including one whose JSON will not parse.

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
