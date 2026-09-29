# route66-snapbot

One Lambda-based component for rendered-page capture, immutable S3 publication,
Ed25519 evidence attestation, and OCR. Since GH #4115 the handler is one static
Rust binary (`snapbot/`) that drives Chromium over raw CDP and links Tesseract
in-process (`ocrd-rust/`); there is no Node, npm or Playwright.

## Invocation contract

The deployed function is `CommandCenterSnapbot-app`. Attestation actions sign
the `r66-evidence-attestation-v4` canonicalization, with the versioned sidecar
layout, `?versionId=` locators, signing key, and mandatory GitHub publication
unchanged. v4 dropped the derived `ci.sha-match` and `ci.true-green` fields
(GH #3840, owner 2026-09-13: "snapbot does not judge, it only snaps."); the CI
block records raw state and the reader decides greenness. Existing v3
statements continue to verify against the committed public keys in route66.

## OCR: in-lane, inside a browse screenshot step

No request carries image bytes. A `browse` step `{"op": "screenshot", "name":
..., "full_page": true, "ocr": {...}}` captures with CDP
`Page.captureScreenshot` (png, `optimizeForSpeed`), decodes the PNG once in
memory to a gray plane, and reads it with the lane's resident Tesseract
(tessdata_fast). Snapbot holds no caller domain knowledge: keywords and regions
arrive as JavaScript evaluated in the page at the capture instant.

```json
{
  "passes":   [{"psm": 3, "lang": "eng", "dpi": 300, "upscale": 0.5}],
  "fallback": [{"psm": 3, "upscale": 1}],
  "stop_when": {"keywords_fn": "() => ['Carmel Valley', '$1,099,000']", "min_fraction": 0.8},
  "regions":  "() => [{x: 630, y: 330, width: 650, height: 4800}]"
}
```

- `passes` (required, non-empty) run in order; `fallback` runs only when
  `stop_when` is still unmet after them (it requires `stop_when`).
- `upscale` is the pass scale: (0, 1) is a downscaled read, 1 (default) the
  captured pixels, a whole 2..4 an upscale bounded by `pixel_budget`
  (default 20,000,000 pixels). `psm` defaults to 3, `lang` to eng, `dpi` to 300.
- Reading stops at the first pass whose unioned text contains at least
  `min_fraction` (default 1) of the keywords (case-insensitive,
  whitespace-collapsed substring match).
- `regions` is a rect list or a JS function returning one, in CSS px (image
  px at device scale 1); each rect is cropped before rescaling, so a read costs
  its own area.

The step value carries the stored PNG (`bucket`, `key`, `version_id`, `sha256`,
`width`, `height`), `timings` (`fns_ms`, `capture_ms`, `decode_ms`,
`resample_ms`, `ocr_ms`, `store_ms`) and `ocr`:

```json
{
  "text": "...", "keywords": ["..."], "matched": ["..."],
  "fraction": 0.92, "min_fraction": 0.8, "met": true,
  "won": {"wave": "passes", "index": 0},
  "passes": [{"wave": "passes", "index": 0, "psm": 3, "lang": "eng", "dpi": 300,
              "upscale": 0.5, "pixel_budget": 20000000, "pixels": 1870000,
              "resample_ms": 12, "ocr_ms": 1900, "fraction": 0.92}],
  "regions": [{"x": 630, "y": 330, "width": 650, "height": 4800}],
  "image": {"width": 1366, "height": 5477},
  "engine": "snapbot-ocr/2.0.0 in-process",
  "timings": {"decode_ms": 60, "resample_ms": 12, "ocr_ms": 1900, "total_ms": 1975}
}
```

`won` is null when no `stop_when` was given or it stayed unmet. An OCR engine
failure fails the step.

## Build and test

```sh
docker build -f Dockerfile.test .
```

`Dockerfile.test` builds the production stages, runs the workspace unit tests,
runs `bootstrap probe` (Chromium renders a page, the engine reads it back) in the
Lambda userland, and ends in a `bench` stage that measures per-read OCR cost on
the tall listing page `fixtures/big.png` (scales, region crops, tessdata fast vs
best). `Dockerfile` is the production image.

## Deployment

A push to `main` runs `.github/workflows/image.yml`: the `Dockerfile.test` gate,
then the immutable `ghcr.io/redzilla-org/route66-snapbot:<sha>` image and the
`snapbot-<sha>` release archive local Kumo pulls. `python attestor/deploy.py
preflight|package|deploy` (run from route66 through
`scripts/deployment/deploy_snapbot.py`) builds and pushes the SHA-tagged ECR
image and applies the CloudFormation stack.
