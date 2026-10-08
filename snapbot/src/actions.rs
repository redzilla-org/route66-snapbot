//! puppeteer-core 25.9.0 page actions over a raw CDP page: navigation with
//! lifecycle waits, in-page polling waits, locator click/fill, keyboard and
//! cookies. Each mirrors the puppeteer call the Node handler made, including
//! its default waits and error wording, because route66's browse plans were
//! written against those semantics.

use crate::browser::{DocResponse, Page};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// A navigation's final main-frame response, or None (about:blank, same-document).
pub struct NavResponse {
    pub resp: DocResponse,
    pub headers: Value,
}

fn lifecycle_name(wait_until: &str) -> Result<&'static str> {
    Ok(match wait_until {
        "load" => "load",
        "domcontentloaded" => "DOMContentLoaded",
        "networkidle0" => "networkIdle",
        "networkidle2" => "networkAlmostIdle",
        other => bail!("Unknown value for options.waitUntil: {other}"),
    })
}

/// Timeout in ms -> deadline; 0 disables the timeout, as in puppeteer.
fn deadline(ms: u64) -> Option<Instant> {
    (ms > 0).then(|| Instant::now() + Duration::from_millis(ms))
}

fn is_context_gone(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("Execution context was destroyed")
        || s.contains("Cannot find default execution context")
        || s.contains("Cannot find context with specified id")
        || s.contains("Inspected target navigated or closed")
}

impl Page {
    fn nav_result(&self, loader: &str) -> Option<NavResponse> {
        let resp = self.state.lock().unwrap().doc_resp.get(loader).cloned()?;
        let headers = self.response_headers(&resp);
        Some(NavResponse { resp, headers })
    }

    /// page.goto(url, {waitUntil, timeout}).
    pub async fn goto(&self, url: &str, wait_until: &str, timeout_ms: u64) -> Result<Option<NavResponse>> {
        let event = lifecycle_name(wait_until)?;
        let dl = deadline(timeout_ms);
        let (frame, before) = {
            let st = self.state.lock().unwrap();
            (st.main_frame.clone(), st.loader.clone())
        };
        let nav = self.send("Page.navigate", json!({"url": url, "frameId": frame}));
        let r = match dl {
            Some(d) => tokio::time::timeout_at(d.into(), nav)
                .await
                .map_err(|_| anyhow!("Navigation timeout of {timeout_ms} ms exceeded"))??,
            None => nav.await?,
        };
        if let Some(err) = r.get("errorText").and_then(Value::as_str).filter(|e| !e.is_empty()) {
            if err != "net::ERR_HTTP_RESPONSE_CODE_FAILURE" {
                bail!("{err} at {url}");
            }
        }
        let Some(loader) = r.get("loaderId").and_then(Value::as_str).map(str::to_string) else {
            // Same-document navigation: no new document, no response.
            return Ok(None);
        };
        // WHY (route66 GH #4082, /content/privacy goto 8000ms timeouts in every
        // brand): puppeteer's LifecycleWatcher follows the frame's CURRENT
        // document, so a client-side redirect (the privacy stub's
        // window.location to compass.com) is waited through. Waiting on the
        // navigate's own loader alone hangs once the redirect replaces it
        // before that loader reached `event`. Either loader reaching it is done.
        let done = self
            .wait_state(dl, |st| {
                let own = st.lifecycle.get(&loader).is_some_and(|s| s.contains(event));
                let current = st.loader != before && st.loader != loader && st.lifecycle.get(&st.loader).is_some_and(|s| s.contains(event));
                (own || current).then_some(())
            })
            .await;
        if done.is_none() {
            bail!("Navigation timeout of {timeout_ms} ms exceeded");
        }
        Ok(self.nav_result(&loader))
    }

