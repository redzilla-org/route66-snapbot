"use strict";

// ============================================================================
// browse -- ONE complete browsing request per Lambda invocation.
//
// WHY THIS EXISTS. Owner directives 2026-09-26, verbatim: "snapbot daemon should
// host the playwright browser sessions exclusively", "snapbot receives a complete
// browsing request as a lambda call" and "delegate to snapbot which can stay up as
// a permanent pool along with kumo". route66's web-regression harness used to
// launch a Chromium per brand process on the Windows host and paid 4.6s driver +
// 1.3s browser + ~2.5s per page before any check ran. Here the browser is launched
// once per process lifetime and a fresh context+page is PRE-CREATED while the
// process is idle, so a request pays only its own navigation and steps.
//
// ONE REQUEST AT A TIME PER PROCESS. Lambda semantics: one invocation per
// execution environment. Locally the pool is N separate processes (kumo-runtime.js
// forks them), each a single Runtime API poller. That shape is also what keeps the
// route66 hang hazard unreachable: "creating a context while other contexts on the
// SAME browser are driving pages hangs that browser" (route66
// .claude/rules/tests.md, GH #4040). A spare context is only ever created after the
// previous request's context is CLOSED, so no context is driving when it is built.
//
// STORAGE SWITCH. Owner 2026-09-26: "Do store in S3 for the normal lambda path" and
// "the LOCAL browse path ... Skip the S3 storage, local only for this path".
// SNAPBOT_STORE is set ONLY by the local launcher (cloud-compose's retained pool)
// to "local"; the real Lambda runs without it and stores in S3. An unknown value
// fails at module load. There is no fallback between the two.
// ============================================================================

const crypto = require("crypto");
const fs = require("fs");
const path = require("path");

// Parsed once at module load so a misconfigured pool dies at startup, not on the
// first screenshot of a run.
const STORE = (() => {
  const raw = process.env.SNAPBOT_STORE;
  if (raw === undefined || raw === "" || raw === "s3") return "s3";
  if (raw === "local") return "local";
  throw new Error(`SNAPBOT_STORE must be unset, "s3" or "local"; got ${JSON.stringify(raw)}`);
})();

// The local root is the container-side mount of the host directory the invoking
// harness writes its artifacts under. Required in local mode and nowhere else.
const LOCAL_ROOT = (() => {
  if (STORE !== "local") return null;
  const root = process.env.SNAPBOT_LOCAL_ROOT;
  if (!root || !path.isAbsolute(root)) throw new Error("SNAPBOT_STORE=local requires an absolute SNAPBOT_LOCAL_ROOT");
  if (!fs.statSync(root).isDirectory()) throw new Error(`SNAPBOT_LOCAL_ROOT ${root} is not a directory`);
  return root;
})();

// Bounds that keep one request from holding a pool process forever. A step's own
// timeout is the caller's; these only cap what a caller can ask for.
const MAX_STEPS = 200;
const MAX_STEP_TIMEOUT_MS = 120000;
const SCREENSHOT_TIMEOUT_MS = 30000;

// ---------------------------------------------------------------------------
// Process-lifetime browser state. `argsKey` names the launch arguments the live
// browser was started with; a request carrying different ones (route66 local
// runs carry --host-resolver-rules) relaunches once and keeps that browser.
// ---------------------------------------------------------------------------
const state = {
  browser: null,
  argsKey: null,
  spare: null,
  sparePromise: null,
  busy: false,
  launches: 0,
  served: 0,
};

let libPromise = null;
function browserLibs() {
  if (!libPromise) {
    libPromise = Promise.all([import("puppeteer-core"), import("@sparticuz/chromium")]).then(([p, c]) => ({
      puppeteer: p.default || p,
      chromium: c.default || c,
    }));
  }
  return libPromise;
}

// Launch-argument validation: only strings, no duplicates of what the base set
// already controls. Anything else is a caller defect.
function parseBrowserArgs(raw) {
  if (raw === undefined || raw === null) return [];
  if (!Array.isArray(raw) || raw.some((a) => typeof a !== "string" || !a.startsWith("--"))) {
    throw new Error("browser_args must be an array of --flag strings");
  }
  return raw.slice();
}

