"use strict";

// browse-bench.js -- end-to-end proof of the browse pool, run inside
// Dockerfile.test's browse-test stage.
//
// WHY A FAKE RUNTIME API: the pool's only external contract is the Lambda Runtime
// API (Kumo locally, the managed runtime in AWS). Serving that contract from this
// script exercises the real dispatcher, the real 40 lane processes, the real
// Chromium and the real browse steps, with nothing mocked inside snapbot itself.
//
// WHAT IT PROVES AND PRINTS:
//   * all 40 lanes come up warm, with startup wall time and RSS per process;
//   * browse requests at 40-way concurrency all succeed, screenshots land in the
//     local store with the returned sha256, and p50/p95 latency per request
//     (compare the host's old ~2.5s NewPage, route66 .claude/rules/tests.md).
// Any failure exits non-zero, failing the image build.

const crypto = require("crypto");
const fs = require("fs");
const http = require("http");
const path = require("path");
const { spawn } = require("child_process");

const TASK = process.env.SNAPBOT_BENCH_TASK_DIR || "/var/task";
const POOL = 40;
const ROUNDS = 2;
const SITE_PORT = 18080;
const RUNTIME_PORT = 19001;
const LOCAL_ROOT = "/tmp/snapbench";
// Exercises the route66 local shape: a brand hostname resolved by
// --host-resolver-rules to loopback.
const BROWSER_ARGS = ["--host-resolver-rules=MAP bench.example 127.0.0.1"];

function fail(msg) {
  console.error(`browse-bench FAIL: ${msg}`);
  process.exit(1);
}

function pct(values, p) {
  const s = values.slice().sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))];
}

// A page with enough layout and a deferred mutation that the wait step is real.
function page(i) {
  const rows = Array.from({ length: 120 }, (_, r) => `<tr><td>row ${r}</td><td>${(r * 7919 + i) % 1000}</td></tr>`).join("");
  return `<!doctype html><html><head><title>bench ${i}</title></head><body id="bench">
<h1>Listing ${i}</h1><div class="result-card">a</div><div class="result-card">b</div>
<table>${rows}</table>
<script>setTimeout(() => { document.body.dataset.ready = "1"; }, 50);</script></body></html>`;
}

function startSite() {
  return new Promise((resolve) => {
    const server = http.createServer((req, res) => {
      const i = Number((/\/p\/(\d+)/.exec(req.url) || [0, 0])[1]);
      res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      res.end(page(i));
    });
    server.listen(SITE_PORT, "127.0.0.1", () => resolve(server));
  });
}

// Minimal Lambda Runtime API: /next long-polls, /response and /error settle.
function startRuntime() {
  const waiting = [];
  const queued = [];
  const pending = new Map();
  let seq = 0;
  const server = http.createServer((req, res) => {
    const m = /\/2018-06-01\/runtime\/invocation\/(next|([^/]+)\/(response|error))$/.exec(req.url);
    if (!m) { res.writeHead(404); return void res.end(); }
    if (m[1] === "next") {
      const job = queued.shift();
      if (job) return void deliver(res, job);
      return void waiting.push(res);
    }
    let body = "";
    req.on("data", (c) => { body += c; });
    req.on("end", () => {
      const settle = pending.get(m[2]);
      pending.delete(m[2]);
      res.writeHead(202);
      res.end();
      if (settle) settle({ kind: m[3], payload: JSON.parse(body) });
    });
  });
  function deliver(res, job) {
    res.writeHead(200, { "content-type": "application/json", "lambda-runtime-aws-request-id": job.id });
    res.end(JSON.stringify(job.payload));
  }
  function invoke(payload) {
    return new Promise((resolve) => {
      const job = { id: `req-${++seq}`, payload };
      pending.set(job.id, resolve);
      const res = waiting.shift();
      if (res) deliver(res, job); else queued.push(job);
    });
  }
  return new Promise((resolve) => server.listen(RUNTIME_PORT, "127.0.0.1", () => resolve({ server, invoke })));
}

