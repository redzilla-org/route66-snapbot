# route66-snapbot

One Lambda-based component for rendered-page capture, immutable S3 publication,
Ed25519 evidence attestation, and OCR. This repository preserves the history of
both predecessors: the repository itself was `ocr-daemon`, while `attestor/` was
history-filtered from `route66/command-center/evidence-attestor` and merged.

## Invocation contract

The deployed function is `CommandCenterSnapbot-app`. Attestation actions sign
the `r66-evidence-attestation-v4` canonicalization, with the versioned sidecar
layout, `?versionId=` locators, signing key, and mandatory GitHub publication
unchanged. v4 dropped the derived `ci.sha-match` and `ci.true-green` fields
(GH #3840, owner 2026-09-13: "snapbot does not judge, it only snaps."); the CI
block records raw state and the reader decides greenness. Existing v3
statements continue to verify against the committed public keys in route66.

OCR uses the same Lambda Invoke API:

```json
{
  "action": "ocr-image",
  "image": "<base64 PNG or JPEG>",
  "psm": 3,
  "lang": "eng",
  "dpi": 300,
  "upscale": 1,
  "pixel_budget": 20000000
}
```

Instead of `image`, callers may supply
`{"s3":{"bucket":"...","key":"...","version_id":"..."}}`. The reply is
`{"text":"...","error":"","metadata":{...}}`; an engine or input failure is
never represented as an empty successful read.

## Build and test

```sh
docker build --platform linux/amd64 -f Dockerfile.test -t route66-snapbot:test .
docker build --platform linux/amd64 -t route66-snapbot:local .
```

`Dockerfile.test` compiles and tests the resident Rust OCR worker, installs the
pinned Node closure, and syntax-checks the combined handler. The production
image repeats executable and syntax probes in the final Lambda userland.

## Deployment

`python attestor/deploy.py preflight` validates the command-center identity,
templates, signing key, and route66-derived dev-CI targets. `package` builds and
pushes an immutable Git-SHA-tagged amd64 image; `deploy` additionally applies the
CloudFormation stack. Local route66 verification attaches this same image to
kumo's Lambda Runtime API; nothing is installed or downloaded onto the WSL host.
