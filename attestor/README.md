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

## What the attestor signs (canonicalization v4)

Only what it retrieved itself (owner 2026-08-26: "the attestor should only
sign what it retrieves. It is independent of cloud-compose"). Snapbot snaps
and never judges (owner 2026-09-13: "snapbot does not judge, it only snaps.";
"Record only. The next step can check if a non-latest id is green"):

- the S3 object: bucket, key, VersionId, its own sha256 of the bytes,
  Content-Length, Content-Type, Last-Modified, ETag, attested-at;
- `observed.ci.*` when the caller names `ci: {env, target_sha}`: the LATEST
  `<env>-ci-orchestrator` execution as found, raw fields only:

  | field | source |
  | --- | --- |
  | `ci.env` | caller |
  | `ci.target-sha` | caller's claim, recorded as given |
  | `ci.checked-at-utc` | attestor clock at the read |
  | `ci.execution-arn` | ListExecutions, newest |
  | `ci.executed-sha` | DescribeExecution input `sha` |
  | `ci.sfn-status` | DescribeExecution status |
  | `ci.start-date`, `ci.stop-date` | DescribeExecution |
  | `ci.finalizer-mode`, `ci.worker-exit-code` | execution history, finalizer input |
  | `ci.error` | present when a read failed |

  No field compares one value with another: whether `executed-sha` equals
  `target-sha` or the execution is green is the reader's decision, made on
  the execution id it cares about. It never refuses (owner 2026-08-26:
  "never refuse, only capture!"); a RUNNING run or a newer sha is a signed
  fact;
- `observed.capture.*` for screenshots the Lambda took itself: requested and
  final URL, HTTP status, viewport, cookie fingerprint and count.

Version history: v3 statements (signed before GH #3840) also carried the
derived `ci.sha-match` and `ci.true-green`. They keep their v3 line and still
verify; route66's verifiers rebuild each version's manifest exactly.

Caller-supplied S3 user metadata is informational and unsigned. The object is
never rewritten; the signed statement is written beside it as
`<key>.attestation.json` (the bucket is versioned, so each attestation is a
version of that sidecar). Every URL the attestor returns carries
`?versionId=`.

Invocation shapes:

```json
{"key": "<env>/issue-evidence/issue-1234/...png", "version_id": "<optional>", "ci": {"env": "california-dev", "target_sha": "<40 hex>"}, "intent": "what this evidence is meant to prove", "category": "BEFORE", "github": {"issue": 1234, "token": "<invocation-only credential>"}}
```

```json
{"action": "capture-web-ui-screenshot", "issue": 1234, "env": "california-dev", "target_sha": "<40 hex>",
 "url": "https://...", "intent": "what this evidence is meant to prove", "category": "AFTER", "cookies": [{"name": "SESSION", "value": "...", "url": "https://..."}], "github": {"issue": 1234, "token": "<invocation-only credential>"}}
```

Cookies are invocation-only; only a redacted fingerprint and count are
recorded. Response: the statement (`object`, `observed`, `manifest`,
`manifest_sha256`, `signature_b64`, `key_id`) plus `url` and
`attestation_url`.

## Direct publication (#3767)

Every published action requires `github.issue`, `github.token`, `intent`, and a
`category` of `BEFORE` or `AFTER`. The target is derived from the action's existing
canonical input and always names a URL or resource, never an env@sha (owner
2026-09-17): the captured page URL, the AWS resource operation, the observed
`<env>-ci-orchestrator` execution ARN (the state machine ARN when the read
failed), or the versioned `s3://` object; callers never repeat it in a second
field that could disagree. The Lambda signs those values as `observed.claim.*`,
wraps a compact summary in `ROUTE66 SIGNED ATTESTATION` armor, and posts that
exact body to the issue/PR before returning `github_posted=true`, `comment_url`
and identical `evidence_text`. The credential is removed before capture and
never enters artifacts, logs, signatures, configuration or the response. Clients
capture existing GH_TOKEN/GITHUB_TOKEN/gh authentication privately. GitHub issue
comment permissions are required; publication failures fail the invocation.

The ticket comment shows only category, intent, target, attestation time, and the
two versioned locators, plus canonicalization and Key-ID. The Statement locator
contains the complete object/observation manifest and Ed25519 signature; removing
those machine fields from the comment changes presentation, not integrity.
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
