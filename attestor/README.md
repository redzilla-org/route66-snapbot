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
{"key": "<env>/issue-evidence/...png", "version_id": "<optional>", "ci": {"env": "california-dev", "target_sha": "<40 hex>"}}
```

```json
{"action": "capture-web-ui-screenshot", "issue": 1234, "env": "california-dev", "target_sha": "<40 hex>",
 "url": "https://...", "cookies": [{"name": "SESSION", "value": "...", "url": "https://..."}]}
```

Cookies are invocation-only; only a redacted fingerprint and count are
recorded. Response: the statement (`object`, `observed`, `manifest`,
`manifest_sha256`, `signature_b64`, `key_id`) plus `url` and
`attestation_url`.

For `attest-aws-resource` the returned `evidence_text` block carries an
`observed.aws.result:` section holding the attested object body itself -- the
SDK result JSON in the exact bytes that were signed and hashed into
`object.sha256`, capped at 64 KiB with an `observed.aws.result-truncated: true`
line past the cap (owner 2026-09-06, GH #3678). `scripts/cicd/attest_aws_resource.py`
prints that block and `--text-out <path>` writes it for `gh issue comment --body-file`.

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