    /// page.waitForNavigation({waitUntil, timeout}).
    pub async fn wait_for_navigation(&self, wait_until: &str, timeout_ms: u64) -> Result<Option<NavResponse>> {
        let event = lifecycle_name(wait_until)?;
        let (initial_loader, initial_same) = {
            let st = self.state.lock().unwrap();
            (st.loader.clone(), st.same_doc)
        };
        let got = self
            .wait_state(deadline(timeout_ms), |st| {
                if st.same_doc != initial_same {
                    return Some(None);
                }
                if st.loader != initial_loader && st.lifecycle.get(&st.loader).is_some_and(|s| s.contains(event)) {
                    return Some(Some(st.loader.clone()));
                }
                None
            })
            .await;
        match got {
            None => bail!("Navigation timeout of {timeout_ms} ms exceeded"),
            Some(None) => Ok(None),
            Some(Some(loader)) => Ok(self.nav_result(&loader)),
        }
    }

    /// page.waitForNetworkIdle({idleTime, timeout}).
    pub async fn wait_for_network_idle(&self, idle_ms: u64, timeout_ms: u64) -> Result<()> {
        let idle = Duration::from_millis(idle_ms);
        let called = Instant::now();
        // Quiet means nothing in flight since the later of the call and the
        // last network event; puppeteer's idle timer also starts at the call.
        let ok = self
            .wait_state(deadline(timeout_ms), |st| {
                if !st.inflight.is_empty() {
                    return None;
                }
                let since = st.last_net.map_or(called, |t| t.max(called));
                (since.elapsed() >= idle).then_some(())
            })
            .await;
        ok.ok_or_else(|| anyhow!("Timed out after waiting {timeout_ms}ms"))
    }

