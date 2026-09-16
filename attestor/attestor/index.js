// ============================================================================
// Command-center evidence attestor.
//
// WHY THIS EXISTS: public S3 capability URLs prove only possession of an
// unguessable object key. Ticket lifecycle evidence also needs tamper evidence:
// a verifier must be able to fetch the exact object version, hash the bytes,
// and check a command-center signature without trusting the runner that
// uploaded the file. This Lambda is the single holder of the private signing
// key; runners never receive it.
//
// WHAT IS SIGNED (owner directive 2026-08-26: "the attestor should only sign
// what it retrieves. It is independent of cloud-compose"): the manifest below
// is built EXCLUSIVELY from facts this Lambda observed itself --
//   * the S3 object it fetched: bucket, key, VersionId, its own sha256 of the
//     bytes, Content-Length, Content-Type, Last-Modified, ETag;
//   * the DEV CI state it read from the env's ci-orchestrator Step Function
//     (when the caller names an env + target sha), captured AS FOUND: latest
//     execution's arn, executed sha, status, start/stop dates, finalizer
//     mode/exitCode, plus the caller's target sha recorded as its claim. NO
//     DERIVED FIELD: owner 2026-09-13, verbatim, "snapbot does not judge, it
//     only snaps." The sha_match / true_green booleans this block used to sign
//     were comparisons, not observations, and are gone since canonicalization
//     v4; judging belongs to the NEXT step, the reader (owner 2026-09-13:
//     "Record only. The next step can check if a non-latest id is green").
//     The attestor NEVER refuses on CI state (owner 2026-08-26: "never refuse,
//     only capture!"); a RUNNING execution, a newer sha, a red run or an
//     unreachable account are all captured as fields and signed as observed;
//   * for screenshots it captured itself: requested/final URL, HTTP status,
//     viewport, cookie fingerprint/count, and every PERTURBATION it applied to
//     the page — aborted subresource patterns and clicked selectors — because an
//     induced state is only evidence while the inducement is part of the
//     signature;
//   * for raw HTTP captures it made itself: every redirect hop, the requested
//     and final URL, the allow-listed response headers, and the response BODY,
//     which is stored as the evidence object rather than reduced to a hash;
// evidence-type:code is NOT produced here. Owner order 2026-09-03, verbatim:
// "Move the evidence-type:code implementation to a local process (cloud-compose
// attest)". The former attest-code-at-sha action read a commit's files, patch and
// ancestry-from-main out of the GitHub REST API with a fine-grained PAT at SSM
// /r66/evidence-attestor/github-read-token.v1 -- a parameter that never existed
// and will not be created. Every fact it wanted is already in the operator's local
// checkout, so `cloud-compose attest code` (cloud-compose/subcmd/attest.go) reads
// it from git, signs with the workstation's local-verify Ed25519 key, and writes
// the SAME canonicalization-v3 statement this file wrote at the time. One implementation of
// that evidence type, never two.
// Caller-supplied S3 user metadata is NOT part of the manifest. It stays on
// the object as informational labels and the signature says nothing about it.
// The object is never rewritten; the signed statement is written beside it as
// <key>.attestation.json (this bucket is versioned, so every attestation of
// an object is a version of that sidecar).
//
// KEY MATERIAL (owner directive 2026-08-26: "the AWS key should be hard-coded
// (not Lambda env)"): the AES-256-GCM unwrap key is the constant below, and
// the wrapped Ed25519 seed is a plain SSM String. Both are needed to sign;
// SSM read on that parameter is granted to this function's role alone.
// ============================================================================

"use strict";

const crypto = require("crypto");
const fs = require("fs");
const os = require("os");
const readline = require("readline");
const { spawn } = require("child_process");
// `https` and `URL` serve the capture-http-raw action only. That action needs to
// see EVERY redirect hop, and no high-level fetch client exposes them: both
// puppeteer and fetch() follow redirects internally and hand back only the final
// response, which is by definition not a redirect. A hop-by-hop client is the
// only way to sign "the request to A was 301'd to B which 302'd to C".
const https = require("https");
const { URL } = require("url");
const {
  S3Client,
  HeadObjectCommand,
  GetObjectCommand,
  PutObjectCommand,
} = require("@aws-sdk/client-s3");
const { SSMClient, GetParameterCommand } = require("@aws-sdk/client-ssm");
const { STSClient, AssumeRoleCommand } = require("@aws-sdk/client-sts");
const {
  SFNClient,
  ListExecutionsCommand,
  DescribeExecutionCommand,
  GetExecutionHistoryCommand,
} = require("@aws-sdk/client-sfn");

// Hard-coded by owner directive (see header). Rotating the signing key means
// generating a new wrapped seed + unwrap key pair, replacing this constant,
// bumping public-key.json, and redeploying in one change.
const UNWRAP_KEY_B64 = "o0nZu1JyR60uvbITWZggDAefj+4EGDyzbisahMYXId8=";

// v4 since GH #3840 (owner 2026-09-13, "snapbot does not judge, it only
// snaps."): the manifest's field set lost the derived observed.ci.true-green
// and observed.ci.sha-match lines, and a changed field set is a changed
// canonical manifest, so the version line moves with it. Statements already
// signed under v3 keep their v3 line and still verify: route66's verifiers
// (cloud-compose attestation-check, scripts/cicd/verify_evidence_attestation.py)
// accept both versions and rebuild each one exactly as it was signed.
const CANONICALIZATION = "r66-evidence-attestation-v4";
const STATEMENT_VERSION = 4;

// WHY THIS GRAMMAR IS SECURITY-LOAD-BEARING (GH #3822): the evidence bucket
// accepts writes from every account in the organization, while
// attest-run-manifest intentionally skips GitHub publication. Restrict that
// exception to the one durable web-regression run namespace; a suffix-only
// check would let any organization principal place arbitrary bytes elsewhere
// in the bucket and ask this Lambda to sign them as a run manifest.
const RUN_MANIFEST_KEY_RE = /^(california-prod|california-dev|chicago-prod|chicago-dev|local-test)\/([0-9a-f]{40})\/(\d{8}T\d{6}Z)\/manifest\.json$/;

function requireRunManifestKey(key) {
  const match = RUN_MANIFEST_KEY_RE.exec(key);
  if (!match) {
    throw new Error(
      "attest-run-manifest key must be <env>/<40-lowercase-hex-sha>/<YYYYMMDDTHHMMSSZ>/manifest.json"
    );
  }
  // WHY: the shape alone admits impossible clock values such as month 99,
  // which are not prefixes the run publisher can mint. Round-tripping through
  // Date keeps the signing exemption equal to the publisher's UTC clock format.
  const stamp = match[3];
  const iso = stamp.slice(0, 4) + "-" + stamp.slice(4, 6) + "-" + stamp.slice(6, 8) +
    "T" + stamp.slice(9, 11) + ":" + stamp.slice(11, 13) + ":" + stamp.slice(13, 15) + "Z";
  const parsed = new Date(iso);
  if (Number.isNaN(parsed.getTime()) || parsed.toISOString() !== iso.replace("Z", ".000Z")) {
    throw new Error("attest-run-manifest key contains an invalid UTC run timestamp");
  }
}

function requiredEnv(name) {
  const v = process.env[name];
  if (!v) throw new Error("missing required environment variable " + name);
  return v;
}

const CFG = {
  region: requiredEnv("EVIDENCE_ATTESTOR_REGION"),
  bucket: requiredEnv("EVIDENCE_ATTESTOR_BUCKET"),
  ssmParam: requiredEnv("EVIDENCE_ATTESTOR_SSM_PARAM"),
  ciReadRoleName: requiredEnv("EVIDENCE_ATTESTOR_CI_READ_ROLE_NAME"),
  // {"<env>": {"account": "123456789012", "region": "us-west-2"}, ...} --
  // a CloudFormation parameter deploy.py derives from route66's env table
  // (owner 2026-08-26: "these should be params coming into the lambda"), so
  // no account id is hand-copied into this file.
  devCI: JSON.parse(requiredEnv("EVIDENCE_ATTESTOR_DEV_CI_JSON")),
};

const PUBLIC_KEY = JSON.parse(fs.readFileSync(__dirname + "/public-key.json", "utf8"));
const s3 = new S3Client({ region: CFG.region });
const ssm = new SSMClient({ region: CFG.region });
const sts = new STSClient({ region: CFG.region });
let privateKeyPromise = null;
let browserLibPromise = null;

function b64url(buf) {
  return Buffer.from(buf).toString("base64url");
}

function bytesSha256(bytes) {
  return crypto.createHash("sha256").update(bytes).digest("hex");
}

async function streamSha256(body) {
  const h = crypto.createHash("sha256");
  for await (const chunk of body) h.update(chunk);
  return h.digest("hex");
}

function metadataValue(v, limit = 512) {
  return String(v == null ? "" : v)
    .replace(/[^\x20-\x7E]/g, "?")
    .slice(0, limit);
}

// The ticket-facing summary is a caller-authored claim about WHY the evidence
// was captured, whether it is the BEFORE or AFTER half of a proof, and WHAT
// resource was examined. Those values must be signed beside the observed facts:
// rendering unsigned prose inside an attestation would let an edited comment
// change its meaning while the object signature continued to verify. Published
// attestations therefore require all three fields and store them in the existing
// observed.* map, which canonicalization v4 already signs generically.
function existingAttestationTarget(event) {
  // Target identity already belongs to each capture action. Requiring a second
  // free-form `target` would let two caller fields disagree about what Snapbot
  // actually observed, so derive the human claim from the canonical action input.
  switch (event.action) {
    case "capture-web-ui-screenshot":
    case "capture-http-raw":
      return metadataValue(event.url, 2000).trim();
    case "attest-ci-verdict":
      return metadataValue(`${event.env ?? event.environment ?? ""}@${event.target_sha ?? event.targetSHA ?? ""}`, 2000).trim();
    case "attest-aws-resource":
      return metadataValue(`${event.service ?? ""}.${event.operation ?? ""} ${JSON.stringify(event.params || {})}`, 2000).trim();
    default: {
      const ci = event.ci && typeof event.ci === "object" ? event.ci : {};
      const sha = ci.target_sha ?? ci.targetSHA;
      if (sha) return metadataValue(`${ci.env ?? ""}@${sha}`, 2000).trim();
      return metadataValue(`s3://${CFG.bucket}/${event.key ?? ""}${event.version_id ? `?versionId=${event.version_id}` : ""}`, 2000).trim();
    }
  }
}

function requiredAttestationContext(event) {
  const intent = metadataValue(event.intent, 1000).trim();
  const category = metadataValue(event.category, 16).trim().toUpperCase();
  const target = existingAttestationTarget(event);
  if (!intent) throw new Error("published attestation requires intent");
  if (category !== "BEFORE" && category !== "AFTER") {
    throw new Error("published attestation category must be BEFORE or AFTER");
  }
  if (!target) throw new Error("published attestation action has no target identity");
  return {
    "claim.intent": intent,
    "claim.category": category,
    "claim.target": target,
  };
}

// Run manifests intentionally have no GitHub publication and therefore no
// human-facing claim. Every ticket publication is validated by the handler and
// arrives here with this private normalized map attached to its capture event.
function addAttestationContext(observed, event) {
  Object.assign(observed, event._attestation_context || {});
  return observed;
}

// ---------------------------------------------------------------------------
// PHASE MARKERS.
//
// WHY THIS EXISTS -- the 2026-09-05 five-timeout incident (GH #3177). Five
// consecutive capture-web-ui-screenshot invocations with --click "#submit"
// against the DMV dev contact form each ended "Duration: 60000.00 ms,
// Status: timeout", and CloudWatch held NOTHING for any of them except START,
// END and REPORT: this handler emitted no application log line at all. So the
// one question worth asking -- which phase spent the budget: chromium cold
// start, page.goto, the lazy-load scroll, a click's navigation wait, the settle,
// the fullPage shot, the S3 put, or the signing -- was unanswerable from the
// logs, and the only way to narrow it was to re-run the whole 60s x 1536MB
// invocation with the inputs changed by hand. A capture is a long chain of
// steps against a live third-party page; every step now names itself and the
// elapsed ms it reached.
//
// The clock starts when the HANDLER starts rather than when capturePNG does, so
// module init and the browser cold start appear as the gap BEFORE the first
// marker instead of hiding inside it.
//
// NOTHING FROM THE PAGE IS LOGGED. A marker carries the phase name, elapsed ms,
// and for the click loop the caller's own selector (already ASCII-clamped by
// metadataValue in parseClickSelectors) plus a boolean. Never a cookie, a
// header, a response body or any page text: CloudWatch Logs is a diagnostic
// channel, not an evidence channel, and the invocation-only contract cookies
// travel under (see normalizeCookies / cookieFingerprint) would be broken by one
// careless interpolation.
let handlerStartedAtMS = Date.now();

function phase(name, detail) {
  console.log("[phase] " + name + " +" + (Date.now() - handlerStartedAtMS) + "ms" +
    (detail ? " " + detail : ""));
}

function safeSegment(s) {
  const cleaned = metadataValue(s, 160).replace(/[^A-Za-z0-9._-]+/g, "-").replace(/^-+|-+$/g, "");
  return cleaned || "evidence";
}

// Every URL this Lambda returns names the exact version it attested (owner
// 2026-08-26: fix "public URL omits versionId"). A verifier following the URL
// therefore fetches the attested bytes even after the key is overwritten.
function publicURL(bucket, key, versionId) {
  const base = "https://" + bucket + ".s3." + CFG.region + ".amazonaws.com/" +
    key.split("/").map(encodeURIComponent).join("/");
  return versionId ? base + "?versionId=" + encodeURIComponent(versionId) : base;
}