async function closeBrowser() {
  const b = state.browser;
  state.browser = null;
  state.spare = null;
  state.sparePromise = null;
  if (b) await b.close();
}

// ensureBrowser keeps exactly one browser per process. acceptInsecureCerts is
// always on because every route66 caller sets IgnoreHttpsErrors; a request that
// asks otherwise is refused in parseContext.
async function ensureBrowser(extraArgs) {
  const key = JSON.stringify(extraArgs);
  if (state.browser && state.argsKey === key && state.browser.connected) return;
  if (state.browser) {
    console.log(`[browse] relaunching browser: launch args changed (${state.argsKey} -> ${key})`);
    await closeBrowser();
  }
  const { puppeteer, chromium } = await browserLibs();
  const started = Date.now();
  state.browser = await puppeteer.launch({
    // WHY --single-process IS DROPPED: the Sparticuz defaults carry it, and in that
    // mode creating a second browser context crashes the browser
    // ("Target.createTarget: Target closed", measured in the Dockerfile.test
    // bench). A process-lifetime browser that recycles contexts needs them.
    args: chromium.args.filter((a) => a !== "--single-process").concat(extraArgs),
    executablePath: await chromium.executablePath(),
    headless: chromium.headless,
    acceptInsecureCerts: true,
    defaultViewport: null,
  });
  state.argsKey = key;
  state.launches += 1;
  // A browser that dies (OOM, crash) must not be handed to the next request.
  state.browser.once("disconnected", () => {
    console.error("[browse] browser disconnected; the next request relaunches it");
    state.browser = null;
    state.spare = null;
    state.sparePromise = null;
  });
  console.log(`[browse] browser launched in ${Date.now() - started}ms args=${key}`);
}

// prepareSpare builds the next request's context+page while this process is idle.
async function prepareSpare() {
  const context = await state.browser.createBrowserContext();
  const page = await context.newPage();
  state.spare = { context, page };
}

function scheduleSpare() {
  state.sparePromise = prepareSpare().catch((err) => {
    console.error(`[browse] spare context build failed: ${err && err.stack ? err.stack : err}`);
    state.spare = null;
  });
  return state.sparePromise;
}

// warm launches the browser and builds the first spare. Called by the pool lane
// before it starts polling, so no request ever lands on a cold process.
async function warm(extraArgs) {
  await ensureBrowser(parseBrowserArgs(extraArgs));
  await scheduleSpare();
  if (!state.spare) throw new Error("warm: first spare context could not be built");
}

// acquire hands the request a fresh context+page built for the current browser.
async function acquire(extraArgs) {
  await ensureBrowser(extraArgs);
  if (state.sparePromise) await state.sparePromise;
  if (!state.spare) await scheduleSpare();
  const spare = state.spare;
  if (!spare) throw new Error("no browser context could be built for this request");
  state.spare = null;
  state.sparePromise = null;
  return spare;
}

// release closes the used context (perfect isolation, no reset contract to trust)
// and builds the next spare in the background.
async function release(spare) {
  await spare.context.close().catch((err) => console.error(`[browse] context close failed: ${err.message}`));
  if (state.browser) scheduleSpare();
}

// ---------------------------------------------------------------------------
// Request parsing. Fail hard on anything the schema does not name.
// ---------------------------------------------------------------------------
function compileRegex(s, what) {
  if (typeof s !== "string" || !s) throw new Error(`${what} must be a non-empty regex string`);
  return new RegExp(s);
}

