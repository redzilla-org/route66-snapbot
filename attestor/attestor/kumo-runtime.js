"use strict";

// kumo-runtime.js -- the local pool dispatcher for Kumo attachment.
//
// Kumo exposes the standard Lambda Runtime API to externally attached clients and
// hands each invocation to whichever poller is waiting on /next. This entrypoint
// therefore needs no scheduler of its own: it forks SNAPBOT_POOL_PROCESSES lane
// processes (kumo-lane.js), each one warm browser plus one poller, and Kumo is the
// round-robin. Production keeps the managed Lambda entrypoint (index.handler);
// only cloud-compose selects this file when attaching the image to Kumo.
//
// WHY PROCESSES, NOT LOOPS IN ONE PROCESS: owner directive 2026-09-26, verbatim,
// "snapbot's lambda pool should be at least 40 processes". A process per lane
// gives each lane its own browser and its own event loop, the Lambda
// one-invocation-per-environment contract, and fault isolation per browser.

const { fork } = require("child_process");
const fs = require("fs");
const path = require("path");

// Owner floor, verbatim 2026-09-26: "snapbot's lambda pool should be at least 40
// processes". Fewer is refused at startup rather than degraded.
const SNAPBOT_POOL_MIN_PROCESSES = 40;

// How long the whole pool may take to warm. A lane that cannot launch Chromium in
// this long is broken, and the pool must not report ready without it.
const POOL_STARTUP_TIMEOUT_MS = 180000;

const runtime = process.env.AWS_LAMBDA_RUNTIME_API;
if (!runtime || /^(https?:\/\/)|[\r\n]/.test(runtime)) {
  throw new Error("AWS_LAMBDA_RUNTIME_API must be a host/path without scheme or newlines");
}
const processes = Number(process.env.SNAPBOT_POOL_PROCESSES || SNAPBOT_POOL_MIN_PROCESSES);
if (!Number.isSafeInteger(processes) || processes < SNAPBOT_POOL_MIN_PROCESSES) {
  throw new Error(`SNAPBOT_POOL_PROCESSES must be an integer >= ${SNAPBOT_POOL_MIN_PROCESSES} (owner floor); got ${process.env.SNAPBOT_POOL_PROCESSES}`);
}

// Sum VmRSS over a process and all of its descendants: a lane's real footprint is
// node plus its Chromium tree, and the owner asked for RSS per process measured.
function treeRSSKiB(rootPid) {
  const parent = new Map();
  const rss = new Map();
  for (const entry of fs.readdirSync("/proc")) {
    if (!/^\d+$/.test(entry)) continue;
    try {
      const status = fs.readFileSync(path.join("/proc", entry, "status"), "utf8");
      const ppid = Number((/^PPid:\s+(\d+)/m.exec(status) || [])[1]);
      const kib = Number((/^VmRSS:\s+(\d+)/m.exec(status) || [0, 0])[1]);
      parent.set(Number(entry), ppid);
      rss.set(Number(entry), kib);
    } catch (_) {
      // A process that exited between readdir and read has no footprint to count.
    }
  }
  let total = 0;
  for (const [pid, kib] of rss) {
    for (let p = pid; p; p = parent.get(p)) {
      if (p === rootPid) { total += kib; break; }
    }
  }
  return total;
}

async function main() {
  const started = Date.now();
  // WHY: @sparticuz/chromium unpacks its Amazon Linux 2023 shared libraries
  // (libnspr4 and friends) only when it recognises a Lambda nodejs runtime, which
  // the managed runtime announces and this local entrypoint does not. The image IS
  // the nodejs22 Lambda base, so declare it; without it every lane dies with
  // "libnspr4.so: cannot open shared object file".
  process.env.AWS_LAMBDA_JS_RUNTIME = process.env.AWS_LAMBDA_JS_RUNTIME || "nodejs22.x";
  // Extract the bundled Chromium ONCE before forking: @sparticuz/chromium
  // decompresses into /tmp on first executablePath(), and 40 lanes racing that
  // extraction would read a half-written binary.
  const chromium = (await import("@sparticuz/chromium")).default;
  await chromium.executablePath();
  const extractMS = Date.now() - started;

  const lanes = [];
  const ready = [];
  for (let i = 0; i < processes; i++) {
    const child = fork(path.join(__dirname, "kumo-lane.js"), [], {
      env: Object.assign({}, process.env, {
        SNAPBOT_LANE: String(i),
        // One invocation at a time per lane, so one OCR engine per lane.
        SNAPBOT_OCR_LANES: "1",
      }),
    });
    lanes.push(child);
    ready.push(new Promise((resolve, reject) => {
      child.once("message", (m) => (m && m.ready ? resolve(m) : reject(new Error(`lane ${i} sent ${JSON.stringify(m)}`))));
      child.once("exit", (code, signal) => reject(new Error(`lane ${i} exited before ready (code=${code} signal=${signal})`)));
    }));
  }
  // ANY lane exit after startup is fatal: the pool is either the owner's full
  // width or it is down, and a restart policy (cloud-compose runs the container
  // with one) brings the whole pool back warm.
  const timeout = new Promise((_, reject) => setTimeout(() => reject(new Error(`pool not ready within ${POOL_STARTUP_TIMEOUT_MS}ms`)), POOL_STARTUP_TIMEOUT_MS).unref());
  const up = await Promise.race([Promise.all(ready), timeout]);
  for (const [i, child] of lanes.entries()) {
    child.removeAllListeners("exit");
    child.on("exit", (code, signal) => {
      console.error(`snapbot pool fatal: lane ${i} exited (code=${code} signal=${signal}); the pool is below its ${processes}-process width`);
      process.exit(1);
    });
  }
  const rss = up.map((m) => treeRSSKiB(m.pid));
  const sorted = rss.slice().sort((a, b) => a - b);
  const warm = up.map((m) => m.warm_ms).sort((a, b) => a - b);
  console.log(
    `snapbot pool ready: ${up.length}/${processes} processes in ${Date.now() - started}ms ` +
    `(chromium extract ${extractMS}ms, lane warm p50 ${warm[Math.floor(warm.length / 2)]}ms max ${warm[warm.length - 1]}ms) ` +
    `rss/process KiB p50 ${sorted[Math.floor(sorted.length / 2)]} max ${sorted[sorted.length - 1]} total ${rss.reduce((a, b) => a + b, 0)}`,
  );
}

main().catch((error) => {
  console.error(`snapbot pool fatal: ${error && error.stack ? error.stack : error}`);
  process.exit(1);
});