// ---------------------------------------------------------------------------
// Canonical manifest v4. One line per observed fact, keys sorted within the
// observed.* block; values are single-line ASCII (newlines would let one field
// forge another). scripts/cicd/verify_evidence_attestation.py rebuilds this
// text byte for byte from the sidecar's fields and the object it fetches.
// ---------------------------------------------------------------------------
function manifestLine(k, v) {
  return k + "=" + String(v == null ? "" : v).replace(/[\r\n]/g, " ") + "\n";
}

function canonicalManifest(obj, observed) {
  let out = CANONICALIZATION + "\n";
  out += manifestLine("bucket", obj.bucket);
  out += manifestLine("key", obj.key);
  out += manifestLine("version-id", obj.version_id);
  out += manifestLine("sha256", obj.sha256);
  out += manifestLine("content-length", obj.content_length);
  out += manifestLine("content-type", obj.content_type);
  out += manifestLine("last-modified", obj.last_modified);
  out += manifestLine("etag", obj.etag);
  out += manifestLine("attested-at-utc", obj.attested_at_utc);
  for (const k of Object.keys(observed).sort()) out += manifestLine("observed." + k, observed[k]);
  return out;
}

// ---------------------------------------------------------------------------
// Signing key.
// ---------------------------------------------------------------------------
function decryptPrivateKeyBlob(blob) {
  const wrapped = JSON.parse(blob);
  if (wrapped.v !== 1 || wrapped.alg !== "aes-256-gcm") {
    throw new Error("unsupported private-key wrapper");
  }
  const unwrap = Buffer.from(UNWRAP_KEY_B64, "base64");
  const decipher = crypto.createDecipheriv("aes-256-gcm", unwrap, Buffer.from(wrapped.nonce_b64, "base64"));
  decipher.setAAD(Buffer.from(wrapped.aad, "utf8"));
  const ciphertext = Buffer.from(wrapped.ciphertext_b64, "base64");
  const tag = ciphertext.subarray(ciphertext.length - 16);
  const enc = ciphertext.subarray(0, ciphertext.length - 16);
  decipher.setAuthTag(tag);
  const plaintext = Buffer.concat([decipher.update(enc), decipher.final()]);
  const key = JSON.parse(plaintext.toString("utf8"));
  if (key.v !== 1 || key.alg !== "ed25519") throw new Error("unsupported signing key");
  if (key.key_id !== PUBLIC_KEY.key_id || key.public_key_b64 !== PUBLIC_KEY.public_key_b64) {
    throw new Error("SSM private key does not match repo-pinned public key");
  }
  return crypto.createPrivateKey({
    key: {
      kty: "OKP",
      crv: "Ed25519",
      x: b64url(Buffer.from(key.public_key_b64, "base64")),
      d: b64url(Buffer.from(key.private_seed_b64, "base64")),
    },
    format: "jwk",
  });
}

async function privateKey() {
  if (!privateKeyPromise) {
    privateKeyPromise = ssm
      .send(new GetParameterCommand({ Name: CFG.ssmParam, WithDecryption: false }))
      .then((res) => {
        const v = res && res.Parameter && res.Parameter.Value;
        if (!v) throw new Error("SSM parameter " + CFG.ssmParam + " has no value");
        return decryptPrivateKeyBlob(v);
      })
      .catch((err) => {
        privateKeyPromise = null;
        throw err;
      });
  }
  return privateKeyPromise;
}

// ---------------------------------------------------------------------------
// DEV CI capture. Reads the env's ci-orchestrator through the narrow
// cross-account role and reports the LATEST execution exactly as found.
// Never throws on CI state; an unreachable account or malformed history is
// itself captured as ci.error so the signature covers "could not read CI".
// ---------------------------------------------------------------------------
function executionInputSHA(input) {
  if (!input) return "";
  try {
    return String(JSON.parse(input).sha || "").trim();
  } catch (_) {
    return "";
  }
}

function finalizerScheduledInput(ev) {
  if (ev.type === "TaskScheduled" && ev.taskScheduledEventDetails && ev.taskScheduledEventDetails.parameters) {
    return ev.taskScheduledEventDetails.parameters;
  }
  if (ev.type === "LambdaFunctionScheduled" && ev.lambdaFunctionScheduledEventDetails && ev.lambdaFunctionScheduledEventDetails.input) {
    return ev.lambdaFunctionScheduledEventDetails.input;
  }
  return "";
}

function parseFinalizerPayload(raw) {
  try {
    const parsed = JSON.parse(raw);
    const payload = parsed.Payload || parsed;
    if (!Object.prototype.hasOwnProperty.call(payload, "mode")) return null;
    return { mode: String(payload.mode || ""), exitCode: Number(payload.exitCode) };
  } catch (_) {
    return null;
  }
}

async function readFinalizerInput(client, executionArn) {
  let nextToken = undefined;
  for (;;) {
    const resp = await client.send(new GetExecutionHistoryCommand({
      executionArn, reverseOrder: true, includeExecutionData: true, nextToken,
    }));
    for (const ev of resp.events || []) {
      const raw = finalizerScheduledInput(ev);
      if (!raw) continue;
      const parsed = parseFinalizerPayload(raw);
      if (parsed) return parsed;
    }
    if (!resp.nextToken) return null;
    nextToken = resp.nextToken;
  }
}

async function devSfnClient(envName) {
  const env = CFG.devCI[envName];
  if (!env) throw new Error("no DEV CI target configured for env " + envName);
  const assumed = await sts.send(new AssumeRoleCommand({
    RoleArn: "arn:aws:iam::" + env.account + ":role/" + CFG.ciReadRoleName,
    RoleSessionName: "r66-evidence-attestor-" + envName.replace(/[^A-Za-z0-9+=,.@-]/g, "-"),
  }));
  const c = assumed.Credentials;
  if (!c) throw new Error("AssumeRole returned no credentials for " + envName);
  return {
    env,
    client: new SFNClient({
      region: env.region,
      credentials: { accessKeyId: c.AccessKeyId, secretAccessKey: c.SecretAccessKey, sessionToken: c.SessionToken },
    }),
  };
}

async function captureDevCI(envName, targetSHA) {
  const ci = {
    "ci.env": envName,
    "ci.target-sha": targetSHA,
    "ci.checked-at-utc": new Date().toISOString(),
  };
  try {
    const { env, client } = await devSfnClient(envName);
    const stateMachineArn = "arn:aws:states:" + env.region + ":" + env.account + ":stateMachine:" + envName + "-ci-orchestrator";
    // Newest first; the FIRST execution is the one that defines DEV right now,
    // whatever its state. That is the fact worth signing.
    const listed = await client.send(new ListExecutionsCommand({ stateMachineArn, maxResults: 1 }));
    const ex = (listed.executions || [])[0];
    if (!ex || !ex.executionArn) {
      ci["ci.error"] = "no execution found for " + stateMachineArn;
      return ci;
    }
    const described = await client.send(new DescribeExecutionCommand({ executionArn: ex.executionArn }));
    const executedSHA = executionInputSHA(described.input);
    ci["ci.execution-arn"] = ex.executionArn;
    ci["ci.executed-sha"] = executedSHA;
    ci["ci.sfn-status"] = String(described.status || "");
    ci["ci.start-date"] = described.startDate ? described.startDate.toISOString() : "";
    ci["ci.stop-date"] = described.stopDate ? described.stopDate.toISOString() : "";
    // No ci.sha-match here (removed in v4, GH #3840): whether the executed sha
    // equals the caller's claimed target-sha is a comparison a reader makes from
    // the two raw fields above, not a fact read from AWS.
    let finalizer = null;
    try {
      finalizer = await readFinalizerInput(client, ex.executionArn);
    } catch (err) {
      ci["ci.error"] = "history: " + String(err && err.message ? err.message : err);
    }
    ci["ci.finalizer-mode"] = finalizer ? finalizer.mode : "";
    ci["ci.worker-exit-code"] = finalizer && Number.isFinite(finalizer.exitCode) ? String(finalizer.exitCode) : "";
    // No ci.true-green (removed in v4, GH #3840). Owner 2026-09-13: "snapbot
    // does not judge, it only snaps." The raw sfn-status, finalizer-mode and
    // worker-exit-code above are what was observed; deciding greenness is the
    // reader's job, against whichever execution id the reader cares about
    // (owner: "The next step can check if a non-latest id is green").
  } catch (err) {
    // A failed read is itself the snapshot: ci.error alone, no verdict field.
    ci["ci.error"] = String(err && err.message ? err.message : err);
  }
  return ci;
}

// ---------------------------------------------------------------------------
// Attest one object version: fetch, hash, (optionally) capture CI, sign, and
// write the sidecar. `observed` carries any facts this Lambda itself produced
// upstream (a screenshot capture); callers cannot inject into it because the
// handler only ever passes its own values.
// ---------------------------------------------------------------------------
// The paste-ready block, owner directive 2026-09-02: "the attestor returns the
// text and the agent or python reposts it blindly into the comment, without
// interpretation."
//
// Every line is a fact this Lambda observed and signed, rendered here so the
// poster composes nothing. That is the point: on 2026-09-02 a GH #3150 evidence
// pair was verified, byte-identical, and described in an agent's own prose as
// "perfectly rendered" while both images showed the reported half-width defect.
// A block the poster cannot edit removes that failure from the loop entirely.
// A human-readable claim about what the evidence MEANS is still the poster's
// own sentence, written OUTSIDE this block and owned by whoever writes it.
// Alt text for the inlined image, built ONLY from signed observed fields. It is
// what a reader sees when the proxy fails, so it must not editorialize: no claim
// about whether the page is correct, only which capture this is.
function inlineImageAlt(statement) {
  const o = statement.observed || {};
  const parts = [
    o["capture.evidence-type"] || "evidence",
    o["capture.final-url"] || o["capture.requested-url"] || "",
    o["capture.viewport"] || "",
  ].filter(Boolean);
  // The sha is the tie-breaker a reader needs when two captures of the same URL
  // and viewport sit in one comment: it is the only part that differs between a
  // before and an after of the same page.
  parts.push("sha256 " + String(statement.object.sha256 || "").slice(0, 12));
  return parts.join(" — ").replace(/[\[\]]/g, "");
}

// The facts a ticket shows, in reading order. Owner 2026-09-03. Two synthetic
// names -- evidence-url and attestation-url -- are the URLs this attestation
// produced; every other name is looked up in the signed `observed` block, so a
// typo here renders nothing rather than inventing a value.
const TICKET_FACTS = [
  "evidence-url",
  "attestation-url",
  // The REQUESTED url sits next to the final one because on an http-raw capture
  // they are frequently different, and that difference is the finding: a ticket
  // claiming "/foo returns 200" is not proven by a 200 measured at /login after
  // two hops. On a capture where they are identical the row is redundant but
  // cheap; on the capture where it matters it is the whole story.
  "capture.requested-url",
  "capture.final-url",
  // Placed beside the URL rather than where transfer-bytes sat, because status is
  // read WITH the address it belongs to: "which page, and what did it answer".
  // Swapped in for capture.timing.transfer-bytes (owner 2026-09-03) after a
  // GH #2921 Chicago investigation had to fall back to the verifier to learn a
  // capture was 200 and not the 404 a sibling ticket describes -- on a
  // page-crash ticket the status IS the claim.
  "capture.http-status",
  // THE PERTURBATION PAIR, read immediately after the address and the status,
  // because they qualify everything below them: an image taken with a subresource
  // aborted or with a dropdown clicked open is evidence of an INDUCED state, and
  // a reader who does not know that misreads the picture. Both render "none" on
  // an untouched capture rather than dropping out of the table, so the absence of
  // perturbation is itself stated (owner rule: an induced condition is declared
  // evidence or it is fabrication). GH #3180 is the capture that needs the second
  // row: its defect only exists while the Property Type multiselect is open.
  "capture.blocked-url-patterns",
  "capture.click-selectors",
  // GH #3862: the ordered step script belongs with the perturbation pair for
  // the same reason — a page reached through nine clicks is an induced state.
  "capture.steps",
  "capture.header.x-amz-cf-id",
  "capture.body-bytes",
  "capture.header.cache-control",
  "capture.header.content-type",
  "capture.header.x-amz-cf-pop",
  "capture.header.x-cache",
  "capture.ttfb-ms",
  "capture.timing.dom-complete-ms",
  "capture.load-event-ms",
  "capture.timing.dom-interactive-ms",
  // http-raw facts. redirect-count says at a glance whether the address a reader
  // sees is the address that was asked for; the per-hop lines stay in the sidecar
  // because a chain renders as N*3 rows and the count is what triggers a reader to
  // go look. `location` and `age` are the two headers that distinguish "the origin
  // is fixed" from "the edge is still holding old bytes".
  "capture.redirect-count",
  "capture.header.location",
  "capture.header.age",
  // The body hash is the object hash for an http-raw capture, but it is ALSO
  // signed for a screenshot, where it is the only way to compare the markup behind
  // two images that differ in live data.
  "capture.body-sha256",
  // No code.* rows: this Lambda no longer produces evidence-type:code (see the
  // file header). `cloud-compose attest code` writes those statements and renders
  // its own output.
  // Raw CI snapshot rows only (v4, GH #3840: "snapbot does not judge, it only
  // snaps."). The execution arn names WHICH run was the newest, so a reader can
  // judge that exact id; the finalizer pair and sfn-status are the raw inputs a
  // reader's own green check consumes.
  "ci.execution-arn",
  "ci.executed-sha",
  "ci.target-sha",
  // A snapshot decays: it names one build, read at one instant. Without the
  // clock and the status a reader cannot tell a run that succeeded an hour ago from
  // one that succeeded in March against a sha main has long since passed.
  "ci.sfn-status",
  "ci.finalizer-mode",
  "ci.worker-exit-code",
  "ci.checked-at-utc",
  "ci.error",
];

function ticketFactValue(name, statement, observed, url, attestationURL) {
  if (name === "evidence-url") return url;
  if (name === "attestation-url") return attestationURL;
  // `object.<field>` addresses the attested object itself; everything else is an
  // observed fact. Both are signed, so neither is more trustworthy than the other.
  if (name.startsWith("object.")) return statement.object[name.slice(7).replace(/-/g, "_")];
  return observed[name];
}