function parseContext(raw) {
  const c = raw || {};
  const viewport = c.viewport || { width: 1366, height: 900 };
  if (!Number.isInteger(viewport.width) || !Number.isInteger(viewport.height)) throw new Error("context.viewport needs integer width/height");
  if (c.ignore_https_errors === false) throw new Error("context.ignore_https_errors=false is unsupported: the pool browser accepts insecure certs");
  const headerRules = (c.extra_headers || []).map((r, i) => {
    if (!r || typeof r.host !== "string" || !r.host || typeof r.headers !== "object") throw new Error(`context.extra_headers[${i}] needs host and headers`);
    return { host: r.host.toLowerCase(), headers: r.headers };
  });
  const fulfill = (c.fulfill || []).map((r, i) => ({
    re: compileRegex(r.url_regex, `context.fulfill[${i}].url_regex`),
    status: r.status || 200,
    contentType: r.content_type || "application/octet-stream",
    body: Buffer.from(r.body_b64 || "", "base64"),
  }));
  const abort = (c.abort || []).map((s, i) => compileRegex(s, `context.abort[${i}]`));
  const initScripts = (c.init_scripts || []).map((s, i) => {
    if (typeof s !== "string") throw new Error(`context.init_scripts[${i}] must be a string`);
    return s;
  });
  return {
    viewport,
    headerRules,
    fulfill,
    abort,
    initScripts,
    cookies: c.cookies || [],
    defaultTimeoutMS: c.default_timeout_ms,
    navigationTimeoutMS: c.navigation_timeout_ms,
  };
}

// Only these ops exist. An unknown op is refused rather than skipped: a skipped
// step still yields a "successful" request about a page state that never existed.
const OPS = new Set([
  "goto", "wait_for_function", "evaluate", "click", "fill", "check", "press", "type",
  "wait_for_selector", "wait_for_url", "wait_for_navigation", "text", "attribute", "count",
  "content", "url", "cookies", "set_cookies", "screenshot", "sleep", "start_clock",
]);

function parseSteps(raw) {
  if (!Array.isArray(raw) || raw.length === 0) throw new Error("steps must be a non-empty array");
  if (raw.length > MAX_STEPS) throw new Error(`steps accepts at most ${MAX_STEPS}; got ${raw.length}`);
  const ids = new Set();
  return raw.map((s, i) => {
    if (!s || !OPS.has(s.op)) throw new Error(`steps[${i}].op ${JSON.stringify(s && s.op)} is not one of ${[...OPS].join(",")}`);
    const id = s.id || `${i}:${s.op}`;
    if (ids.has(id)) throw new Error(`steps[${i}].id ${id} is duplicated`);
    ids.add(id);
    for (const ref of ["if_ok", "if_failed"]) {
      if (s[ref] !== undefined && !ids.has(s[ref])) throw new Error(`steps[${i}].${ref} names ${s[ref]}, which is not an EARLIER step id`);
    }
    if (s.timeout_ms !== undefined && (!Number.isFinite(s.timeout_ms) || s.timeout_ms < 0 || s.timeout_ms > MAX_STEP_TIMEOUT_MS)) {
      throw new Error(`steps[${i}].timeout_ms must be 0..${MAX_STEP_TIMEOUT_MS}`);
    }
    return Object.assign({}, s, { id });
  });
}

// ---------------------------------------------------------------------------
// Timeouts. A step may draw from a named CLOCK (started by start_clock): the
// route66 harness bounds a page's waits by one shared deadline, and a per-step
// fixed timeout would let waits stack past it. Effective timeout =
// min(remaining clock, timeout_ms). An exhausted clock fails the step without
// running it -- the same "deadline exhausted before X began" the harness records.
// ---------------------------------------------------------------------------
function stepTimeout(step, clocks) {
  let t = step.timeout_ms === undefined ? null : step.timeout_ms;
  if (step.clock !== undefined) {
    const deadline = clocks.get(step.clock);
    if (deadline === undefined) throw new Error(`clock ${step.clock} was never started`);
    const remaining = deadline - Date.now();
    if (remaining <= 0) return { exhausted: true };
    t = t === null ? remaining : Math.min(t, remaining);
  }
  return { ms: t };
}

