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

// WHY A TRANSPORT RETRY: the /next long-poll is IDLE by design -- a lane waits
// minutes between invocations, and any peer (kumo, a proxy, the OS) may close an
// idle connection. undici surfaces that as a thrown "TypeError: fetch failed",
// which is a NORMAL condition for an idle poller, NOT a handler fault. route66
// local-verify run306 died exactly here: 40 lanes sat ~7m30s through the
// preamble's rendezvous cold builds, the connection dropped, and the rejection
// killed the whole attachment before a single regression screenshot arrived, so
// all 293 later OCR invocations returned 502 "no runtime handler available".
// Reconnecting is the only correct response; the backoff keeps a genuinely
// unreachable Runtime API from spinning.
const pollRetryBaseMs = 100;
const pollRetryMaxMs = 5000;

async function pollNext(lane) {
  let delay = pollRetryBaseMs;
  for (;;) {
    try {
      return await fetch(`${base}/next`);
    } catch (error) {
      // Transport-level only: a fetch that RESOLVED is handled by the caller.
      console.error(
        `snapbot kumo runtime: lane ${lane} reconnecting to the Runtime API after a transport error in ${delay}ms: ${error && error.message ? error.message : error}`,
      );
      await new Promise((resolve) => setTimeout(resolve, delay));
      delay = Math.min(delay * 2, pollRetryMaxMs);
    }
  }
}

async function poll(lane) {
  for (;;) {
    const next = await pollNext(lane);
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

// LANE FAILURE IS ISOLATED. Promise.all rejects on the FIRST lane to fail and
// took the whole process down with it (run306), so one lane's fault destroyed
// the other 39 healthy pollers. Each lane is settled independently and reported
// by number; the attachment dies only when the pool is empty, which is the
// condition that actually makes the OCR tier useless and must reach
// cloud-compose as a child failure.
let liveLanes = lanes;
Promise.allSettled(
  Array.from({ length: lanes }, (_, lane) =>
    poll(lane).catch((error) => {
      liveLanes -= 1;
      console.error(
        `snapbot kumo runtime: lane ${lane} exited (${liveLanes}/${lanes} lanes remain): ${error && error.stack ? error.stack : error}`,
      );
    }),
  ),
).then(() => {
  console.error("snapbot kumo runtime fatal: every OCR lane exited; no poller remains for this function");
  process.exit(1);
});