// The largest result body inlined into a ticket comment. GitHub renders a
// comment up to 65536 characters, so a whole aws-resource dump can exceed what
// a comment can hold; 64 KiB is the cap, and past it the block says so and
// points at the object rather than silently ending mid-JSON.
const RESULT_TEXT_CAP_BYTES = 64 * 1024;

// WHY THE RESULT BODY IS RENDERED, not just its metadata. Owner 2026-09-06 on
// GH #3678: "the cfn attestation is lacking the actual output". Until this, an
// attest-aws-resource block carried only observed.aws.* rows -- service,
// operation, caller, params hash, http status -- so a ticket comment built from
// evidence_text verbatim proved that a read HAPPENED and showed nothing of what
// it RETURNED. The claim being closed is almost always about the CONTENT of the
// dump (this stack has that parameter, this table has that TTL), and a reader
// had to leave the ticket and fetch the object to see it.
//
// The bytes emitted here are the exact bytes PUT to S3 and hashed into
// object.sha256, in the same serialization, so a reader can hash the fenced
// block and compare. That is why the body is threaded down from the caller
// rather than re-serialized from `statement`: a second JSON.stringify of a
// reparsed doc would render identical-looking text that hashes differently.
//
// RENDERING ONLY: the signed statement and its canonicalization
// (r66-evidence-attestation-v4) are untouched by this section.
function renderResultSection(objectBodyText, url) {
  const lines = [];
  const buf = Buffer.from(objectBodyText, "utf8");
  const truncated = buf.length > RESULT_TEXT_CAP_BYTES;
  lines.push("observed.aws.result:");
  lines.push("");
  lines.push("```json");
  lines.push(buf.subarray(0, RESULT_TEXT_CAP_BYTES).toString("utf8"));
  lines.push("```");
  if (truncated) {
    lines.push("");
    lines.push("observed.aws.result-truncated: true, full object at " + url);
  }
  lines.push("");
  return lines;
}

function renderEvidenceText(statement, url, attestationURL) {
  const observed = statement.observed || {};
  // GH #3933: the ticket is an adjudication surface, not a sidecar dump. Keep
  // only the signed human claim and its clock visible; the versioned Evidence
  // and Statement links retain every object/CI/capture/AWS fact and signature.
  const lines = ["-----BEGIN ROUTE66 SIGNED ATTESTATION-----", ""];
  // Inline the image so the evidence is SEEN, not merely linked (owner
  // 2026-09-03). A link is a URL a reader has to choose to follow, and the
  // GH #3150 misreading happened precisely because nobody opened the picture;
  // an image that renders in the comment body puts the pixels in front of every
  // reader of the thread by default. The evidence bucket is world-readable and
  // these objects are image/png, so GitHub proxies them through camo and the
  // ?versionId= survives — the rendered image is the exact attested version,
  // not whatever later overwrote the key.
  //
  // Emitted only for image content types: a markdown image tag around a JSON
  // aws-resource-dump would render as a broken-image icon and read as evidence
  // that failed to load.
  if (/^image\//.test(String(statement.object.content_type || ""))) {
    lines.push("![" + inlineImageAlt(statement) + "](" + url + ")");
    lines.push("");
  }
  lines.push(`**${observed["claim.category"]}: ${observed["claim.intent"]}**`, "");
  lines.push(`Target: ${observed["claim.target"]}`, "");
  lines.push(`Attested: ${statement.object.attested_at_utc}`, "");
  lines.push(`Evidence: <${url}>`, "");
  lines.push(`Statement: <${attestationURL}>`, "");
  lines.push(`${statement.canonicalization} · Key-ID: ${statement.key_id}`, "");
  lines.push("-----END ROUTE66 SIGNED ATTESTATION-----");
  return lines.join("\n");
}

// Every observed field, including the compact claim.* summary, is signed in the
// canonical manifest. The linked sidecar remains the complete machine record.
async function attest(key, versionId, observed) {
  if (!key || typeof key !== "string") throw new Error("missing object key");
  const headArgs = { Bucket: CFG.bucket, Key: key };
  if (versionId) headArgs.VersionId = versionId;
  const head = await s3.send(new HeadObjectCommand(headArgs));
  const vid = head.VersionId || versionId || "";
  if (!vid) throw new Error("object has no VersionId; the evidence bucket must be versioned");
  const get = await s3.send(new GetObjectCommand({ Bucket: CFG.bucket, Key: key, VersionId: vid }));
  const sha256 = await streamSha256(get.Body);

  const obj = {
    bucket: CFG.bucket,
    key,
    version_id: vid,
    sha256,
    content_length: String(head.ContentLength == null ? "" : head.ContentLength),
    content_type: head.ContentType || "",
    last_modified: head.LastModified ? head.LastModified.toISOString() : "",
    etag: head.ETag || "",
    attested_at_utc: new Date().toISOString(),
  };
  const manifest = canonicalManifest(obj, observed);
  const keyObj = await privateKey();
  const signature = crypto.sign(null, Buffer.from(manifest, "utf8"), keyObj).toString("base64");
  const statement = {
    // Tracks the canonicalization line (v4 since GH #3840) so a verifier can
    // pick the matching manifest rebuild from either field.
    v: STATEMENT_VERSION,
    canonicalization: CANONICALIZATION,
    algorithm: PUBLIC_KEY.algorithm,
    key_id: PUBLIC_KEY.key_id,
    public_key_b64: PUBLIC_KEY.public_key_b64,
    object: obj,
    observed,
    manifest,
    manifest_sha256: bytesSha256(Buffer.from(manifest, "utf8")),
    signature_b64: signature,
  };
  const sidecarKey = key + ".attestation.json";
  const put = await s3.send(new PutObjectCommand({
    Bucket: CFG.bucket,
    Key: sidecarKey,
    Body: JSON.stringify(statement, null, 2),
    ContentType: "application/json",
  }));
  const url = publicURL(CFG.bucket, key, vid);
  const attestationURL = publicURL(CFG.bucket, sidecarKey, put.VersionId || "");
  return Object.assign({}, statement, {
    url,
    attestation_key: sidecarKey,
    attestation_url: attestationURL,
    evidence_text: renderEvidenceText(statement, url, attestationURL),
  });
}

async function attestExistingObject(event) {
  if (event.bucket && event.bucket !== CFG.bucket) {
    throw new Error("bucket " + event.bucket + " is not the evidence bucket");
  }
  const observed = {};
  const ci = event.ci || {};
  if (ci.env || ci.target_sha) {
    const envName = metadataValue(ci.env, 64);
    const targetSHA = metadataValue(ci.target_sha, 80);
    if (!envName || !targetSHA) throw new Error("ci requires both env and target_sha");
    Object.assign(observed, await captureDevCI(envName, targetSHA));
  }
  addAttestationContext(observed, event);
  return attest(event.key, event.version_id ? String(event.version_id) : "", observed);
}

// ---------------------------------------------------------------------------
// CI snapshot attestation (action name "attest-ci-verdict" kept for callers).
//
// WHY THIS ACTION EXISTS: captureDevCI has only ever ridden along on some OTHER
// artifact -- a screenshot, an AWS resource dump. But a whole class of tickets
// (the evidence-type:ci-report set) has no artifact at all: the thing recorded
// IS the dev CI state. Owner 2026-09-13: "snapbot should attest the state of the
// CI environment when it is asked to do so (start and end of a test run)" and
// "snapbot does not judge, it only snaps." Before this action that state could
// only be recorded by capturing an unrelated screenshot for its CI ride-along
// fields, which puts an irrelevant picture in the ticket and makes the actual
// evidence a footnote of it. Here the CI read is the artifact, and whether it
// is green is decided by the reader, never here.
//
// The snapshot is written to S3 FIRST and attested SECOND, because `attest`
// re-fetches and hashes the stored bytes; nothing in this Lambda signs bytes
// that exist only in memory.
//
// WHY THE SHA AND THE CLOCK ARE MANDATORY IN THE SIGNED DOCUMENT: a CI state is
// a fact about one build at one instant, and it DECAYS -- the instant main
// moves, a snapshot recorded here describes a build nobody is running any more.
// ci.executed-sha names which build, and ci.checked-at-utc names when the state
// was read; both come from captureDevCI and are asserted below so a future
// refactor of that function cannot silently drop them and leave an undated
// snapshot signed as if it were current.
// ---------------------------------------------------------------------------
async function attestCIVerdict(event) {
  const issue = metadataValue(event.issue ?? event.issue_number ?? "unknown", 64);
  const envName = metadataValue(event.env ?? event.environment, 64);
  const targetSHA = metadataValue(event.target_sha ?? event.targetSHA, 80);
  if (!envName) throw new Error("attest-ci-verdict requires env");
  if (!targetSHA) throw new Error("attest-ci-verdict requires target_sha");

  const ci = await captureDevCI(envName, targetSHA);
  // captureDevCI never throws (an unreachable account is captured as ci.error),
  // so ci.executed-sha may legitimately be absent on a failed read. What must
  // never be absent is the clock: a snapshot with no timestamp cannot be aged.
  if (!ci["ci.checked-at-utc"]) throw new Error("CI capture produced no ci.checked-at-utc");
  if (ci["ci.executed-sha"] === undefined) ci["ci.executed-sha"] = "";

  const capturedAt = ci["ci.checked-at-utc"];
  const stamp = capturedAt.replace(/[-:]/g, "").replace(/\.\d{3}Z$/, "Z");
  const uuid = crypto.randomUUID();
  const key = envName + "/issue-evidence/issue-" + safeSegment(issue) + "/" + uuid + "/" +
    stamp + "-" + safeSegment(envName) + "-ci-verdict.json";
  // The stored object is the ci.* map exactly as it will be signed, so the
  // evidence object and the manifest cannot disagree about the snapshot.
  const doc = {
    v: 1,
    evidence_type: "ci-verdict",
    env: envName,
    issue,
    target_sha: targetSHA,
    ci,
  };
  const put = await s3.send(new PutObjectCommand({
    Bucket: CFG.bucket,
    Key: key,
    Body: Buffer.from(JSON.stringify(doc, null, 2), "utf8"),
    ContentType: "application/json",
    // Informational labels only; the signed facts are in `observed` below.
    Metadata: {
      "environment": envName, "issue-number": issue, "target-sha": targetSHA,
      "evidence-type": "ci-verdict", "captured-by": "command-center evidence-attestor",
    },
  }));
  const observed = addAttestationContext(Object.assign({
    "ci.evidence-type": "ci-verdict",
    "ci.issue": issue,
  }, ci), event);
  return attest(key, put.VersionId || "", observed);
}

// ---------------------------------------------------------------------------
// Screenshot capture. The Lambda drives Chromium itself, so every capture fact
// in `observed` is first-hand.
// ---------------------------------------------------------------------------
async function browserLibs() {
  if (!browserLibPromise) {
    browserLibPromise = Promise.all([import("puppeteer-core"), import("@sparticuz/chromium")])
      .then(([puppeteerMod, chromiumMod]) => ({
        puppeteer: puppeteerMod.default || puppeteerMod,
        chromium: chromiumMod.default || chromiumMod,
      }));
  }
  return browserLibPromise;
}

function normalizeCookies(raw, url) {
  if (!raw) return [];
  if (Array.isArray(raw)) {
    return raw.map((c) => Object.assign({ url }, c)).filter((c) => c.name && c.value != null);
  }
  if (typeof raw === "object") {
    return Object.entries(raw).map(([name, value]) => ({ name, value: String(value), url }));
  }
  throw new Error("cookies must be an array of cookie objects or a name/value object");
}

function cookieFingerprint(cookies) {
  const redacted = cookies.map((c) => ({
    name: c.name, domain: c.domain || "", path: c.path || "",
    secure: !!c.secure, httpOnly: !!c.httpOnly, sameSite: c.sameSite || "",
  })).sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b)));
  return bytesSha256(Buffer.from(JSON.stringify(redacted), "utf8"));
}

// Optional PERTURBATION: CSS selectors this Lambda clicks, in order, before the
// shutter.
//
// WHY IT EXISTS — GH #3180. Owner 2026-09-05: "it's hard to select Property Type
// because it gets occluded by the containing div boundaries". That defect is a
// property of the OPEN Bootstrap multiselect dropdown inside the DMV Filters
// panel: with the dropdown shut the page looks identical before and after any
// fix, so a fixed-viewport load with no interaction can photograph neither the
// symptom nor its repair. Driving the page into the state under test is the only
// way this action can evidence an interaction-only defect at all.
//
// WHY THE LIST IS SIGNED (capture.click-selectors below): the same rule
// block_url_patterns already follows — any induced condition is DECLARED
// evidence or it is fabrication. A reader looking at an image of an opened
// dropdown cannot tell a page that opened it from a capture that clicked it, so
// the clicks must travel inside the signed statement rather than in the
// invocation that produced it.
//
// WHY MALFORMED INPUT THROWS rather than being filtered the way block_url_patterns
// filters junk: a silently dropped selector still yields a successful capture, of
// the WRONG page state, which is precisely the misleading evidence this action
// exists to prevent. Bounded at 8 because a capture is a photograph and not a UI
// test; a longer interaction script belongs in web-regression.
//
// NOT APPLIED TO capture-http-raw: that action drives no browser at all. It is a
// hop-by-hop https.request (requestOneHop, below), with no page, no DOM and no
// clickable element, so click_selectors has no meaning there and is not accepted.
const MAX_CLICK_SELECTORS = 8;