// A JS function source becomes an invocable expression. Only function sources are
// accepted: an expression-vs-function guess is exactly the ambiguity that made
// playwright's own string heuristics a trap.
function callExpr(fn, arg) {
  if (typeof fn !== "string" || !/^\s*(async\s*)?(\(|function\b|[A-Za-z_$][\w$]*\s*=>)/.test(fn)) {
    throw new Error("fn must be a JavaScript function source");
  }
  return `(${fn})(${arg === undefined ? "" : JSON.stringify(arg)})`;
}

// Value assertions evaluated here so the verdict travels with the value.
function checkExpect(expect, value) {
  if (!expect) return null;
  if ("equals" in expect && JSON.stringify(value) !== JSON.stringify(expect.equals)) return `expected ${JSON.stringify(expect.equals)}, got ${JSON.stringify(value)}`;
  if ("gte" in expect && !(value >= expect.gte)) return `expected >= ${expect.gte}, got ${JSON.stringify(value)}`;
  if ("lte" in expect && !(value <= expect.lte)) return `expected <= ${expect.lte}, got ${JSON.stringify(value)}`;
  if ("contains" in expect && !(typeof value === "string" && value.includes(expect.contains))) return `expected to contain ${JSON.stringify(expect.contains)}`;
  if ("truthy" in expect && !!value !== !!expect.truthy) return `expected truthy=${expect.truthy}, got ${JSON.stringify(value)}`;
  return null;
}

function safeName(name) {
  if (typeof name !== "string" || !/^[A-Za-z0-9._-]{1,200}$/.test(name)) throw new Error(`screenshot name ${JSON.stringify(name)} must match [A-Za-z0-9._-]{1,200}`);
  return name;
}

// Resolve a caller-named directory under the local root. It must stay inside.
function localDir(rel) {
  if (typeof rel !== "string" || !rel || path.isAbsolute(rel)) throw new Error("local_dir must be a relative path under the local root");
  const abs = path.resolve(LOCAL_ROOT, rel);
  if (abs !== LOCAL_ROOT && !abs.startsWith(LOCAL_ROOT + path.sep)) throw new Error(`local_dir ${rel} escapes the local root`);
  return abs;
}

// storeArtifact writes one artifact per the process's storage switch and returns
// where it went plus its sha256. PNG bytes never travel back in the response.
async function storeArtifact(event, deps, name, bytes, contentType) {
  const sha256 = crypto.createHash("sha256").update(bytes).digest("hex");
  if (STORE === "local") {
    const dir = localDir(event.local_dir);
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, name), bytes);
    return { store: "local", local_path: path.posix.join(event.local_dir.split(path.sep).join("/"), name), sha256, bytes: bytes.length };
  }
  if (typeof event.s3_prefix !== "string" || !event.s3_prefix) throw new Error("browse screenshot in S3 mode requires s3_prefix");
  const key = `${event.s3_prefix.replace(/\/+$/, "")}/${name}`;
  const put = await deps.s3Put(key, bytes, contentType);
  return { store: "s3", bucket: put.bucket, key, version_id: put.versionId || "", sha256, bytes: bytes.length };
}

