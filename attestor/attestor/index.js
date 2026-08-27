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
//     execution's sha, status, finalizer mode/exitCode, and the derived
//     sha_match / true_green booleans. The attestor NEVER refuses on CI state
//     (owner 2026-08-26: "never refuse, only capture!"); a RUNNING execution,
//     a newer sha, a red run or an unreachable account are all captured as
//     fields and signed as observed;
//   * for screenshots it captured itself: requested/final URL, HTTP status,
//     viewport, cookie fingerprint/count.
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

const CANONICALIZATION = "r66-evidence-attestation-v3";

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
// Canonical manifest v3. One line per observed fact, keys sorted within the
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
    ci["ci.sha-match"] = String(executedSHA === targetSHA);
    let finalizer = null;
    try {
      finalizer = await readFinalizerInput(client, ex.executionArn);
    } catch (err) {
      ci["ci.error"] = "history: " + String(err && err.message ? err.message : err);
    }
    ci["ci.finalizer-mode"] = finalizer ? finalizer.mode : "";
    ci["ci.worker-exit-code"] = finalizer && Number.isFinite(finalizer.exitCode) ? String(finalizer.exitCode) : "";
    ci["ci.true-green"] = String(
      executedSHA === targetSHA &&
      described.status === "SUCCEEDED" &&
      !!finalizer && finalizer.mode === "success" && finalizer.exitCode === 0
    );
  } catch (err) {
    ci["ci.error"] = String(err && err.message ? err.message : err);
    ci["ci.true-green"] = "false";
  }
  return ci;
}

// ---------------------------------------------------------------------------
// Attest one object version: fetch, hash, (optionally) capture CI, sign, and
// write the sidecar. `observed` carries any facts this Lambda itself produced
// upstream (a screenshot capture); callers cannot inject into it because the
// handler only ever passes its own values.
// ---------------------------------------------------------------------------
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
    v: 3,
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
  return Object.assign({}, statement, {
    url: publicURL(CFG.bucket, key, vid),
    attestation_key: sidecarKey,
    attestation_url: publicURL(CFG.bucket, sidecarKey, put.VersionId || ""),
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
  return attest(event.key, event.version_id ? String(event.version_id) : "", observed);
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

async function capturePNG(event) {
  const url = metadataValue(event.url, 2048);
  if (!/^https:\/\//.test(url)) throw new Error("capture-web-ui-screenshot requires an https URL");
  const width = Math.max(320, Math.min(3840, Number(event.viewport_width || event.width || 1365)));
  const height = Math.max(320, Math.min(3000, Number(event.viewport_height || event.height || 900)));
  const timeoutMS = Math.max(5000, Math.min(45000, Number(event.timeout_ms || 20000)));
  const waitMS = Math.max(0, Math.min(10000, Number(event.wait_ms || 1000)));
  const cookies = normalizeCookies(event.cookies || event.session_cookies, url);
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
    const response = await page.goto(url, { waitUntil: "networkidle2", timeout: timeoutMS });
    if (waitMS) await new Promise((resolve) => setTimeout(resolve, waitMS));
    const png = await page.screenshot({ type: "png", fullPage: !!event.full_page });
    return {
      bytes: Buffer.from(png),
      requestedURL: url,
      finalURL: page.url(),
      httpStatus: response ? response.status() : 0,
      viewport: width + "x" + height,
      cookieFingerprint: cookieFingerprint(cookies),
      cookieCount: cookies.length,
    };
  } finally {
    await browser.close();
  }
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
  const put = await s3.send(new PutObjectCommand({
    Bucket: CFG.bucket, Key: key, Body: captured.bytes, ContentType: "image/png", Metadata: metadata,
  }));
  const observed = Object.assign({
    "capture.evidence-type": "web-ui-screenshot",
    "capture.issue": issue,
    "capture.captured-at-utc": capturedAt,
    "capture.requested-url": captured.requestedURL,
    "capture.final-url": captured.finalURL,
    "capture.http-status": String(captured.httpStatus),
    "capture.viewport": captured.viewport,
    "capture.cookie-fingerprint-sha256": captured.cookieFingerprint,
    "capture.cookie-count": String(captured.cookieCount),
  }, ci);
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
const READ_ONLY_OPERATION = /^(Describe|Get|List|Head|Lookup|Query|Scan|BatchGet)[A-Za-z0-9]*$/;

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
  return attest(key, put.VersionId || "", observed);
}

exports.handler = async (event) => {
  if (event && event.action === "capture-web-ui-screenshot") {
    return captureWebUIScreenshot(event);
  }
  if (event && event.action === "attest-aws-resource") {
    return attestAwsResource(event);
  }
  return attestExistingObject(event || {});
};
