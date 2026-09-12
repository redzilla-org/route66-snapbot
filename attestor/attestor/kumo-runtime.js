"use strict";

// Kumo exposes the standard Lambda Runtime API to externally attached clients.
// The managed AWS Node runtime polls once per execution environment, which is
// correct in Lambda but would leave an 80-CPU local host with one OCR lane. This
// local-only entrypoint runs one polling loop per declared lane against the same
// handler and worker pool. Production keeps the managed image entrypoint; only
// cloud-compose selects this file when attaching the image to kumo.

const { handler } = require("./index.js");

const runtime = process.env.AWS_LAMBDA_RUNTIME_API;
if (!runtime || /^(https?:\/\/)|[\r\n]/.test(runtime)) {
  throw new Error("AWS_LAMBDA_RUNTIME_API must be a host/path without scheme or newlines");
}
const lanes = Number(process.env.SNAPBOT_OCR_LANES);
if (!Number.isSafeInteger(lanes) || lanes < 1) {
  throw new Error("SNAPBOT_OCR_LANES must be a positive integer for kumo attachment");
}

const base = `http://${runtime}/2018-06-01/runtime/invocation`;

async function post(requestID, suffix, body) {
  const response = await fetch(`${base}/${requestID}/${suffix}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!response.ok) throw new Error(`Runtime API ${suffix} returned HTTP ${response.status}`);
}

async function poll(lane) {
  for (;;) {
    const next = await fetch(`${base}/next`);
    if (!next.ok) throw new Error(`Runtime API next returned HTTP ${next.status} on lane ${lane}`);
    const requestID = next.headers.get("lambda-runtime-aws-request-id");
    if (!requestID) throw new Error(`Runtime API next omitted request id on lane ${lane}`);
    try {
      const result = await handler(await next.json(), {});
      await post(requestID, "response", result);
    } catch (error) {
      await post(requestID, "error", {
        errorMessage: error && error.message ? error.message : String(error),
        errorType: error && error.name ? error.name : "Error",
        stackTrace: error && error.stack ? String(error.stack).split("\n") : [],
      });
    }
  }
}

Promise.all(Array.from({ length: lanes }, (_, lane) => poll(lane))).catch((error) => {
  // A dead Runtime API loop silently shrinks the OCR pool. Terminate the whole
  // attachment so cloud-compose observes the child failure and fails the gate.
  console.error(`snapbot kumo runtime fatal: ${error.stack || error}`);
  process.exit(1);
});
