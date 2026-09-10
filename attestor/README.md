# Command-Center Evidence Attestor

This directory owns command-center evidence capture and signing for durable
ticket evidence.

`public-key.json` is committed on purpose: it is the verifier trust root.
The wrapped Ed25519 seed is a plain SSM String
(`/r66/evidence-attestor/private-key.v1`); the AES-256-GCM unwrap key is the
`UNWRAP_KEY_B64` constant in `attestor/index.js` (owner 2026-08-26: "the AWS
key should be hard-coded (not Lambda env)"). `deploy.py` reads that constant,
unwraps the SSM value and refuses to deploy unless the seed derives to the
committed public key.

## What the attestor signs (canonicalization v3)

Only what it retrieved itself (owner 2026-08-26: "the attestor should only
sign what it retrieves. It is independent of cloud-compose"):

- the S3 object: bucket, key, VersionId, its own sha256 of the bytes,
  Content-Length, Content-Type, Last-Modified, ETag, attested-at;
- `observed.ci.*` when the caller names `ci: {env, target_sha}`: the LATEST
  `<env>-ci-orchestrator` execution as found -- executed sha, status,
  finalizer mode, worker exit code, `sha-match`, `true-green`, or `ci.error`
  when the account was unreachable. It never refuses (owner 2026-08-26:
  "never refuse, only capture!"); a RUNNING run or a newer sha is a signed
  fact, and the reader decides what it proves;
- `observed.capture.*` for screenshots the Lambda took itself: requested and
  final URL, HTTP status, viewport, cookie fingerprint and count.

Caller-supplied S3 user metadata is informational and unsigned. The object is
never rewritten; the signed statement is written beside it as
`<key>.attestation.json` (the bucket is versioned, so each attestation is a
version of that sidecar). Every URL the attestor returns carries
`?versionId=`.

Invocation shapes:

```json
{"key": "<env>/issue-evidence/issue-1234/...png", "version_id": "<optional>", "ci": {"env": "california-dev", "target_sha": "<40 hex>"}, "github": {"issue": 1234, "token": "<invocation-only credential>"}}
```

```json
{"action": "capture-web-ui-screenshot", "issue": 1234, "env": "california-dev", "target_sha": "<40 hex>",
 "url": "https://...", "cookies": [{"name": "SESSION", "value": "...", "url": "https://..."}], "github": {"issue": 1234, "token": "<invocation-only credential>"}}
```

Cookies are invocation-only; only a redacted fingerprint and count are
recorded. Response: the statement (`object`, `observed`, `manifest`,
`manifest_sha256`, `signature_b64`, `key_id`) plus `url` and
`attestation_url`.

## Direct publication (#3767)

Every action requires `github.issue` and `github.token`. The Lambda signs, wraps
the existing canonical v3 manifest in readable `ROUTE66 SIGNED ATTESTATION` armor, and posts that
exact body to the issue/PR before returning `github_posted=true`, `comment_url`
and identical `evidence_text`. The credential is removed before capture and
never enters artifacts, logs, signatures, configuration or the response. Clients
capture existing GH_TOKEN/GITHUB_TOKEN/gh authentication privately. GitHub issue
comment permissions are required; publication failures fail the invocation.

The clear-signed section is the exact plaintext manifest bytes covered by the
Ed25519 signature. Only that compact signature is Base64; there is no encoded
JSON body or duplicate manifest. Locator headers sit outside the signed section,
whose bucket/key/version/hash let the existing verifier validate those URLs.
Explicit angle autolinks preserve every VersionId character in GitHub's rendered
href. A text fence preserves manifest line breaks without encoding the evidence.
The armor preserves Ed25519 and versioned sidecars; it is not OpenPGP. AWS result
bytes remain available in the linked signed object. After confirmed publication,
run the verifier and inspect those bytes before a separate interpretation comment.
`--text-out` retains the already-posted body and must not cause another comment.

Sequential retries for the same object version/env/target reuse a verified
existing comment and return its original signed observation. Discovery is bounded
to 1,000 issue comments; exceeding it fails rather than risking a duplicate.
New captures get new identities; concurrent submissions are not exactly-once.
Earlier encoded presentations remain historical signed evidence. Cleartext uses
its own retry identity so the new protocol never returns the superseded format.

Verify with `python scripts/cicd/verify_evidence_attestation.py "<url with ?versionId=>"`.

## Configuration

- `DevCiTargetsJson` stack parameter: `{"<env>": {"account", "region"}}` for
  every non-prod env, derived by `deploy.py` from route66's env table
  (`scripts/lib/r66`); `deploy_dev_ci_read_roles.py` derives the same set
  from the same source. No account table is hand-copied anywhere here.
- The Lambda zip is staged in the private command-center DevOps bucket
  `core-devops-bucket-command-center-<acct>-<region>`
  (`devops-bucket-template.yaml`, created by `deploy.py`), never in the
  public evidence bucket.

Deploy order:

1. `python command-center/evidence-attestor/deploy_dev_ci_read_roles.py`
2. `python command-center/evidence-attestor/deploy.py deploy`