    /// The in-page poller behind waitForFunction/waitForSelector: `pred` is a
    /// JS expression re-evaluated per `polling` until truthy. Navigations that
    /// destroy the context re-install it, as puppeteer's WaitTask does.
    pub async fn poll(&self, pred: &str, polling: &Value, timeout_ms: u64, timeout_msg: &str) -> Result<Option<Value>> {
        let dl = deadline(timeout_ms);
        let schedule = match polling {
            Value::Number(n) => format!("() => setTimeout(check, {})", n.as_f64().unwrap_or(100.0)),
            Value::String(s) if s == "mutation" => "() => {}".to_string(),
            Value::String(s) if s == "raf" => "() => requestAnimationFrame(check)".to_string(),
            Value::Null => "() => requestAnimationFrame(check)".to_string(),
            other => bail!("Unknown polling option: {other}"),
        };
        let observe = matches!(polling, Value::String(s) if s == "mutation");
        let budget = timeout_ms.max(1) + 1000;
        let expr = format!(
            "new Promise((resolve, reject) => {{
               let done = false; let obs = null;
               const stopAt = performance.now() + {budget};
               const finish = (f, v) => {{ if (done) return; done = true; if (obs) obs.disconnect(); f(v); }};
               const check = async () => {{
                 if (done) return;
                 let v;
                 try {{ v = await (async () => {{ return ({pred}); }})(); }} catch (e) {{ return finish(reject, e); }}
                 if (v) return finish(resolve, v);
                 if (performance.now() > stopAt) return finish(resolve, undefined);
                 schedule();
               }};
               const schedule = {schedule};
               if ({observe}) {{ obs = new MutationObserver(() => check()); obs.observe(document, {{childList: true, subtree: true, attributes: true, characterData: true}}); }}
               check();
             }})"
        );
        loop {
            let eval = self.evaluate(&expr);
            let r = match dl {
                Some(d) => match tokio::time::timeout_at(d.into(), eval).await {
                    Ok(r) => r,
                    Err(_) => bail!("{timeout_msg}"),
                },
                None => eval.await,
            };
            match r {
                Ok(Some(v)) => return Ok(Some(v)),
                Ok(None) => {}
                Err(e) if is_context_gone(&e) => {}
                Err(e) => bail!("Waiting failed: {e}"),
            }
            if dl.is_some_and(|d| Instant::now() >= d) {
                bail!("{timeout_msg}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// page.waitForSelector(selector, {visible, hidden, timeout}).
    pub async fn wait_for_selector(&self, selector: &str, state: &str, timeout_ms: u64) -> Result<()> {
        let sel = serde_json::to_string(selector)?;
        let visible = "(el) => { const st = getComputedStyle(el); const r = el.getBoundingClientRect(); return st.visibility !== 'hidden' && r.width > 0 && r.height > 0; }";
        let (pred, polling) = match state {
            "visible" => (format!("(() => {{ const el = document.querySelector({sel}); return !!el && ({visible})(el); }})()"), json!("raf")),
            "hidden" => (format!("(() => {{ const el = document.querySelector({sel}); return !el || !({visible})(el); }})()"), json!("raf")),
            _ => (format!("!!document.querySelector({sel})"), json!("mutation")),
        };
        let msg = format!("Waiting for selector `{selector}` failed: Waiting failed: {timeout_ms}ms exceeded");
        self.poll(&pred, &polling, timeout_ms, &msg).await.map(|_| ())
    }

    /// An element handle for `selector`, or None.
    pub async fn query(&self, selector: &str) -> Result<Option<String>> {
        let sel = serde_json::to_string(selector)?;
        self.query_object(&format!("document.querySelector({sel})")).await
    }

    async fn require(&self, selector: &str) -> Result<String> {
        self.query(selector).await?.ok_or_else(|| anyhow!("No element found for selector: {selector}"))
    }

    /// ElementHandle.click: scrollIntoViewIfNeeded, clickablePoint, mouse click.
    async fn click_handle(&self, obj: &str) -> Result<()> {
        // A background tab may never deliver IntersectionObserver callbacks;
        // read the viewport synchronously before using the same CDP click quads.
        let intersect = "function() { const r = this.getBoundingClientRect(); return r.width > 0 && r.height > 0 && r.left >= 0 && r.top >= 0 && r.right <= innerWidth && r.bottom <= innerHeight; }";
        let full = self.call_on(obj, intersect, vec![]).await?;
        if full != Some(json!(true)) {
            self.call_on(obj, "function() { this.scrollIntoView({block: 'center', inline: 'center', behavior: 'instant'}); }", vec![]).await?;
        }
        let quads = self.send("DOM.getContentQuads", json!({"objectId": obj})).await?;
        let metrics = self.send("Page.getLayoutMetrics", json!({})).await?;
        let vw = metrics.pointer("/cssLayoutViewport/clientWidth").and_then(Value::as_f64).unwrap_or(0.0);
        let vh = metrics.pointer("/cssLayoutViewport/clientHeight").and_then(Value::as_f64).unwrap_or(0.0);
        let mut point = None;
        for q in quads.get("quads").and_then(Value::as_array).cloned().unwrap_or_default() {
            let n: Vec<f64> = q.as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
            if n.len() != 8 {
                continue;
            }
            let pts: Vec<(f64, f64)> = (0..4).map(|i| (n[2 * i].clamp(0.0, vw), n[2 * i + 1].clamp(0.0, vh))).collect();
            let mut area = 0.0;
            for i in 0..4 {
                let (a, b) = (pts[i], pts[(i + 1) % 4]);
                area += (a.0 * b.1 - b.0 * a.1) / 2.0;
            }
            if area.abs() > 1.0 {
                let x = pts.iter().map(|p| p.0).sum::<f64>() / 4.0;
                let y = pts.iter().map(|p| p.1).sum::<f64>() / 4.0;
                point = Some((x, y));
                break;
            }
        }
        let (x, y) = point.ok_or_else(|| anyhow!("Node is either not clickable or not an Element"))?;
        self.mouse_click(x, y).await
    }

    /// page.click(selector): the element must exist now; no waiting.
    pub async fn click_selector(&self, selector: &str) -> Result<()> {
        let obj = self.require(selector).await?;
        let r = self.click_handle(&obj).await;
        self.release(&obj).await;
        r
    }

    /// page.focus(selector).
    pub async fn focus(&self, selector: &str) -> Result<()> {
        let obj = self.require(selector).await?;
        let r = self.call_on(&obj, "function() { this.focus(); }", vec![]).await;
        self.release(&obj).await;
        r.map(|_| ())
    }

    /// The locator preconditions: attached, in the viewport (scrolled if not),
    /// a stable bounding box across two host-clock samples, and enabled.
    async fn locator_ready(
        &self,
        selector: &str,
        phase: &mut &str,
        last_error: &mut Option<String>,
    ) -> Result<Option<String>> {
        let Some(obj) = self.query(selector).await? else {
            return Ok(None);
        };
        // Hidden Chromium tabs can suspend observer and animation-frame callbacks.
        // Two CDP reads separated by the host clock retain the stability check.
        let snapshot = "function() {
            if (!this.isConnected) return null;
            let r = this.getBoundingClientRect();
            if (r.width <= 0 || r.height <= 0) return null;
            if (r.right <= 0 || r.bottom <= 0 || r.left >= innerWidth || r.top >= innerHeight) {
                this.scrollIntoView({block: 'center', inline: 'center', behavior: 'instant'});
                r = this.getBoundingClientRect();
            }
            if (r.width <= 0 || r.height <= 0 || r.right <= 0 || r.bottom <= 0 || r.left >= innerWidth || r.top >= innerHeight) return null;
            if (this instanceof HTMLElement && ['BUTTON','INPUT','SELECT','TEXTAREA','OPTION','OPTGROUP'].includes(this.nodeName) && this.hasAttribute('disabled')) return null;
            return [r.x, r.y, r.width, r.height];
        }";
        let first = self.call_on(&obj, snapshot, vec![]).await;
        let result = match first {
            Ok(Some(ref a)) if !a.is_null() => {
                tokio::time::sleep(Duration::from_millis(32)).await;
                self.call_on(&obj, snapshot, vec![]).await.map(|b| b.is_some_and(|v| v == *a))
            }
            Ok(_) => Ok(false),
            Err(error) => Err(error),
        };
        match result {
            Ok(true) => Ok(Some(obj)),
            result => {
                // Keep readiness retries unchanged, but retain the error that
                // would otherwise disappear before a locator timeout.
                if let Err(error) = result {
                    *last_error = Some(error.to_string());
                }
                *phase = "readiness-release";
                self.release(&obj).await;
                Ok(None)
            }
        }
    }

    /// Retry `action` on a ready element every 100ms until it succeeds or the
    /// locator timeout passes (puppeteer's retryAndRaceWithSignalAndTimer).
    async fn with_locator<F, Fut>(
        &self,
        selector: &str,
        timeout_ms: u64,
        mut action: F,
    ) -> Result<()>
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let dl = deadline(timeout_ms);
        // A four-second locator timeout cancels CDP before its five-second
        // slow-call trace. Retain the exact phase without changing the action.
        let mut phase = "readiness";
        let mut attempts = 0;
        let mut last_error = None;
        let attempt = async {
            loop {
                attempts += 1;
                phase = "readiness";
                let ready = self
                    .locator_ready(selector, &mut phase, &mut last_error)
                    .await;
                if let Err(error) = &ready {
                    last_error = Some(error.to_string());
                }
                if let Some(obj) = ready.unwrap_or(None) {
                    phase = "action";
                    let r = action(obj.clone()).await;
                    if let Err(error) = &r {
                        last_error = Some(error.to_string());
                    }
                    phase = "action-release";
                    self.release(&obj).await;
                    if r.is_ok() {
                        return;
                    }
                }
                phase = "retry-delay";
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        match dl {
            Some(d) => match tokio::time::timeout_at(d.into(), attempt).await {
                Ok(()) => Ok(()),
                Err(_) => {
                    if crate::runtime::diagnostic_enabled() {
                        eprintln!("[snapbot diagnostic] {} locator_timeout_ms={timeout_ms} phase={phase} attempts={attempts} last_error={last_error:?}", crate::runtime::diagnostic_identity());
                    }
                    Err(anyhow!("Timed out after waiting {timeout_ms}ms"))
                }
            },
            None => {
                attempt.await;
                Ok(())
            }
        }
    }

    /// page.locator(selector).setTimeout(t).click().
    pub async fn locator_click(&self, selector: &str, timeout_ms: u64) -> Result<()> {
        self.with_locator(selector, timeout_ms, |obj| async move { self.click_handle(&obj).await }).await
    }

    /// page.locator(selector).setTimeout(t).fill(value).
    pub async fn locator_fill(&self, selector: &str, value: &str, timeout_ms: u64) -> Result<()> {
        let value = value.to_string();
        self.with_locator(selector, timeout_ms, |obj| {
            let value = value.clone();
            async move { self.fill_handle(&obj, &value).await }
        })
        .await
    }

    async fn fill_handle(&self, obj: &str, value: &str) -> Result<()> {
        let kind = self
            .call_on(
                obj,
                "function() {
                    if (this instanceof HTMLSelectElement) return 'select';
                    if (this instanceof HTMLTextAreaElement) return 'typeable-input';
                    if (this instanceof HTMLInputElement) {
                        switch (this.type) {
                            case 'checkbox': case 'radio': return 'checkable-input';
                            case 'text': case 'url': case 'tel': case 'search': case 'password': case 'number': case 'email': return 'typeable-input';
                            default: return 'other-input';
                        }
                    }
                    switch (this.getAttribute('role')) { case 'checkbox': case 'radio': case 'switch': return 'checkable-input'; }
                    if (this.isContentEditable) return 'contenteditable';
                    return 'unknown';
                }",
                vec![],
            )
            .await?;
        let kind = kind.and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        match kind.as_str() {
            "checkable-input" => {
                let state = self
                    .call_on(obj, "function() { if (this.indeterminate || this.getAttribute('aria-checked') === 'mixed') return 'mixed'; return this.checked || this.getAttribute('aria-checked') === 'true'; }", vec![])
                    .await?;
                // A string value is truthy unless empty.
                if state == Some(json!("mixed")) || state != Some(json!(!value.is_empty())) {
                    self.click_handle(obj).await?;
                }
                Ok(())
            }
            "select" => {
                self.call_on(
                    obj,
                    "function(vals) {
                        const values = new Set(vals);
                        if (!this.multiple) {
                            for (const o of this.options) o.selected = false;
                            for (const o of this.options) { if (values.has(o.value)) { o.selected = true; break; } }
                        } else {
                            for (const o of this.options) o.selected = values.has(o.value);
                        }
                        this.dispatchEvent(new Event('input', {bubbles: true}));
                        this.dispatchEvent(new Event('change', {bubbles: true}));
                    }",
                    vec![json!([value])],
                )
                .await?;
                Ok(())
            }
            "typeable-input" | "contenteditable" if value.encode_utf16().count() < 100 => {
                let to_type = self
                    .call_on(
                        obj,
                        "function(newValue) {
                            const valString = String(newValue);
                            const currentValue = this.isContentEditable ? this.innerText : this.value;
                            if (currentValue === valString) return '';
                            if (!valString.startsWith(currentValue) || !currentValue) {
                                if (this.isContentEditable) this.innerText = ''; else this.value = '';
                                return valString;
                            }
                            if (this.isContentEditable) { this.innerText = ''; this.innerText = currentValue; }
                            else { this.value = ''; this.value = currentValue; }
                            return valString.substring(currentValue.length);
                        }",
                        vec![json!(value)],
                    )
                    .await?;
                let text = to_type.and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                if !text.is_empty() {
                    self.call_on(obj, "function() { this.focus(); }", vec![]).await?;
                    self.type_text(&text, 0).await?;
                }
                Ok(())
            }
            "typeable-input" | "contenteditable" | "other-input" => {
                self.call_on(obj, "function() { this.focus(); }", vec![]).await?;
                self.call_on(
                    obj,
                    "function(newValue) {
                        const valString = String(newValue);
                        const currentValue = this.isContentEditable ? this.innerText : this.value;
                        if (currentValue === valString) return;
                        if (this.isContentEditable) this.innerText = valString; else this.value = valString;
                        this.dispatchEvent(new Event('input', {bubbles: true}));
                        this.dispatchEvent(new Event('change', {bubbles: true}));
                    }",
                    vec![json!(value)],
                )
                .await?;
                Ok(())
            }
            _ => bail!("Element cannot be filled out."),
        }
    }

    /// page.type(selector, text, {delay}).
    pub async fn type_into(&self, selector: &str, text: &str, delay_ms: u64) -> Result<()> {
        self.focus(selector).await?;
        self.type_text(text, delay_ms).await
    }

    /// page.content().
    pub async fn content(&self) -> Result<String> {
        let v = self
            .evaluate("(() => { let s = ''; if (document.doctype) s = new XMLSerializer().serializeToString(document.doctype); if (document.documentElement) s += document.documentElement.outerHTML; return s; })()")
            .await?;
        Ok(v.and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default())
    }

    /// Network.getAllCookies, as the Node handler read it.
    pub async fn all_cookies(&self) -> Result<Value> {
        let r = self.send("Network.getAllCookies", json!({})).await?;
        Ok(r.get("cookies").cloned().unwrap_or(json!([])))
    }

    /// page.setCookie(...cookies): default url from the page, delete then set.
    pub async fn set_cookies(&self, cookies: &[Value]) -> Result<()> {
        let page_url = self.url();
        let http = page_url.starts_with("http");
        let mut items = Vec::new();
        for c in cookies {
            let mut item = c.as_object().cloned().ok_or_else(|| anyhow!("cookie must be an object"))?;
            let name = item.get("name").map(crate::js::js_string).unwrap_or_default();
            if !item.get("url").is_some_and(|u| crate::js::truthy(Some(u))) && http {
                item.insert("url".to_string(), json!(page_url));
            }
            let url = item.get("url").and_then(Value::as_str).unwrap_or("");
            if url == "about:blank" {
                bail!("Blank page can not have cookie \"{name}\"");
            }
            if url.starts_with("data:") {
                bail!("Data URL page can not have cookie \"{name}\"");
            }
            items.push(item);
        }
        for item in &items {
            let mut del = serde_json::Map::new();
            for k in ["name", "url", "domain", "path"] {
                if let Some(v) = item.get(k) {
                    del.insert(k.to_string(), v.clone());
                }
            }
            if !del.contains_key("url") && http {
                del.insert("url".to_string(), json!(page_url));
            }
            self.send("Network.deleteCookies", Value::Object(del.clone())).await?;
            if http {
                if let Ok(u) = url::Url::parse(&page_url) {
                    let origin = u.origin().ascii_serialization();
                    let site = match u.port() {
                        Some(p) => origin.replace(&format!(":{p}"), ""),
                        None => origin,
                    };
                    del.insert("partitionKey".to_string(), json!({"topLevelSite": site, "hasCrossSiteAncestor": false}));
                    let _ = self.send("Network.deleteCookies", Value::Object(del)).await;
                }
            }
        }
        if !items.is_empty() {
            let cookies: Vec<Value> = items.into_iter().map(Value::Object).collect();
            self.send("Network.setCookies", json!({"cookies": cookies})).await?;
        }
        Ok(())
    }
}

pub fn nav_json(r: &Option<NavResponse>, url_fallback: &str) -> Value {
    match r {
        Some(n) => json!({"status": n.resp.status, "url": n.resp.url, "headers": n.headers}),
        None => json!({"status": 0, "url": url_fallback}),
    }
}
