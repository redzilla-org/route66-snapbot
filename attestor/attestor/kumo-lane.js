"use strict";

// kumo-lane.js -- ONE pool process: one warm browser, one Runtime API poller.
//
// kumo-runtime.js forks SNAPBOT_POOL_PROCESSES of these. Each is a Lambda
// execution environment in miniature: it serves exactly one invocation at a time,
// which is the Lambda contract and also what makes browse.js's spare-context
// rebuild safe (no context is ever driving while the next one is built).

const browse = require("./browse.js");
const { handler } = require("./index.js");

const runtime = process.env.AWS_LAMBDA_RUNTIME_API;
if (!runtime || /^(https?:\/\/)|[\r\n]/.test(runtime)) {
  throw new Error("AWS_LAMBDA_RUNTIME_API must be a host/path without scheme or newlines");
}
const lane = process.env.SNAPBOT_LANE;
const base = `http://${runtime}/2018-06-01/runtime/invocation`;

// Initial launch arguments for the warm browser. A request carrying different
// ones relaunches once and keeps that browser for the rest of the process life.
const warmArgs = process.env.SNAPBOT_BROWSER_ARGS ? JSON.parse(process.env.SNAPBOT_BROWSER_ARGS) : [];

async function post(requestID, suffix, body) {
  const response = await fetch(`${base}/${requestID}/${suffix}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!response.ok) throw new Error(`Runtime API ${suffix} returned HTTP ${response.status}`);
}

// WHY /next IS RETRIED ON BOTH TRANSPORT ERRORS AND NON-2XX: the pool is RETAINED
// across local-verify runs while the Kumo emulator it polls is not always. An idle
// long-poll can be dropped by any peer (route66 run306), and a Kumo that restarted
// answers for a function that is not yet re-created. Both are "no work yet", never
// a handler fault; the backoff keeps a genuinely absent Runtime API from spinning.
const pollRetryBaseMs = 100;
const pollRetryMaxMs = 5000;

async function pollNext() {
  let delay = pollRetryBaseMs;
  for (;;) {
    try {
      const next = await fetch(`${base}/next`);
      if (next.ok) return next;
      console.error(`snapbot lane ${lane}: Runtime API next returned HTTP ${next.status}; retrying in ${delay}ms`);
    } catch (error) {
      console.error(`snapbot lane ${lane}: Runtime API transport error, retrying in ${delay}ms: ${error && error.message ? error.message : error}`);
    }
    await new Promise((resolve) => setTimeout(resolve, delay));
    delay = Math.min(delay * 2, pollRetryMaxMs);
  }
}

async function main() {
  const started = Date.now();
  await browse.warm(warmArgs);
  // READY is reported only once the browser AND the first spare page exist, so the
  // dispatcher's "N processes up" count means N warm browsers, not N node boots.
  process.send({ ready: true, lane, pid: process.pid, warm_ms: Date.now() - started });
  for (;;) {
    const next = await pollNext();
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

// A lane outliving its dispatcher would keep a browser and a poller with no one
// supervising the pool width, so it follows the dispatcher down.
process.on("disconnect", () => process.exit(1));

// A lane that cannot warm or whose loop dies exits non-zero; the dispatcher
// treats any lane exit as fatal for the whole pool.
main().catch((error) => {
  console.error(`snapbot lane ${lane} fatal: ${error && error.stack ? error.stack : error}`);
  process.exit(1);
});