function parseClickSelectors(raw) {
  if (raw === undefined || raw === null) return [];
  if (!Array.isArray(raw)) {
    throw new Error("click_selectors must be an array of CSS selector strings");
  }
  if (raw.length > MAX_CLICK_SELECTORS) {
    throw new Error("click_selectors accepts at most " + MAX_CLICK_SELECTORS + " selectors; got " + raw.length);
  }
  return raw.map((sel, i) => {
    if (typeof sel !== "string" || !sel.trim()) {
      throw new Error("click_selectors[" + i + "] must be a non-empty CSS selector string");
    }
    // metadataValue for the same reason block_url_patterns uses it: the value ends
    // up on a single ASCII manifest line, and a selector carrying a control
    // character would otherwise be signed as something no verifier can rebuild.
    return metadataValue(sel.trim(), 256);
  });
}

// Optional ORDERED INTERACTION SCRIPT: `steps: [{click: "<selector>"},
// {wait_ms: N}, ...]`, run in caller order where click_selectors would run.
//
// WHY IT EXISTS — GH #3862. The defect ("Areas: Jump To zips 92104,92102
// survive unchecking North Park / South Park and the map picker restores them")
// exists only at the END of a multi-page session: check two Areas, search,
// reopen the revise panel, uncheck both, submit "pick from map", then look at
// the landing page's Jump To box. That is two navigating clicks with page
// transitions and a Bootstrap collapse between them, and click_selectors has no
// way to say "let the collapse finish before the next click". A step list whose
// entries are either a click or an explicit pause is the smallest shape that
// can drive such a session; it is still a photograph, not a UI test, so the
// verbs stay exactly these two.
//
// WHY IT IS SIGNED (capture.steps in captureWebUIScreenshot): the same rule as
// click_selectors and block_url_patterns — an induced page state is declared
// evidence or it is fabrication. The WAITS are signed too, because "the panel
// was photographed 800ms after it was toggled" qualifies what the image shows.
//
// WHY steps AND click_selectors ARE MUTUALLY EXCLUSIVE: two lists have no
// defined interleaving, and guessing one would photograph an order the caller
// never asked for. click_selectors keeps working unchanged on its own.
//
// WHY "|" IS REFUSED INSIDE A STEP SELECTOR: capture.steps is rendered as
// `click:<sel>|wait_ms:<n>|...` on one manifest line. JSON would put `\"`
// escapes into the armored GitHub block for any attribute selector, and a "|"
// inside a selector (CSS namespace or `|=`) would let one step read as two.
// Refusing it loudly keeps the signed line unambiguous; no capture so far has
// needed either construct.
//
// BOUNDS: 12 steps (GH #3862's sequence is 9 clicks plus pauses) and 5000ms
// per pause. The per-click cost is the click_selectors loop's own bound
// (5s find + 8s nav + 5s idle + 1.4s), so the Lambda's platform timeout stays
// the outer ceiling for a pathological all-navigating script, and each step's
// [phase] line names where the time went.
const MAX_STEPS = 12;
const MAX_STEP_WAIT_MS = 5000;

function parseSteps(raw) {
  if (raw === undefined || raw === null) return null;
  if (!Array.isArray(raw) || raw.length === 0) {
    throw new Error("steps must be a non-empty array of {click: selector} / {wait_ms: N} objects");
  }
  if (raw.length > MAX_STEPS) {
    throw new Error("steps accepts at most " + MAX_STEPS + " entries; got " + raw.length);
  }
  return raw.map((step, i) => {
    // Exactly one key per step: an object carrying both click and wait_ms has
    // no defined order between them, the same ambiguity refused above.
    const keys = step && typeof step === "object" && !Array.isArray(step) ? Object.keys(step) : [];
    if (keys.length !== 1) {
      throw new Error("steps[" + i + "] must be an object with exactly one of click, wait_ms");
    }
    if (keys[0] === "click") {
      const sel = step.click;
      if (typeof sel !== "string" || !sel.trim()) {
        throw new Error("steps[" + i + "].click must be a non-empty CSS selector string");
      }
      if (sel.includes("|")) {
        throw new Error("steps[" + i + "].click must not contain '|' (capture.steps separator)");
      }
      // metadataValue for the ASCII single-line manifest, as parseClickSelectors.
      return { click: metadataValue(sel.trim(), 256) };
    }
    if (keys[0] === "wait_ms") {
      const n = step.wait_ms;
      if (!Number.isInteger(n) || n < 0 || n > MAX_STEP_WAIT_MS) {
        throw new Error("steps[" + i + "].wait_ms must be an integer 0.." + MAX_STEP_WAIT_MS);
      }
      return { wait_ms: n };
    }
    // An unknown verb (type, hover, ...) is refused rather than skipped: a
    // skipped step still yields a successful capture of the wrong state.
    throw new Error("steps[" + i + "] has unsupported key " + JSON.stringify(keys[0]) + "; allowed: click, wait_ms");
  });
}

// Walk the document top to bottom in viewport-sized steps so lazy-loaded images
// and any IntersectionObserver-driven content below the fold actually render,
// then return to the top. Bounded: a page that keeps growing as it is scrolled
// (infinite feed) must not spin here, so the walk stops after a fixed number of
// steps and the shot is taken of whatever has rendered by then.
async function scrollThroughPage(page) {
  try {
    await page.evaluate(async () => {
      const step = window.innerHeight;
      const settle = () => new Promise((r) => setTimeout(r, 120));
      for (let y = 0, steps = 0; steps < 40; steps++) {
        y += step;
        if (y > document.body.scrollHeight) break;
        window.scrollTo(0, y);
        await settle();
      }
      window.scrollTo(0, 0);
      await settle();
    });
  } catch (err) {
    // A page that refuses to be scripted still deserves its screenshot; the shot
    // is the evidence, and a failed pre-scroll only risks unloaded lazy content.
  }
}