// ---------------------------------------------------------------------------
// One step against the page. Returns the step's value; throws on failure.
// ---------------------------------------------------------------------------
async function runStep(step, page, ctx) {
  const t = stepTimeout(step, ctx.clocks);
  if (t.exhausted) throw new Error(`clock ${step.clock} exhausted before ${step.op} began`);
  const timeout = t.ms === null ? undefined : t.ms;
  switch (step.op) {
    case "goto": {
      const resp = await page.goto(step.url, { waitUntil: step.wait_until || "domcontentloaded", timeout });
      ctx.lastResponse = resp;
      if (!resp) return { status: 0, url: page.url() };
      const out = { status: resp.status(), url: resp.url(), headers: resp.headers() };
      if (step.body) out.body = await resp.text();
      return out;
    }
    case "wait_for_navigation": {
      const resp = await page.waitForNavigation({ waitUntil: step.wait_until || "domcontentloaded", timeout });
      return resp ? { status: resp.status(), url: resp.url() } : { status: 0, url: page.url() };
    }
    case "wait_for_function": {
      const opts = { timeout };
      if (step.polling !== undefined) opts.polling = step.polling;
      const handle = await page.waitForFunction(callExpr(step.fn, step.arg), opts);
      const v = await handle.jsonValue().catch(() => null);
      await handle.dispose().catch(() => {});
      return v;
    }
    case "evaluate":
      return page.evaluate(callExpr(step.fn, step.arg));
    case "click":
      await page.locator(step.selector).setTimeout(timeout === undefined ? 30000 : timeout).click();
      return null;
    case "fill":
      await page.locator(step.selector).setTimeout(timeout === undefined ? 30000 : timeout).fill(String(step.value));
      return null;
    case "check": {
      // Click only when unchecked: a check step states the END state.
      await page.waitForSelector(step.selector, { timeout });
      const checked = await page.$eval(step.selector, (el) => !!el.checked);
      if (!checked) await page.click(step.selector);
      return true;
    }
    case "press":
      if (step.selector) await page.focus(step.selector);
      await page.keyboard.press(step.key);
      return null;
    case "type":
      await page.type(step.selector, String(step.text), { delay: step.delay_ms || 0 });
      return null;
    case "wait_for_selector": {
      const st = step.state || "attached";
      const opts = { timeout, visible: st === "visible", hidden: st === "hidden" };
      await page.waitForSelector(step.selector, opts);
      return true;
    }
    case "wait_for_url": {
      const re = compileRegex(step.url_regex, "wait_for_url.url_regex");
      await page.waitForFunction(`new RegExp(${JSON.stringify(re.source)}).test(location.href)`, { timeout });
      return page.url();
    }
    case "text":
      return page.evaluate(`(() => { const el = ${step.selector ? `document.querySelector(${JSON.stringify(step.selector)})` : "document.body"}; return el ? el.innerText : null; })()`);
    case "attribute":
      return page.evaluate(`(() => { const el = document.querySelector(${JSON.stringify(step.selector)}); return el ? el.getAttribute(${JSON.stringify(step.name)}) : null; })()`);
    case "count":
      return page.evaluate(`document.querySelectorAll(${JSON.stringify(step.selector)}).length`);
    case "content":
      return page.content();
    case "url":
      return page.url();
    case "cookies": {
      const client = await page.createCDPSession();
      const { cookies } = await client.send("Network.getAllCookies");
      await client.detach().catch(() => {});
      return cookies;
    }
    case "set_cookies":
      await page.setCookie(...(step.cookies || []));
      return null;
    case "sleep":
      await new Promise((r) => setTimeout(r, step.ms || 0));
      return null;
    case "start_clock":
      if (typeof step.name !== "string" || !Number.isFinite(step.ms)) throw new Error("start_clock needs name and ms");
      ctx.clocks.set(step.name, Date.now() + step.ms);
      return null;
    case "screenshot": {
      const name = safeName(step.name);
      const png = Buffer.from(await Promise.race([
        page.screenshot({ type: "png", fullPage: step.full_page !== false }),
        new Promise((_, reject) => setTimeout(() => reject(new Error(`screenshot exceeded ${SCREENSHOT_TIMEOUT_MS}ms`)), SCREENSHOT_TIMEOUT_MS)),
      ]));
      const stored = await storeArtifact(ctx.event, ctx.deps, name, png, "image/png");
      stored.width = png.length > 24 ? png.readUInt32BE(16) : -1;
      stored.height = png.length > 24 ? png.readUInt32BE(20) : -1;
      // OCR runs in-process on the bytes just captured; its text is stored beside
      // the image and returned (text, never pixels).
      if (step.ocr) {
        const ocr = await ctx.deps.ocrImage(Object.assign({ image: png.toString("base64") }, step.ocr));
        stored.ocr = { text: ocr.text, error: ocr.error, metadata: ocr.metadata };
        stored.ocr_artifact = await storeArtifact(ctx.event, ctx.deps, `${name}.ocr.txt`, Buffer.from(ocr.text, "utf8"), "text/plain; charset=utf-8");
      }
      return stored;
    }
    default:
      throw new Error(`unreachable op ${step.op}`);
  }
}