function poolEnv(extra) {
  return Object.assign({}, process.env, {
    AWS_LAMBDA_RUNTIME_API: `127.0.0.1:${RUNTIME_PORT}/_runtime/bench`,
    NODE_PATH: "/var/runtime/node_modules",
    SNAPBOT_STORE: "local",
    SNAPBOT_LOCAL_ROOT: LOCAL_ROOT,
    SNAPBOT_BROWSER_ARGS: JSON.stringify(BROWSER_ARGS),
    // index.js reads these at module load; browse touches none of them.
    EVIDENCE_ATTESTOR_REGION: "us-west-2",
    EVIDENCE_ATTESTOR_BUCKET: "bench-unused",
    EVIDENCE_ATTESTOR_SSM_PARAM: "/bench/unused",
    EVIDENCE_ATTESTOR_CI_READ_ROLE_NAME: "bench-unused",
    EVIDENCE_ATTESTOR_DEV_CI_JSON: "{}",
    AWS_REGION: "us-west-2",
  }, extra);
}

function request(i) {
  return {
    action: "browse",
    browser_args: BROWSER_ARGS,
    local_dir: "run",
    context: { viewport: { width: 1366, height: 900 }, default_timeout_ms: 4000, navigation_timeout_ms: 4000 },
    steps: [
      { id: "goto", op: "goto", url: `http://bench.example:${SITE_PORT}/p/${i}`, timeout_ms: 4000, body: true },
      { id: "steady", op: "start_clock", name: "page", ms: 4000 },
      { id: "ready", op: "wait_for_function", fn: "() => document.body.dataset.ready === '1'", clock: "page" },
      { id: "cards", op: "count", selector: ".result-card", expect: { gte: 2 } },
      { id: "body_id", op: "evaluate", fn: "() => document.body.id", expect: { equals: "bench" } },
      { id: "text", op: "text" },
      { id: "shot", op: "screenshot", name: `bench-${i}.png`, full_page: true },
      { id: "html", op: "content" },
    ],
  };
}

function verify(resp, i) {
  if (!resp || resp.kind !== "response") fail(`request ${i} errored: ${JSON.stringify(resp && resp.payload)}`);
  const r = resp.payload;
  if (!r.ok) fail(`request ${i} not ok: ${JSON.stringify(r.steps)}`);
  const shot = r.steps.find((s) => s.id === "shot").value;
  const bytes = fs.readFileSync(path.join(LOCAL_ROOT, shot.local_path));
  if (crypto.createHash("sha256").update(bytes).digest("hex") !== shot.sha256) fail(`request ${i} screenshot sha256 mismatch`);
  if (r.steps.find((s) => s.id === "goto").value.status !== 200) fail(`request ${i} goto status`);
  if (!r.steps.find((s) => s.id === "html").value.includes(`Listing ${i}`)) fail(`request ${i} html`);
  if (r.worker.browser_launches !== 1) fail(`request ${i} lane relaunched its browser (${r.worker.browser_launches} launches)`);
}

async function main() {
  fs.mkdirSync(LOCAL_ROOT, { recursive: true });
  const dispatcher = path.join(TASK, "kumo-runtime.js");

  const site = await startSite();
  const runtime = await startRuntime();
  const started = Date.now();
  const pool = spawn(process.execPath, [dispatcher], { env: poolEnv({ SNAPBOT_POOL_PROCESSES: String(POOL) }), stdio: ["ignore", "pipe", "inherit"] });
  const readyLine = await new Promise((resolve, reject) => {
    let buf = "";
    pool.stdout.on("data", (d) => {
      buf += d;
      process.stdout.write(d);
      const m = /snapbot pool ready:[^\n]*/.exec(buf);
      if (m) resolve(m[0]);
    });
    pool.once("exit", (code) => reject(new Error(`pool exited ${code} before ready`)));
  }).catch((e) => fail(e.message));
  const startupMS = Date.now() - started;

  // 40-way concurrency, ROUNDS rounds, every lane busy at once.
  const concurrent = [];
  let n = 0;
  for (let round = 0; round < ROUNDS; round++) {
    await Promise.all(Array.from({ length: POOL }, async () => {
      const i = n++;
      const t0 = Date.now();
      const resp = await runtime.invoke(request(i));
      concurrent.push(Date.now() - t0);
      verify(resp, i);
    }));
  }

  console.log(`browse-bench: pool startup ${startupMS}ms for ${POOL} processes; ${readyLine}`);
  console.log(`browse-bench: ${concurrent.length} requests at ${POOL}-way concurrency p50=${pct(concurrent, 50)}ms p95=${pct(concurrent, 95)}ms max=${Math.max(...concurrent)}ms`);
  console.log("browse-bench: OK");
  pool.kill("SIGTERM");
  site.close();
  runtime.server.close();
  process.exit(0);
}

main().catch((e) => fail(e && e.stack ? e.stack : String(e)));