async function capturePNG(event) {
  const url = metadataValue(event.url, 2048);
  if (!/^https:\/\//.test(url)) throw new Error("capture-web-ui-screenshot requires an https URL");
  const width = Math.max(320, Math.min(3840, Number(event.viewport_width || event.width || 1365)));
  const height = Math.max(320, Math.min(3000, Number(event.viewport_height || event.height || 900)));
  const timeoutMS = Math.max(5000, Math.min(45000, Number(event.timeout_ms || 20000)));
  const waitMS = Math.max(0, Math.min(10000, Number(event.wait_ms || 1000)));
  const cookies = normalizeCookies(event.cookies || event.session_cookies, url);
  // FULL PAGE BY DEFAULT (owner 2026-09-03: "it would be better if the screenshot
  // is taller (2-3 screens tall, full HTML)"). A viewport-height shot answers only
  // "what is above the fold", and most reported defects are not there -- GH #3150's
  // Public Records panel sat below the fold on the very captures filed as evidence
  // for it. Evidence should show the whole document unless the caller says
  // otherwise, so this inverts the old opt-in `full_page` into an opt-out.
  const fullPage = event.full_page === undefined || event.full_page === null
    ? true
    : !!event.full_page;
  // Optional fault injection: abort any subresource request whose URL contains one
  // of these substrings. Exists so evidence can photograph a defect whose trigger
  // is a third-party script FAILING to load (GH #3150: the mobile half-page bug
  // fires only when Google Maps JS never sets its inline overflow on #map_canvas,
  // so a healthy capture shows nothing on either side). The blocked patterns are
  // part of the SIGNED statement (capture.blocked-url-patterns) — an induced
  // condition is declared evidence, never a hidden edit. The MAIN document is
  // never blocked, so the page under test is always the real served page.
  const blockPatterns = Array.isArray(event.block_url_patterns)
    ? event.block_url_patterns.map((p) => metadataValue(String(p), 256)).filter(Boolean).slice(0, 16)
    : [];
  // The second declared perturbation, parsed and signed exactly like the first
  // (see parseClickSelectors above for the GH #3180 rationale). Parsed HERE,
  // before the browser launches, so a malformed request fails without paying a
  // chromium cold start.
  const clickSelectors = parseClickSelectors(event.click_selectors);
  // GH #3862: the ordered step script, parsed before launch for the same
  // fail-before-cold-start reason. click_selectors is normalized into the same
  // step shape so ONE loop below performs every interaction and the signed
  // capture.steps line describes exactly what ran, whichever field was used.
  const parsedSteps = parseSteps(event.steps);
  if (parsedSteps && clickSelectors.length) {
    throw new Error("steps and click_selectors are mutually exclusive; put the clicks inside steps");
  }
  const interactions = parsedSteps || clickSelectors.map((sel) => ({ click: sel }));
  const { puppeteer, chromium } = await browserLibs();
  const executablePath = await chromium.executablePath();
  const browser = await puppeteer.launch({
    args: chromium.args, defaultViewport: { width, height }, executablePath, headless: chromium.headless,
  });
  try {
    const page = await browser.newPage();
    await page.setViewport({ width, height });
    if (event.headers && typeof event.headers === "object") {
      const headers = {};
      for (const [k, v] of Object.entries(event.headers)) {
        if (String(k).toLowerCase() === "cookie") continue;
        headers[k] = String(v);
      }
      await page.setExtraHTTPHeaders(headers);
    }
    if (cookies.length) await page.setCookie(...cookies);
    if (blockPatterns.length) {
      await page.setRequestInterception(true);
      page.on("request", (req) => {
        // The navigation itself always goes through: blocking the main document
        // would photograph an error page, not the served page under a fault.
        if (req.isNavigationRequest() && req.frame() === page.mainFrame()) return req.continue();
        const target = req.url();
        if (blockPatterns.some((p) => target.includes(p))) return req.abort();
        return req.continue();
      });
    }
    const response = await page.goto(url, { waitUntil: "networkidle2", timeout: timeoutMS });
    // First marker of the capture: everything before it -- module init, the
    // browser libraries, the chromium cold start, cookies and interception setup
    // and the networkidle2 navigation itself -- is the elapsed value printed
    // here. The status is the served response's, never any body text.
    phase("goto", "status=" + (response ? response.status() : 0));
    // Body and headers are read BEFORE the settle wait and the shot: both
    // describe the served document, and a later in-page navigation discards the
    // body puppeteer is holding.
    let bodySha256 = "unavailable";
    let bodyBytes = -1;
    try {
      const body = response ? await response.text() : "";
      bodyBytes = Buffer.byteLength(body, "utf8");
      bodySha256 = bytesSha256(Buffer.from(body, "utf8"));
    } catch (err) {
      bodySha256 = "unavailable: " + (err && err.message ? err.message : "read failed");
    }
    const responseHeaders = response ? response.headers() : {};
    const timing = await navigationTimingMS(page);
    // response.text() drains the body over CDP and navigationTimingMS runs a
    // page.evaluate, so this phase is a real cost on a large document and can
    // stall outright against a frame that has begun navigating. Only the byte
    // COUNT is logged -- never the body.
    phase("body+headers+timing", "body-bytes=" + bodyBytes);
    // A full-page shot renders the whole document, but anything lazy-loaded below
    // the fold has never been near the viewport and so was never asked to load --
    // the tall image would show real markup wrapped around empty photo frames and
    // read as a rendering defect that does not exist. Walking the page to the
    // bottom triggers those loads, and returning to the top keeps the image framed
    // the way a reader opens the page.
    // Marked because this walk is up to 40 steps x 120ms of settle plus whatever
    // the lazy loads it triggers cost, i.e. seconds on a long listing page, and
    // it is skipped entirely for a viewport-only capture -- a distinction that
    // was invisible in the logs.
    if (fullPage) {
      await scrollThroughPage(page);
      phase("scroll", "full-page=true");
    }
    // The declared clicks, in caller order: AFTER the networkidle2 load and the
    // lazy-load scroll, BEFORE the settle wait and the shutter. Ordering is
    // deliberate — scrollThroughPage walks the whole document and returns to the
    // top, which would scroll an opened dropdown out of frame or dismiss it
    // outright, and the settle wait after the last click is what lets its
    // transition finish before the screenshot.
    //
    // A selector that never becomes visible THROWS, failing the whole capture:
    // the Lambda then records nothing at all, which is the correct outcome. A
    // screenshot of the un-clicked page returned as if the interaction happened
    // would be filed on a ticket as evidence of a state it never reached.
    //
    // NAVIGATING CLICKS — GH #3177. A bare `page.click` is only correct for a
    // click that stays on the page (GH #3180's Property Type dropdown toggle,
    // which works). When the clicked element SUBMITS A FORM the click starts a
    // navigation, and every later call against the navigating frame —
    // scrollThroughPage's page.evaluate, page.screenshot, page.url() — stalls
    // behind it: on 2026-09-05 five consecutive invocations with --click
    // "#submit" on https://dmv.cal-dev.us1.click/contact-form ended
    // "Duration: 60000.00 ms, Status: timeout" in CloudWatch, i.e. the Lambda
    // ran to its own ceiling and produced no evidence at all. Puppeteer's
    // documented pattern for that case is to arm waitForNavigation BEFORE the
    // click and await it after.
    //
    // WHY THE PROBE AND NOT A PLAIN `await nav`: arming the navigation promise
    // and unconditionally awaiting it would make every NON-navigating click pay
    // the full 8s timeout before the catch fires — GH #3180's two-click dropdown
    // capture would go from ~0.8s to ~16s of pure waiting. So the 400ms settle
    // this loop already needed doubles as the navigation-start probe: race the
    // nav promise against it, and keep waiting on nav only if the frame's URL
    // moved, which is puppeteer's cached mainFrame URL (updated on navigation
    // COMMIT, no CDP round trip, and no page.evaluate that would itself stall
    // against the very frame we suspect is navigating). Non-navigating click:
    // ~400ms, unchanged. Navigating click: bounded by the 8s nav timeout.
    //
    // WHY THE CLICK PROMISE IS RACED AND NEVER AWAITED BARE — the 2026-09-05
    // 180s timeout, RequestId 9656ca79-0455-472a-98fd-bfdfaef846bb. That run
    // printed `[phase] goto +8124ms`, `[phase] body+headers+timing +8129ms`,
    // `[phase] scroll +8251ms` and then NOTHING until "Task timed out after
    // 180.00 seconds" — `[phase] click` never printed, so the stall was inside
    // this loop. Every other statement here is bounded (5s waitForSelector,
    // which throws on a miss; 400ms probe; 8s nav), which left exactly one
    // unbounded statement: `await page.click(sel)`. MECHANISM: a click on a
    // submit button commits a navigation, and the CDP response to the
    // Input.dispatchMouseEvent that puppeteer is waiting on is LOST WITH THE OLD
    // FRAME — the click promise then never settles, and no timeout of ours
    // applies to it because it is not our await that is waiting on the wire.
    // Arming waitForNavigation ahead of the click (added above for GH #3177) did
    // not help on its own: the code still awaited the click before it could ever
    // reach the race. Puppeteer's own guidance for a navigating click is to RACE
    // it against waitForNavigation rather than await it alone, which is what the
    // Promise.race below does.
    //
    // WHY THE URL PROBE IS NOT THE NAVIGATION SIGNAL — the 2026-09-05 180s
    // timeout, RequestId 3fef470d-821a-4fb4-b5d1-16d09664ebfa, `--click
    // "#submit"` on the DMV dev contact form with a session cookie. That run
    // printed `[phase] click-selector-ready +5635`, `[phase] click-issued
    // +5636`, `[phase] click +7039 navigated=false` and `[phase] settle
    // +10042`, then nothing until "Task timed out after 180.00 seconds". The
    // 1403ms between click-issued and click is exactly the 400ms probe plus the
    // 1000ms drain, both running to their timers: clickP never settled and nav
    // had not resolved. TWO FACTS follow, and they are why the loop below is
    // shaped as it is:
    //
    //   (a) THE PROBE-TIME URL IS STALE FOR A COMMITTING NAVIGATION. puppeteer's
    //       cached mainFrame URL updates on navigation COMMIT, and a form POST
    //       commits after the probe window — so `page.url() !== urlBeforeClick`
    //       read at +400ms says false for a click that IS navigating. The loop
    //       then skipped `await nav`, skipped the network drain, and handed a
    //       mid-navigation frame to the shutter.
    //   (b) AN UNSETTLED CLICK PROMISE WITHIN THE PROBE *IS* THE NAVIGATION
    //       SIGNAL. Control run ea4c2974 (the non-navigating GH #3180 dropdown)
    //       went `click-issued +6127` -> `click +6251`: 124ms, clickP settled at
    //       once. The navigating run's clickP never settled, for the reason
    //       already documented above (the CDP response to
    //       Input.dispatchMouseEvent is lost with the old frame). So clickSettled
    //       is tracked explicitly and `!clickSettled` ORs into `navigated`,
    //       catching the case the URL comparison provably misses. A
    //       non-navigating click still settles inside the probe and pays nothing.
    //
    // After a navigation is declared, `await nav` (its own 8s timeout, its own
    // catch) is followed by a bounded waitForNetworkIdle so the POST-navigation
    // document is quiescent before the shutter — the screenshot in the incident
    // hung against a frame that was still loading, not merely still navigating.
    //
    // Per-selector bound and the 8-selector cap are unchanged in shape: 5s to
    // find the element, then at most 8s of navigation plus a 5s network-idle
    // wait plus the 400ms settle, plus a final 1s drain.
    for (const step of interactions) {
      // GH #3862: a pause step. Bounded by MAX_STEP_WAIT_MS at parse time and
      // marked so a slow script shows which pause it spent, not only its clicks.
      if (step.wait_ms !== undefined) {
        await new Promise((resolve) => setTimeout(resolve, step.wait_ms));
        phase("step-wait", "wait-ms=" + step.wait_ms);
        continue;
      }
      const sel = step.click;
      await page.waitForSelector(sel, { visible: true, timeout: 5000 });
      // Splits the loop's cost into three observable sub-steps. On 2026-09-05
      // the single end-of-iteration marker could not distinguish "the selector
      // never appeared" from "the click promise hung", because neither printed.
      phase("click-selector-ready", sel);
      const urlBeforeClick = page.url();
      // Armed BEFORE the click: a navigation started by the click can commit
      // before the click promise resolves, and a waitForNavigation registered
      // afterwards would miss it and then hang for its whole timeout.
      // .catch(() => null) because "this click did not navigate" is the common,
      // expected outcome and must not fail the capture — and because it keeps
      // the promise handled in the branch below where it is never awaited.
      const nav = page
        .waitForNavigation({ waitUntil: "networkidle2", timeout: 8000 })
        .catch(() => null);
      // Issued, not awaited. Both handlers set clickSettled: the flag is the
      // navigation signal per fact (b) above, and "settled" means the CDP
      // response came back AT ALL — a click that rejects (frame detached
      // mid-dispatch) still tells us the wire was not lost with the old frame.
      // The rejection handler doubles as the .catch that keeps a late reject
      // from surfacing as an unhandled rejection that kills the node process.
      let clickSettled = false;
      const clickP = page.click(sel).then(
        () => {
          clickSettled = true;
        },
        () => {
          clickSettled = true;
        }
      );
      phase("click-issued", sel);
      // Fixed pause per click: a Bootstrap dropdown opens on a CSS transition
      // with no load event to wait on, and the NEXT selector in the list is
      // routinely inside the element this click just revealed. It is also the
      // navigation-start probe window described above. clickP joins the race so
      // an ordinary non-navigating click still proceeds as soon as it settles.
      await Promise.race([clickP, nav, new Promise((r) => setTimeout(r, 400))]);
      // Either signal declares a navigation: a URL that already moved (commit
      // landed inside the probe) or a click promise that has not settled by the
      // end of the probe (commit still in flight, fact (b) above). The OR is the
      // whole fix for RequestId 3fef470d — that run's URL comparison said false.
      const navigated = page.url() !== urlBeforeClick || !clickSettled;
      if (navigated) {
        await nav;
        // The navigation being COMMITTED is not the document being QUIET, and
        // it was a still-loading frame that hung the shutter for the remaining
        // ~170s of the budget. Bounded and caught: on a click that turned out
        // not to navigate after all, this is 5s of ceiling we never reach,
        // because an already-idle page resolves after idleTime.
        await page
          .waitForNetworkIdle({ idleTime: 500, timeout: 5000 })
          .catch(() => null);
      }
      // Final bounded drain: after a navigating click, clickP may still be
      // waiting on a CDP response that will never arrive. It is left unawaited
      // rather than leaked into the next iteration's race, where it would be
      // indistinguishable from that iteration's own click settling.
      await Promise.race([clickP, new Promise((r) => setTimeout(r, 1000))]);
      // THE marker the 2026-09-05 incident was missing: this loop is the only
      // phase whose cost depends on what the page did, and the navigated flag
      // says which of the two branches described above was taken -- a ~400ms
      // non-navigating click, or one that paid up to the 8s navigation wait. The
      // selector is the caller's own, already ASCII-clamped by
      // parseClickSelectors. click-settled distinguishes the two ways navigated
      // can be true, which is what the incident log could not do. The landing
      // URL is logged because "navigated" without an address is unactionable --
      // stripped to its PATH whenever it carries a query string, since a
      // post-submit redirect can echo form field values into the query and those
      // are user-entered data this log must not capture.
      const landedURL = page.url();
      const loggedURL = landedURL.includes("?")
        ? new URL(landedURL).pathname
        : landedURL;
      phase(
        "click",
        sel +
          " navigated=" +
          navigated +
          " click-settled=" +
          clickSettled +
          " url=" +
          loggedURL
      );
    }
    // NOTE: capture.body-sha256 / capture.body-bytes, capture.header.* and the
    // capture.timing.* block were all read ABOVE, before this loop, and they
    // therefore describe the PRE-CLICK document — that is deliberate (a
    // navigation discards the body puppeteer is holding), and it stays true when
    // a click navigates away. The one fact that must follow the clicks is the
    // address the shutter fired at: capture.final-url is `page.url()` read in the
    // returned object below, AFTER this loop and after the screenshot, so a
    // navigating click is visible to a reader as requested-url != final-url.
    if (waitMS) await new Promise((resolve) => setTimeout(resolve, waitMS));
    // Caller-controlled and up to 10s (map tiles need ~6000), so it is worth
    // seeing spent rather than inferring it from the gap between two markers.
    phase("settle", "wait-ms=" + waitMS);
    // Marks the shutter's START, not just its completion: on RequestId
    // 3fef470d-821a-4fb4-b5d1-16d09664ebfa the last line printed was `settle
    // +10042` and the next 170s were silent, which left "the settle wait hung"
    // and "the screenshot hung" indistinguishable. They are now two lines apart.
    phase("screenshot-start", "full-page=" + fullPage);
    // BOUNDED. page.screenshot against a frame that is still navigating or
    // loading does not time out on its own, and in the incident above it ate the
    // entire remaining 180s Lambda budget, so the invocation died on the
    // platform's timeout and reported NOTHING -- no phase, no error, no
    // evidence. Losing this race throws a named error instead: the capture still
    // fails (a screenshot of a half-rendered frame is not evidence), but it
    // fails with the phase in the message, so the next reader knows where to
    // look without correlating timestamps. 30s is chosen against the observed
    // cost of the legitimate worst case -- a fullPage shot of a long listing
    // page runs ~2-7s -- so it fires only for a genuine stall.
    const png = await Promise.race([
      page.screenshot({ type: "png", fullPage }),
      new Promise((_, reject) =>
        setTimeout(
          () => reject(new Error("screenshot exceeded 30s (phase=screenshot)")),
          30000
        )
      ),
    ]);
    const pngBytes = Buffer.from(png);
    // A fullPage shot of a document several screens tall is the single most
    // expensive step here after the load, it runs against a frame that a
    // navigating click may still be moving, and its output size is the input to
    // the S3 put that follows.
    phase("screenshot", "png-bytes=" + pngBytes.length + " full-page=" + fullPage);
    return {
      bytes: pngBytes,
      fullPage,
      // Read from the PNG's own IHDR rather than from what was requested: this is
      // the size of the image a reader is actually looking at, and it is how the
      // block can say "this is 3 screens tall" as a fact instead of an intention.
      imageWidth: pngBytes.length > 24 ? pngBytes.readUInt32BE(16) : -1,
      imageHeight: pngBytes.length > 24 ? pngBytes.readUInt32BE(20) : -1,
      requestedURL: url,
      finalURL: page.url(),
      httpStatus: response ? response.status() : 0,
      viewport: width + "x" + height,
      viewportHeight: height,
      cookieFingerprint: cookieFingerprint(cookies),
      cookieCount: cookies.length,
      blockedURLPatterns: blockPatterns,
      // The clicks actually performed, in order, whichever request field
      // carried them, so capture.click-selectors stays truthful for a steps
      // capture too; the full ordered script (waits included) is `steps`.
      clickSelectors: interactions.filter((s) => s.click !== undefined).map((s) => s.click),
      steps: interactions,
      responseHeaders,
      bodySha256,
      bodyBytes,
      ttfbMS: timing.ttfb,
      domContentLoadedMS: timing.domContentLoaded,
      loadEventMS: timing.load,
      timing,
    };
  } finally {
    await browser.close();
  }
}

// Browser-side navigation timing, read from the Navigation Timing API rather
// than measured around page.goto: wall clock around goto includes this Lambda's
// own cold start and scheduling, so it is not comparable between two captures.
// -1 records that the entry was unavailable, which is a fact; a 0 would read as
// an instantaneous response.
// The FULL PerformanceNavigationTiming breakdown, not just three roll-ups.
// WHY: "the page took 959ms" is not a diagnosis. A reader adjudicating a
// performance claim needs to know WHICH phase spent the time, because the
// remedies are unrelated -- slow DNS is a Route53/resolver problem, slow TCP+TLS
// is a connection-reuse or PoP problem, a large request-to-response gap is the
// origin thinking, a large response-to-responseEnd gap is transfer size or
// bandwidth, and time after responseEnd is the browser parsing and running our
// own JS. Collapsing those into one number throws away the only part that says
// what to fix.
//
// Every value is milliseconds, rounded. Phase durations are DIFFERENCES between
// adjacent marks; milestone values are absolute offsets from timeOrigin (the
// navigation start), which is what makes them comparable between two captures.
// -1 means the mark was unavailable -- a fact worth signing, and distinct from a
// 0 that would read as "instantaneous".
async function navigationTimingMS(page) {
  const UNAVAILABLE = {
    redirect: -1, dns: -1, tcp: -1, tls: -1, request: -1, response: -1,
    ttfb: -1, domInteractive: -1, domContentLoaded: -1, domComplete: -1,
    load: -1, transferBytes: -1, encodedBytes: -1, decodedBytes: -1,
    protocol: "", redirectCount: -1,
  };
  try {
    return await page.evaluate(() => {
      const nav = performance.getEntriesByType("navigation")[0];
      if (!nav) return null;
      // A mark of 0 means "this phase did not happen" (no redirect, a reused
      // connection, plaintext HTTP), which is NOT the same as a zero duration.
      // Reporting -1 for those keeps a skipped phase from reading as an
      // instantaneous one.
      const span = (a, b) => (a > 0 && b > 0 ? Math.round(b - a) : -1);
      const at = (t) => (t > 0 ? Math.round(t) : -1);
      return {
        redirect: span(nav.redirectStart, nav.redirectEnd),
        dns: span(nav.domainLookupStart, nav.domainLookupEnd),
        tcp: span(nav.connectStart, nav.connectEnd),
        // secureConnectionStart is 0 on a plaintext or reused connection, so
        // this correctly reports -1 rather than claiming a TLS handshake ran.
        tls: span(nav.secureConnectionStart, nav.connectEnd),
        request: span(nav.requestStart, nav.responseStart),
        response: span(nav.responseStart, nav.responseEnd),
        ttfb: span(nav.requestStart, nav.responseStart),
        domInteractive: at(nav.domInteractive),
        domContentLoaded: at(nav.domContentLoadedEventEnd),
        domComplete: at(nav.domComplete),
        load: at(nav.loadEventEnd),
        // Sizes belong with timing: a slow `response` phase is only meaningful
        // next to how many bytes crossed the wire, and transfer-vs-decoded is
        // the compression verdict.
        transferBytes: Number.isFinite(nav.transferSize) ? nav.transferSize : -1,
        encodedBytes: Number.isFinite(nav.encodedBodySize) ? nav.encodedBodySize : -1,
        decodedBytes: Number.isFinite(nav.decodedBodySize) ? nav.decodedBodySize : -1,
        // h2 vs http/1.1 changes what the connection numbers even mean.
        protocol: String(nav.nextHopProtocol || ""),
        redirectCount: Number.isFinite(nav.redirectCount) ? nav.redirectCount : -1,
      };
    }).then((t) => t || UNAVAILABLE);
  } catch (err) {
    return UNAVAILABLE;
  }
}

// One signed line per timing fact, matching the one-fact-per-line manifest rule.
// A single packed "timings" blob would hide a changed phase inside an otherwise
// unchanged string, which is exactly what the per-line canonicalization exists
// to prevent.
function timingObserved(t) {
  return {
    "capture.timing.redirect-ms": String(t.redirect),
    "capture.timing.dns-ms": String(t.dns),
    "capture.timing.tcp-ms": String(t.tcp),
    "capture.timing.tls-ms": String(t.tls),
    "capture.timing.request-ms": String(t.request),
    "capture.timing.response-ms": String(t.response),
    "capture.timing.dom-interactive-ms": String(t.domInteractive),
    "capture.timing.dom-complete-ms": String(t.domComplete),
    "capture.timing.redirect-count": String(t.redirectCount),
    "capture.timing.protocol": t.protocol,
    "capture.timing.transfer-bytes": String(t.transferBytes),
    "capture.timing.encoded-body-bytes": String(t.encodedBytes),
    "capture.timing.decoded-body-bytes": String(t.decodedBytes),
  };
}

// Response headers, one signed line each. The canonical manifest is already one
// fact per line, so a header belongs there as a peer of every other observed
// fact; folding them into a single blob would hide a changed value inside an
// unchanged string.
//
// The allowlist is what a reader adjudicates evidence with: content identity,
// caching, the redirect target, and the CDN's own hit/miss verdict. `set-cookie`
// and `authorization` are absent BY CONSTRUCTION — this bucket is world-readable
// and a capture of an authenticated page would otherwise publish a live session.
const RESPONSE_HEADER_ALLOWLIST = [
  "age", "cache-control", "content-encoding", "content-length", "content-type",
  "date", "etag", "expires", "last-modified", "location", "server", "vary",
  "via", "x-amz-cf-id", "x-amz-cf-pop", "x-cache", "x-content-type-options",
  "x-frame-options",
];

function headerObserved(headers) {
  const out = {};
  const lower = {};
  for (const [k, v] of Object.entries(headers || {})) lower[String(k).toLowerCase()] = v;
  for (const name of RESPONSE_HEADER_ALLOWLIST) {
    if (lower[name] === undefined) continue;
    out["capture.header." + name] = metadataValue(lower[name], 512);
  }
  // The count covers the headers the allowlist does not name, so a reader can
  // see that a response carried more than what is signed here.
  out["capture.header-count"] = String(Object.keys(lower).length);
  return out;
}

async function captureWebUIScreenshot(event) {
  const issue = metadataValue(event.issue ?? event.issue_number ?? "unknown", 64);
  const envName = metadataValue(event.env ?? event.environment, 64);
  const targetSHA = metadataValue(event.target_sha ?? event.targetSHA, 80);
  if (!envName) throw new Error("capture-web-ui-screenshot requires env");
  if (!targetSHA) throw new Error("capture-web-ui-screenshot requires target_sha");

  const ci = await captureDevCI(envName, targetSHA);
  const captured = await capturePNG(event);
  const photoUUID = crypto.randomUUID();
  const capturedAt = new Date().toISOString();
  const stamp = capturedAt.replace(/[-:]/g, "").replace(/\.\d{3}Z$/, "Z");
  const key = envName + "/issue-evidence/issue-" + safeSegment(issue) + "/" + photoUUID + "/" +
    stamp + "-" + safeSegment(envName) + "-web-ui-screenshot.png";
  // Informational labels only; the signed facts are in `observed` below.
  const metadata = {
    "environment": envName,
    "target-sha": targetSHA,
    "issue-number": issue,
    "evidence-type": "web-ui-screenshot",
    "captured-by": "command-center evidence-attestor",
    "requested-url": metadataValue(captured.requestedURL, 512),
    "final-url": metadataValue(captured.finalURL, 512),
  };
  // Everything after the shutter is still inside the function's budget, and a
  // multi-megabyte fullPage PNG crossing to S3 is not free. Without this marker
  // a timeout during the upload is indistinguishable from a timeout during the
  // capture, which is exactly the ambiguity the 2026-09-05 incident hit.
  phase("s3-put", "png-bytes=" + captured.bytes.length);
  const put = await s3.send(new PutObjectCommand({
    Bucket: CFG.bucket, Key: key, Body: captured.bytes, ContentType: "image/png", Metadata: metadata,
  }));
  const observed = addAttestationContext(Object.assign({
    "capture.evidence-type": "web-ui-screenshot",
    "capture.issue": issue,
    "capture.captured-at-utc": capturedAt,
    "capture.requested-url": captured.requestedURL,
    "capture.final-url": captured.finalURL,
    "capture.http-status": String(captured.httpStatus),
    "capture.viewport": captured.viewport,
    // Whether the image is the whole document or only the fold, and how big it
    // actually came out. Without these a reader cannot tell an absent element
    // from one that was merely below the bottom edge of the capture.
    "capture.full-page": String(!!captured.fullPage),
    "capture.image-width-px": String(captured.imageWidth),
    "capture.image-height-px": String(captured.imageHeight),
    // How many viewport-heights tall the evidence is, which is the thing a reader
    // is actually judging when they ask whether the whole page was captured.
    "capture.image-screens-tall": String(
      captured.imageHeight > 0 && captured.viewportHeight > 0
        ? (captured.imageHeight / captured.viewportHeight).toFixed(2)
        : "-1"),
    "capture.cookie-fingerprint-sha256": captured.cookieFingerprint,
    "capture.cookie-count": String(captured.cookieCount),
    "capture.blocked-url-patterns": (captured.blockedURLPatterns || []).join(",") || "none",
    // The clicks this Lambda performed on the page before the shutter, in the
    // order it performed them. Joined with "|" rather than the "," its sibling
    // uses because a comma is a legal CSS selector-list separator, so a comma
    // join would let one selector read as two. "none" renders the empty case
    // exactly as blocked-url-patterns does: a reader must be able to see that a
    // capture was UNPERTURBED, which an absent field cannot say.
    "capture.click-selectors": (captured.clickSelectors || []).join("|") || "none",
    // GH #3862: the whole ordered interaction script, pauses included, as
    // `click:<selector>|wait_ms:<n>|...`. The verb prefix ends at the FIRST ":"
    // (a selector's own pseudo-class colons come after it) and "|" is refused
    // inside step selectors at parse time, so the line reads back unambiguously.
    // "none" for an unperturbed capture, as its two siblings above.
    "capture.steps": (captured.steps || [])
      .map((s) => (s.wait_ms !== undefined ? "wait_ms:" + s.wait_ms : "click:" + s.click))
      .join("|") || "none",
    "capture.ttfb-ms": String(captured.ttfbMS),
    "capture.dom-content-loaded-ms": String(captured.domContentLoadedMS),
    "capture.load-event-ms": String(captured.loadEventMS),
    // The DOCUMENT's hash, distinct from the PNG's: two captures whose images
    // differ only by live data carry different image hashes and can still be
    // shown to have served the same markup, and a page whose defect is in its
    // markup rather than its pixels becomes evidenceable at all.
    "capture.body-sha256": captured.bodySha256,
    "capture.body-bytes": String(captured.bodyBytes),
  }, timingObserved(captured.timing || {}), headerObserved(captured.responseHeaders), ci), event);
  // The last phase, and the one nobody would guess is expensive: attest() does a
  // HeadObject, re-DOWNLOADS the object it just wrote to hash the bytes itself
  // (deliberately -- it signs only what it retrieved), fetches and unwraps the
  // SSM signing seed on a cold container, signs, and writes the sidecar. Several
  // round trips after the capture already looked done.
  phase("attest-and-sign", "key=" + key);
  return attest(key, put.VersionId || "", observed);
}

// ---------------------------------------------------------------------------
// Raw HTTP capture (evidence-type:http-raw).
//
// TWO THINGS THE SCREENSHOT PATH CANNOT DO, and this action exists for exactly
// those two:
//
// 1. THE EVIDENCE OBJECT IS THE RESPONSE BODY ITSELF. capturePNG reduces the
//    body to capture.body-sha256, and a hash is not a body: a ticket asserting
//    "the 404 page carries incident id Y" is unprovable from a hash, because a
//    reader months later cannot recover Y from it. Storing the bytes makes
//    object.sha256 the body hash AND leaves the bytes fetchable at ?versionId=,
//    so the assertion can be re-read rather than re-trusted.
//
// 2. THE REDIRECT CHAIN IS NOT COLLAPSED. page.goto follows redirects, so the
//    capture.header.location a screenshot signs is read off the FINAL response,
//    which by definition is not a redirect -- the chain is simply gone. Each hop
//    is signed here as its own line.
//
// WHY A PLAIN https.request RATHER THAN response.request().redirectChain():
// the redirectChain route would keep the whole Chromium dependency (cold start,
// 1GB of ephemeral storage) for an action that renders nothing, and puppeteer
// hands back a decoded, browser-processed body rather than the bytes as served.
// A hop-by-hop client gives both the exact served bytes and full control of the
// chain, and it is the smaller mechanism.
//
// THE FAILURE MODE THE SIGNED URL AND CACHE LINES PREVENT: a 200 that is really
// a login redirect, and a CloudFront HIT still serving pre-fix bytes, BOTH read
// as "the fix shipped" when all a reader has is a status code. The requested url
// vs the final url separates the first; x-cache and age separate the second --
// they are what distinguishes "the origin is fixed" from "the edge is holding
// old bytes". Both are mandatory lines in the signed map below.
// ---------------------------------------------------------------------------

// Bounded by construction: a redirect loop must not spin this Lambda to its
// timeout, and 10 hops is more chain than any real page has.
const RAW_MAX_REDIRECTS = 10;

// Extension chosen from the served content type so the object is openable in a
// browser tab. Unknown types get .bin rather than a guess, because a wrong
// extension on stored evidence invites a reader to misread what it is.
function rawBodyExtension(contentType) {
  const ct = String(contentType || "").toLowerCase();
  if (/^text\/html/.test(ct)) return "html";
  if (/^application\/(json|.*\+json)/.test(ct)) return "json";
  if (/^application\/xml|^text\/xml|\+xml/.test(ct)) return "xml";
  if (/^text\/plain/.test(ct)) return "txt";
  if (/^text\/css/.test(ct)) return "css";
  if (/^(application|text)\/javascript/.test(ct)) return "js";
  return "bin";
}

// One hop. No redirect following, no body decoding beyond what the server sent:
// what comes back here is what crossed the wire, which is the point of the
// action. The timeout is on the socket rather than around the promise so a
// hung connection is destroyed rather than merely abandoned.
function requestOneHop(targetURL, headers, timeoutMS) {
  return new Promise((resolve, reject) => {
    const u = new URL(targetURL);
    if (u.protocol !== "https:") {
      reject(new Error("capture-http-raw refuses non-https hop " + targetURL));
      return;
    }
    const req = https.request({
      method: "GET",
      hostname: u.hostname,
      port: u.port || 443,
      path: u.pathname + u.search,
      headers,
    }, (res) => {
      const chunks = [];
      res.on("data", (c) => chunks.push(c));
      res.on("error", reject);
      res.on("end", () => resolve({
        status: res.statusCode || 0,
        headers: res.headers || {},
        body: Buffer.concat(chunks),
      }));
    });
    req.setTimeout(timeoutMS, () => req.destroy(new Error("timeout after " + timeoutMS + "ms requesting " + targetURL)));
    req.on("error", reject);
    req.end();
  });
}

async function captureHTTPRaw(event) {
  const issue = metadataValue(event.issue ?? event.issue_number ?? "unknown", 64);
  const envName = metadataValue(event.env ?? event.environment, 64);
  // target_sha is OPTIONAL here, matching attest-aws-resource rather than the
  // screenshot action: an http-raw capture is often taken against a URL that is
  // not tied to a build under test at all (a CDN edge, a third-party endpoint).
  // When it IS given, the CI snapshot rides along exactly as it does elsewhere.
  const targetSHA = metadataValue(event.target_sha ?? event.targetSHA, 80);
  const requestedURL = metadataValue(event.url, 2048);
  if (!envName) throw new Error("capture-http-raw requires env");
  if (!/^https:\/\//.test(requestedURL)) throw new Error("capture-http-raw requires an https URL");
  const timeoutMS = Math.max(1000, Math.min(30000, Number(event.timeout_ms || 15000)));
  const maxRedirects = Math.max(0, Math.min(RAW_MAX_REDIRECTS, Number(
    event.max_redirects === undefined || event.max_redirects === null ? RAW_MAX_REDIRECTS : event.max_redirects)));
  const cookies = normalizeCookies(event.cookies || event.session_cookies, requestedURL);

  // Request headers are caller-supplied minus any Cookie: cookies arrive through
  // the same `cookies` field the screenshot action uses, so the fingerprint below
  // describes them the same way and neither path publishes a raw session value.
  const requestHeaders = { "accept": "*/*", "user-agent": "r66-evidence-attestor" };
  if (event.headers && typeof event.headers === "object") {
    for (const [k, v] of Object.entries(event.headers)) {
      if (String(k).toLowerCase() === "cookie") continue;
      requestHeaders[k] = String(v);
    }
  }
  if (cookies.length) {
    requestHeaders.cookie = cookies.map((c) => c.name + "=" + c.value).join("; ");
  }

  const capturedAt = new Date().toISOString();
  const hops = [];
  let current = requestedURL;
  let last = null;
  for (let i = 0; ; i++) {
    const res = await requestOneHop(current, requestHeaders, timeoutMS);
    const location = res.headers.location === undefined ? "" : String(res.headers.location);
    const isRedirect = res.status >= 300 && res.status < 400 && !!location;
    hops.push({ url: current, status: res.status, location: isRedirect ? location : "" });
    last = { url: current, res };
    if (!isRedirect || i >= maxRedirects) break;
    // Relative Location values are legal and common; resolving against the hop's
    // own URL is what a browser does and what makes the next hop's signed url a
    // real address rather than a fragment.
    current = new URL(location, current).toString();
  }

  const finalRes = last.res;
  const contentType = metadataValue(finalRes.headers["content-type"] || "application/octet-stream", 256);
  const body = finalRes.body;
  const uuid = crypto.randomUUID();
  const stamp = capturedAt.replace(/[-:]/g, "").replace(/\.\d{3}Z$/, "Z");
  const key = envName + "/issue-evidence/issue-" + safeSegment(issue) + "/" + uuid + "/" +
    stamp + "-" + safeSegment(envName) + "-http-raw." + rawBodyExtension(contentType);
  const put = await s3.send(new PutObjectCommand({
    Bucket: CFG.bucket,
    Key: key,
    Body: body,
    // Stored under the type the origin served, so the object a reader opens is
    // interpreted the way the browser under test interpreted it.
    ContentType: contentType,
    // Informational labels only; the signed facts are in `observed` below.
    Metadata: {
      "environment": envName,
      "issue-number": issue,
      "evidence-type": "http-raw",
      "captured-by": "command-center evidence-attestor",
      "requested-url": metadataValue(requestedURL, 512),
      "final-url": metadataValue(last.url, 512),
    },
  }));

  const observed = {
    "capture.evidence-type": "http-raw",
    "capture.issue": issue,
    "capture.captured-at-utc": capturedAt,
    // Requested vs final: the pair that exposes a 200 which is really a login
    // redirect. Both are signed; neither is derivable from the other.
    "capture.requested-url": requestedURL,
    "capture.final-url": last.url,
    "capture.http-status": String(finalRes.status),
    "capture.redirect-count": String(hops.length - 1),
    // The chain was truncated rather than exhausted -- a fact, because a reader
    // must not read the last recorded hop as the final destination.
    "capture.redirect-truncated": String(hops.length - 1 >= maxRedirects && !!hops[hops.length - 1].location),
    "capture.body-bytes": String(body.length),
    // Redundant with object.sha256 by construction (the body IS the object), and
    // signed anyway so an http-raw statement can be compared field-for-field with
    // a screenshot statement of the same page.
    "capture.body-sha256": bytesSha256(body),
    "capture.request-timeout-ms": String(timeoutMS),
    "capture.cookie-fingerprint-sha256": cookieFingerprint(cookies),
    "capture.cookie-count": String(cookies.length),
  };
  // One signed line per hop, per the one-fact-per-line manifest rule: a packed
  // chain string would hide a changed hop inside an unchanged value. The index is
  // zero-padded so the manifest's lexicographic key sort is also hop order --
  // unpadded, hop 10 would sort between hop 1 and hop 2.
  hops.forEach((hop, i) => {
    const prefix = "capture.redirect." + String(i).padStart(2, "0") + ".";
    observed[prefix + "url"] = metadataValue(hop.url, 512);
    observed[prefix + "status"] = String(hop.status);
    observed[prefix + "location"] = metadataValue(hop.location, 512);
  });
  // The final response's headers through the SAME allowlist the screenshot path
  // uses -- including x-cache and age, the edge-vs-origin discriminators -- and
  // with set-cookie and authorization excluded by that same construction, which
  // matters more here because this bucket is world-readable and the stored object
  // is the page body.
  Object.assign(observed, headerObserved(finalRes.headers));
  if (targetSHA) Object.assign(observed, await captureDevCI(envName, targetSHA));
  addAttestationContext(observed, event);
  return attest(key, put.VersionId || "", observed);
}

// ---------------------------------------------------------------------------
// AWS resource attestation (owner 2026-08-26: "the attestator should be able
// to be called to check and attest the existence or particular attribute of
// an aws resource (via aws sdk readonly). In this case it needs to be passed
// the temporary credentials, and it should write the result of the AWS SDK
// Call on the target resource." e.g. "describe dynamo table").
//
// The caller passes TEMPORARY credentials (session token mandatory), a region,
// a service (the @aws-sdk/client-<service> package name, e.g. "dynamodb"),
// a READ-ONLY operation (allow-listed by prefix) and its params. This Lambda
// makes the call itself with those credentials, identifies the caller through
// STS GetCallerIdentity with the same credentials, writes the SDK result as a
// JSON evidence object, and attests it with observed.aws.* facts. A failed
// call is still a result: the error is written and captured, never refused.
// The clients come from the runtime's bundled AWS SDK v3; an unknown service
// or operation is an invocation error. Credentials are never logged or
// stored; only the identity they resolve to is recorded.
// ---------------------------------------------------------------------------
// `Filter` is on this list because FilterLogEvents is the call a CloudWatch-logs
// evidence ticket actually needs, and it is genuinely read-only: it returns log
// events and changes nothing. `Start` is deliberately NOT on the list and must
// not be added -- StartQuery, StartQueryExecution and DetectStackDrift all begin
// work in the target account, so a "Start" prefix would let a mutation through a
// gate whose entire job is to admit reads only.
const READ_ONLY_OPERATION = /^(Describe|Get|List|Head|Lookup|Query|Scan|BatchGet|Filter)[A-Za-z0-9]*$/;

function loadServiceClient(service, operation) {
  if (!/^[a-z0-9-]+$/.test(service)) throw new Error("invalid service name " + service);
  if (!READ_ONLY_OPERATION.test(operation)) {
    throw new Error("operation " + operation + " is not an allow-listed read-only operation");
  }
  let mod;
  try {
    mod = require("@aws-sdk/client-" + service);
  } catch (err) {
    throw new Error("no AWS SDK client for service " + service + ": " + (err && err.message ? err.message : err));
  }
  const Command = mod[operation + "Command"];
  if (typeof Command !== "function") throw new Error("service " + service + " has no operation " + operation);
  const clientKey = Object.keys(mod).find((k) => k !== "Client" && /Client$/.test(k) && typeof mod[k] === "function");
  if (!clientKey) throw new Error("service " + service + " exposes no Client class");
  return { Client: mod[clientKey], Command };
}

// JSON-safe copy of an SDK result: binary to base64, streams read to a cap,
// $metadata reduced to its HTTP status.
async function serializeSdkResult(result) {
  const out = {};
  for (const [k, v] of Object.entries(result || {})) {
    if (k === "$metadata") {
      out.$metadata = { httpStatusCode: v && v.httpStatusCode };
    } else if (v && typeof v.transformToByteArray === "function") {
      const bytes = await v.transformToByteArray();
      const cap = 1 << 20;
      out[k] = { $stream: true, bytes: bytes.length, truncated: bytes.length > cap,
                 body_b64: Buffer.from(bytes.subarray(0, cap)).toString("base64") };
    } else {
      out[k] = v;
    }
  }
  return JSON.parse(JSON.stringify(out, (key, val) =>
    (val instanceof Uint8Array || Buffer.isBuffer(val)) ? { $bytes_b64: Buffer.from(val).toString("base64") } : val));
}

async function attestAwsResource(event) {
  const issue = metadataValue(event.issue ?? event.issue_number ?? "unknown", 64);
  const envName = metadataValue(event.env ?? event.environment, 64);
  const targetSHA = metadataValue(event.target_sha ?? event.targetSHA, 80);
  const region = metadataValue(event.region, 32);
  const service = metadataValue(event.service, 64);
  const operation = metadataValue(event.operation, 96);
  const params = event.params && typeof event.params === "object" ? event.params : {};
  const creds = event.credentials || {};
  if (!envName) throw new Error("attest-aws-resource requires env");
  if (!region) throw new Error("attest-aws-resource requires region");
  if (!creds.accessKeyId || !creds.secretAccessKey || !creds.sessionToken) {
    throw new Error("attest-aws-resource requires TEMPORARY credentials (accessKeyId, secretAccessKey, sessionToken)");
  }
  const credentials = {
    accessKeyId: String(creds.accessKeyId),
    secretAccessKey: String(creds.secretAccessKey),
    sessionToken: String(creds.sessionToken),
  };
  const { Client, Command } = loadServiceClient(service, operation);
  const calledAt = new Date().toISOString();

  // Who is asking, proven with the very credentials the call uses.
  const { GetCallerIdentityCommand } = require("@aws-sdk/client-sts");
  const ident = await new STSClient({ region, credentials }).send(new GetCallerIdentityCommand({}));

  const doc = {
    v: 1,
    service, operation, region, params,
    caller: { arn: ident.Arn || "", account: ident.Account || "", user_id: ident.UserId || "" },
    called_at_utc: calledAt,
    result: null,
    error: null,
  };
  let httpStatus = "";
  try {
    const result = await new Client({ region, credentials }).send(new Command(params));
    doc.result = await serializeSdkResult(result);
    httpStatus = String((result && result.$metadata && result.$metadata.httpStatusCode) || "");
  } catch (err) {
    doc.error = {
      name: String(err && err.name ? err.name : "Error"),
      message: String(err && err.message ? err.message : err),
      http_status: err && err.$metadata ? String(err.$metadata.httpStatusCode || "") : "",
    };
    httpStatus = doc.error.http_status;
  }
  const paramsJSON = JSON.stringify(params);
  const body = Buffer.from(JSON.stringify(doc, null, 2), "utf8");
  const uuid = crypto.randomUUID();
  const stamp = calledAt.replace(/[-:]/g, "").replace(/\.\d{3}Z$/, "Z");
  const key = envName + "/issue-evidence/issue-" + safeSegment(issue) + "/" + uuid + "/" +
    stamp + "-" + safeSegment(envName) + "-aws-" + safeSegment(service) + "-" + safeSegment(operation) + ".json";
  const put = await s3.send(new PutObjectCommand({
    Bucket: CFG.bucket, Key: key, Body: body, ContentType: "application/json",
    Metadata: {
      "environment": envName, "issue-number": issue, "evidence-type": "aws-resource-dump",
      "captured-by": "command-center evidence-attestor", "aws-service": service, "aws-operation": operation,
    },
  }));
  const observed = {
    "aws.evidence-type": "aws-resource-dump",
    "aws.issue": issue,
    "aws.service": service,
    "aws.operation": operation,
    "aws.region": region,
    "aws.params-sha256": bytesSha256(Buffer.from(paramsJSON, "utf8")),
    "aws.caller-arn": doc.caller.arn,
    "aws.caller-account": doc.caller.account,
    "aws.called-at-utc": calledAt,
    "aws.http-status": httpStatus,
    "aws.error": doc.error ? doc.error.name + ": " + doc.error.message : "",
  };
  if (targetSHA) Object.assign(observed, await captureDevCI(envName, targetSHA));
  addAttestationContext(observed, event);
  // The compact comment links this exact versioned body instead of inlining a
  // potentially enormous SDK result; its hash remains in the signed sidecar.
  return attest(key, put.VersionId || "", observed);
}

// #3767: publication is part of attestation, not a caller's optional next step.
// GitHub credentials stay in this invocation only; never pass them to capture,
// rendering, S3 metadata, or error diagnostics. Issues and PRs share this API.
function githubTarget(event) {
  const target = event.github || {};
  const issue = Number(target.issue);
  const token = typeof target.token === "string" ? target.token.trim() : "";
  if (!Number.isSafeInteger(issue) || issue <= 0 || !token || /[\r\n]/.test(token)) {
    throw new Error("GitHub publication requires a positive issue/PR number and invocation-only token");
  }
  if (event.issue != null && Number(event.issue) !== issue) {
    throw new Error("GitHub publication target disagrees with capture issue");
  }
  const keyIssue = typeof event.key === "string" && event.key.match(/\/issue-evidence\/issue-(\d+)\//);
  if (keyIssue && Number(keyIssue[1]) !== issue) {
    throw new Error("GitHub publication target disagrees with evidence key");
  }
  return { issue, token };
}

// Fixed HTTPS origin and bounded responses keep the credential away from redirects
// and diagnostics. GitHub error bodies are deliberately not echoed to logs.
async function githubJSON(target, method, suffix, body) {
  const response = await fetch(`https://api.github.com/repos/redzilla-org/route66/issues/${target.issue}/comments${suffix}`, {
    method, redirect: "error", signal: AbortSignal.timeout(15000),
    headers: { Authorization: `Bearer ${target.token}`, Accept: "application/vnd.github+json",
      "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "route66-evidence-attestor",
      "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!response.ok) throw new Error(`GitHub attestation ${method} failed: HTTP ${response.status}`);
  // Bound bytes while reading, not after allocating an arbitrary response.
  const chunks = [];
  let size = 0;
  for await (const chunk of response.body) {
    size += chunk.length;
    if (size > 8 * 1024 * 1024) throw new Error("GitHub comment response exceeds 8 MiB");
    chunks.push(chunk);
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

// #3767 owner correction: evidence is readable plaintext, never encoded JSON.
// The clear-signed section is EXACTLY the canonical manifest bytes; only the
// 64-byte Ed25519 signature is Base64. Locator headers are outside that section,
// and the verifier binds their artifact to the signed bucket/key/version/hash.
function armoredAttestation(result, identity) {
  // GH #3933: comments carry the four facts a reviewer needs, while the exact
  // object facts and signature remain in the immutable versioned Statement.
  // The three claim.* values are inside the signed v4 observed map, so compact
  // presentation does not turn caller intent into editable unsigned prose.
  const observed = result.observed || {};
  return ["-----BEGIN ROUTE66 SIGNED ATTESTATION-----", "",
    `**${markdownLiteral(observed["claim.category"])}: ${markdownLiteral(observed["claim.intent"])}**`, "",
    `Target: ${markdownLiteral(observed["claim.target"])}`, "",
    `Attested: ${result.object.attested_at_utc}`, "",
    // Explicit autolinks preserve trailing '_' in S3 VersionIds. GitHub's bare
    // URL autolinker removed it from the actual #3767 proof, producing HTTP403.
    `Evidence: <${result.url}>`, "", `Statement: <${result.attestation_url}>`, "",
    `${result.canonicalization} · Key-ID: ${result.key_id}`, "",
    // Retry identity is machine state, not ticket content. Hide it while keeping
    // bounded comment discovery deterministic and independently checkable.
    `<!-- r66-attestation-identity: ${identity} -->`, "",
    "-----END ROUTE66 SIGNED ATTESTATION-----"].join("\n");
}

// GFM inline-active characters in signed text (S3 keys carry '_', CI error text
// can carry '<' or '|') are backslash-escaped so they render literally.
function markdownLiteral(line) {
  return line.replace(/[\\`*_\[\]<>&|~]/g, (c) => "\\" + c);
}

// A retry of the same immutable object version/target reuses its already posted
// receipt. No new database is needed. The bounded scan fails rather than guessing
// if an unusually long issue exceeds the supported comment window.
async function postAttestation(target, result) {
  // Presentation is part of retry identity: earlier encoded comments remain
  // verifiable history, but a new invocation must publish the owner's readable
  // format rather than silently returning the superseded encoded presentation.
  // compact-v1 is a new receipt identity because old cleartext comments remain
  // immutable history and must never be mistaken for the #3933 presentation.
  const identity = bytesSha256(Buffer.from(JSON.stringify(["compact-v1", result.object.bucket, result.object.key,
    result.object.version_id, result.observed["ci.env"] || "", result.observed["ci.target-sha"] || "",
    result.observed["claim.category"], result.observed["claim.intent"], result.observed["claim.target"]]), "utf8"));
  for (let page = 1; page <= 10; page++) {
    const comments = await githubJSON(target, "GET", `?per_page=100&page=${page}`);
    if (!Array.isArray(comments)) throw new Error("GitHub comment listing is malformed");
    for (const comment of comments) {
      if (typeof comment.body !== "string" ||
          !comment.body.includes(`\n<!-- r66-attestation-identity: ${identity} -->\n`)) continue;
      // A comment is editable: verify its original statement before accepting it
      // as a retry receipt, and return its original CI observation unchanged.
      // The existing versioned sidecar is the structured receipt; no second
      // encoded JSON copy belongs in the human-facing comment. Constrain its
      // locator to this exact artifact's sidecar before making the S3 read.
      const match = comment.body.match(/^Statement: <(https:\/\/[^>\s]+)>$/m);
      if (!match) throw new Error("Existing attestation comment has no sidecar locator");
      const version = new URL(match[1]).searchParams.get("versionId");
      if (!version || match[1] !== publicURL(CFG.bucket, result.attestation_key, version)) {
        throw new Error("Existing attestation comment has an invalid sidecar locator");
      }
      const saved = await s3.send(new GetObjectCommand({ Bucket: CFG.bucket,
        Key: result.attestation_key, VersionId: version }));
      if (!saved.ContentLength || saved.ContentLength > 65536) throw new Error("Attestation sidecar size is invalid");
      const prior = JSON.parse(await saved.Body.transformToString());
      prior.url = publicURL(CFG.bucket, prior.object.key, prior.object.version_id);
      prior.attestation_key = result.attestation_key;
      prior.attestation_url = match[1];
      const manifest = canonicalManifest(prior.object, prior.observed);
      const publicKey = crypto.createPublicKey({ key: Buffer.concat([
        Buffer.from("302a300506032b6570032100", "hex"), Buffer.from(PUBLIC_KEY.public_key_b64, "base64")]), format: "der", type: "spki" });
      if (prior.manifest !== manifest || prior.key_id !== PUBLIC_KEY.key_id ||
          !crypto.verify(null, Buffer.from(manifest, "utf8"), publicKey, Buffer.from(prior.signature_b64, "base64")) ||
          prior.object.bucket !== result.object.bucket || prior.object.key !== result.object.key ||
          prior.object.version_id !== result.object.version_id ||
          prior.url !== result.url ||
          prior.observed["ci.env"] !== result.observed["ci.env"] ||
          prior.observed["ci.target-sha"] !== result.observed["ci.target-sha"] ||
          armoredAttestation(prior, identity) !== comment.body) {
        throw new Error("Existing attestation comment failed signed receipt validation");
      }
      if (!comment.html_url) throw new Error("Existing attestation comment has no publication URL");
      phase("github-publication", "issue=" + target.issue + " reused=true");
      return { ...prior, evidence_text: comment.body, comment_url: comment.html_url, github_posted: true };
    }
    if (comments.length < 100) break;
    if (page === 10) throw new Error("GitHub attestation deduplication exceeds 1000 comments");
  }
  const block = armoredAttestation(result, identity);
  if (block.length > 60000) throw new Error("Armored attestation exceeds GitHub comment size budget");
  const posted = await githubJSON(target, "POST", "", { body: block });
  if (posted.body !== block || !posted.html_url) throw new Error("GitHub did not confirm the identical attestation comment");
  phase("github-publication", "issue=" + target.issue + " reused=false");
  return { ...result, evidence_text: block, comment_url: posted.html_url, github_posted: true };
}

// Capture dispatch receives no GitHub credential. Every successful path joins the
// same mandatory publication step before the handler returns any signed facts.
async function captureAttestation(event) {
  // Restart the phase clock on every invocation: a warm container reuses this
  // module, so a module-init-only timestamp would make the second invocation's
  // markers read as minutes of elapsed time. Set here rather than in capturePNG
  // so the chromium cold start is inside the measured window (see phase()).
  handlerStartedAtMS = Date.now();
  if (event && event.action === "capture-web-ui-screenshot") {
    return captureWebUIScreenshot(event);
  }
  if (event && event.action === "attest-aws-resource") {
    return attestAwsResource(event);
  }
  // The two actions below join the same flat dispatch rather than a routing
  // table: four branches read as four branches, and every one of them ends in
  // attest(), which is the only place a signature is produced. attest-code-at-sha
  // was DELETED 2026-09-03 by owner order (see the file header): evidence-type:code
  // is produced locally by `cloud-compose attest code`.
  if (event && event.action === "attest-ci-verdict") {
    return attestCIVerdict(event);
  }
  if (event && event.action === "capture-http-raw") {
    return captureHTTPRaw(event);
  }
  return attestExistingObject(event || {});
}

// ---------------------------------------------------------------------------
// OCR image reads.
//
// WHY THIS LIVES IN THE SAME HANDLER. Screenshot capture and attestation already
// own Chromium, S3 publication, and the immutable image version. Sending those
// pixels through a separately provisioned host daemon duplicated lifecycle and
// made local and Lambda execution different products. `ocr-image` accepts the
// bytes directly or retrieves an exact S3 object version, then delegates only
// the CPU-heavy Tesseract call to a private resident child. NATS, host downloads,
// filesystem paths, and public daemon ports are deliberately absent.
//
// WHY A FIXED CHILD POOL. Each child owns one warm TessApi and processes one
// request at a time. The pool width is therefore the sole in-process OCR lane
// count. Lambda normally invokes one request per execution environment, while
// kumo may drive several concurrent Runtime API loops in the same container;
// both routes use this identical pool and cannot oversubscribe it accidentally.
// ---------------------------------------------------------------------------
const OCR_WORKER = process.env.SNAPBOT_OCR_WORKER || "/opt/snapbot/snapbot-ocr-worker";
const OCR_LANES = (() => {
  const available = typeof os.availableParallelism === "function" ? os.availableParallelism() : os.cpus().length;
  const requested = Number(process.env.SNAPBOT_OCR_LANES || available);
  if (!Number.isSafeInteger(requested) || requested < 1 || requested > available) {
    throw new Error(`SNAPBOT_OCR_LANES must be an integer in [1, ${available}], got ${process.env.SNAPBOT_OCR_LANES}`);
  }
  return requested;
})();

class OCRWorker {
  constructor(id) {
    this.id = id;
    this.pending = null;
    this.failure = null;
    this.child = spawn(OCR_WORKER, ["--stdio"], { stdio: ["pipe", "pipe", "inherit"] });
    this.lines = readline.createInterface({ input: this.child.stdout, crlfDelay: Infinity });
    this.lines.on("line", (line) => {
      const pending = this.pending;
      this.pending = null;
      if (!pending) throw new Error(`snapbot OCR worker ${id} emitted an unsolicited reply`);
      try { pending.resolve(JSON.parse(line)); } catch (error) { pending.reject(new Error(`snapbot OCR worker ${id} malformed reply: ${error.message}`)); }
    });
    this.child.once("error", (error) => this.fail(error));
    this.child.once("exit", (code, signal) => this.fail(new Error(`exited code=${code} signal=${signal}`)));
  }

  fail(error) {
    // WHY: a child can die while idle. Remember that terminal state so the
    // next invocation fails explicitly instead of writing to a dead pipe and
    // potentially waiting until the Lambda timeout.
    this.failure = new Error(`snapbot OCR worker ${this.id}: ${error.message}`);
    if (!this.pending) return;
    const pending = this.pending;
    this.pending = null;
    pending.reject(this.failure);
  }

  read(request) {
    if (this.failure) return Promise.reject(this.failure);
    if (this.pending) return Promise.reject(new Error(`snapbot OCR worker ${this.id} received concurrent work`));
    return new Promise((resolve, reject) => {
      this.pending = { resolve, reject };
      this.child.stdin.write(JSON.stringify(request) + "\n", (error) => {
        if (error) this.fail(error);
      });
    });
  }
}

const ocrPool = [];
const ocrWaiters = [];

function acquireOCRWorker() {
  if (ocrPool.length < OCR_LANES) {
    // Reserve the lane before returning it so simultaneous cold invokes cannot
    // all observe length zero and create an unbounded number of engines.
    const worker = new OCRWorker(ocrPool.length);
    ocrPool.push(worker);
    return Promise.resolve(worker);
  }
  const idle = ocrPool.find((worker) => !worker.pending);
  if (idle) return Promise.resolve(idle);
  return new Promise((resolve) => ocrWaiters.push(resolve));
}

function releaseOCRWorker(worker) {
  const waiter = ocrWaiters.shift();
  if (waiter) waiter(worker);
}

async function ocrImage(event) {
  let image = typeof event.image === "string" ? event.image : "";
  let source = "inline";
  if (!image && event.s3 && event.s3.bucket && event.s3.key) {
    const object = await s3.send(new GetObjectCommand({
      Bucket: String(event.s3.bucket), Key: String(event.s3.key),
      VersionId: event.s3.version_id ? String(event.s3.version_id) : undefined,
    }));
    image = Buffer.from(await object.Body.transformToByteArray()).toString("base64");
    source = "s3";
  }
  if (!image) throw new Error("ocr-image requires image base64 or s3.bucket + s3.key");

  const worker = await acquireOCRWorker();
  const started = Date.now();
  try {
    const result = await worker.read({ image, psm: event.psm, lang: event.lang,
      dpi: event.dpi, upscale: event.upscale, pixel_budget: event.pixel_budget });
    if (!result || typeof result.text !== "string" || typeof result.error !== "string") {
      throw new Error("snapbot OCR worker reply lacks text/error strings");
    }
    return { text: result.text, error: result.error, metadata: {
      source, psm: Number(event.psm || 3), lang: String(event.lang || "eng"),
      dpi: Number(event.dpi || 300), upscale: Number(event.upscale || 1),
      pixel_budget: Number(event.pixel_budget || 20000000), elapsed_ms: Date.now() - started,
      worker_version: "snapbot-ocr-worker/1",
    } };
  } finally {
    releaseOCRWorker(worker);
  }
}

exports.handler = async (event) => {
  // OCR produces a fact about pixels but no signed evidence object. It therefore
  // needs no GitHub publication credential. Ticket evidence continues to join
  // mandatory publication before returning; the narrowly validated run-manifest
  // branch below is evidence infrastructure rather than a ticket artifact.
  if (event && event.action === "ocr-image") return ocrImage(event);

  // WHY THIS IS THE ONLY NON-GITHUB ATTESTATION PATH (GH #3822): cloud-compose
  // must publish one run manifest automatically for every local and CI harness run,
  // before any ticket-specific progress comment exists. Requiring an issue token
  // here would either make unattended CI unable to sign its own manifest or spray
  // every routine run onto one unrelated issue. Keep the exemption narrower than
  // the generic existing-object action: the caller must name an immutable S3
  // VersionId and the object must be the run-prefix manifest itself. The attestor
  // still GETs those exact bytes and captures the requested CI observation through
  // attestExistingObject; this branch relaxes publication, never provenance.
  if (event && event.action === "attest-run-manifest") {
    const key = typeof event.key === "string" ? event.key : "";
    const versionID = typeof event.version_id === "string" ? event.version_id.trim() : "";
    requireRunManifestKey(key);
    if (!versionID) {
      throw new Error("attest-run-manifest requires the manifest object's VersionId");
    }
    return attestExistingObject(event);
  }
  const target = githubTarget(event || {});
  const attestationContext = requiredAttestationContext(event || {});
  const { github, ...capture } = event;
  // Private transport key: capture actions receive the already-normalized
  // signed claim without having to repeat validation or trust raw event fields.
  capture._attestation_context = attestationContext;
  const result = await captureAttestation(capture);
  return postAttestation(target, result);
};