// installContext applies the request's context-level setup to the fresh page:
// viewport, default timeouts, init scripts, cookies and request interception
// (abort -> fulfill -> host-scoped headers -> continue, the route66 ordering).
async function installContext(page, c, observed) {
  await page.setViewport(c.viewport);
  if (Number.isFinite(c.defaultTimeoutMS)) page.setDefaultTimeout(c.defaultTimeoutMS);
  if (Number.isFinite(c.navigationTimeoutMS)) page.setDefaultNavigationTimeout(c.navigationTimeoutMS);
  for (const src of c.initScripts) await page.evaluateOnNewDocument(src);
  if (c.cookies.length) await page.setCookie(...c.cookies);
  page.on("console", (msg) => {
    if (msg.type() === "error") observed.console_errors.push(msg.text().slice(0, 2000));
  });
  page.on("pageerror", (err) => observed.console_errors.push(`pageerror: ${String(err && err.message ? err.message : err).slice(0, 2000)}`));
  page.on("requestfailed", (req) => observed.failed_requests.push({ url: req.url().slice(0, 2000), error: (req.failure() && req.failure().errorText) || "" }));
  page.on("response", (resp) => {
    if (resp.status() >= 400) observed.http_errors.push({ url: resp.url().slice(0, 2000), status: resp.status() });
  });
  if (!c.abort.length && !c.fulfill.length && !c.headerRules.length) return;
  await page.setRequestInterception(true);
  page.on("request", (req) => {
    if (req.isInterceptResolutionHandled()) return;
    const url = req.url();
    if (c.abort.some((re) => re.test(url))) return void req.abort();
    const f = c.fulfill.find((r) => r.re.test(url));
    if (f) return void req.respond({ status: f.status, contentType: f.contentType, body: f.body });
    let host = "";
    try { host = new URL(url).hostname.toLowerCase(); } catch (_) { /* data: and friends carry no host */ }
    const rule = c.headerRules.find((r) => r.host === host);
    if (rule) return void req.continue({ headers: Object.assign({}, req.headers(), rule.headers) });
    return void req.continue();
  });
}

// browse is the Lambda action. deps = { ocrImage, s3Put } from index.js.
async function browse(event, deps) {
  if (state.busy) throw new Error("browse: a second concurrent request reached one pool process; each process serves one request at a time");
  state.busy = true;
  const started = Date.now();
  try {
    const browserArgs = parseBrowserArgs(event.browser_args);
    const c = parseContext(event.context);
    const steps = parseSteps(event.steps);
    const acquireStart = Date.now();
    const spare = await acquire(browserArgs);
    const acquireMS = Date.now() - acquireStart;
    const observed = { console_errors: [], failed_requests: [], http_errors: [] };
    const results = [];
    const ctx = { event, deps, clocks: new Map(), lastResponse: null };
    try {
      const setupStart = Date.now();
      await installContext(spare.page, c, observed);
      const setupMS = Date.now() - setupStart;
      let stopped = false;
      const byId = new Map();
      for (const step of steps) {
        const r = { id: step.id, op: step.op };
        const gate = (step.if_ok !== undefined && !(byId.get(step.if_ok) || {}).ok) ||
          (step.if_failed !== undefined && (byId.get(step.if_failed) || {}).ok !== false);
        if (stopped || gate) {
          r.skipped = true;
          results.push(r);
          byId.set(step.id, r);
          continue;
        }
        const t0 = Date.now();
        try {
          r.value = await runStep(step, spare.page, ctx);
          const failure = checkExpect(step.expect, r.value);
          r.ok = failure === null;
          if (failure) r.error = `expect: ${failure}`;
        } catch (err) {
          r.ok = false;
          r.error = String(err && err.message ? err.message : err);
        }
        r.elapsed_ms = Date.now() - t0;
        if (!r.ok && !step.optional) stopped = true;
        results.push(r);
        byId.set(step.id, r);
      }
      const finalURL = spare.page.url();
      state.served += 1;
      return {
        ok: results.every((r) => r.skipped || r.ok || steps.find((s) => s.id === r.id).optional),
        steps: results,
        final_url: finalURL,
        console_errors: observed.console_errors,
        failed_requests: observed.failed_requests,
        http_errors: observed.http_errors,
        timings: { total_ms: Date.now() - started, acquire_ms: acquireMS, setup_ms: setupMS },
        worker: { pid: process.pid, lane: process.env.SNAPBOT_LANE || "", browser_launches: state.launches, served: state.served, store: STORE },
      };
    } finally {
      await release(spare);
    }
  } finally {
    state.busy = false;
  }
}

module.exports = { browse, warm, STORE };
