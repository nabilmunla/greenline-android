// Greenline measurement screen (task 2b.7): the scripted run per file and the numbers-only export.
//
// Steps (one JSON line each, written and fsynced by the app as soon as the step ends):
//   open (recorded by the app) -> first view (fd received -> low-res fit painted, then full fit painted)
//   -> 4 tiles at 400 % at the glspike dl positions (render, handoff = total - engine wait, total)
//   -> full-screen refill at 400 % -> 20 spread pages (ShowPage, fit 1600; skipped for 1-page files)
//   -> optional whole-file walk (Cancel stops it). The app adds VmRSS, VmHWM and the direct-mode flag
//   to every line. Tile positions, refill tiles and pages come from the app (measure_plan).
//
// What goes to the app is numbers and fixed step names only (the app refuses anything else), and the
// page shows numbers and fixed codes only.
//
// Confidentiality: this file never logs (no console calls) and never sees or shows a path, URI or name.
"use strict";
(() => {
  const T = window.__TAURI__;
  const invoke = T.core.invoke;
  const $ = (id) => document.getElementById(id);

  const MAX_INFLIGHT = 3;   // as the viewer: each pending gl:// request holds a WebView request thread
  const WALK_EVERY = 25;    // walk progress line every this many pages
  const BUFFER_MAX = 32;    // engine events kept for a waiter that has not asked yet

  const GL = (() => {
    try {
      const u = T.core.convertFileSrc("", "gl");
      return u.endsWith("/") ? u : u + "/";
    } catch (_) {
      return /^https?:$/.test(location.protocol) ? location.protocol + "//gl.localhost/" : "gl://localhost/";
    }
  })();

  const canvas = $("rcanvas");
  const ctx = canvas.getContext("2d");

  const LABELS = Array.from({ length: 12 }, (_, i) => "f" + String(i).padStart(2, "0"));

  // ---------------------------------------------------------------------------------------------
  // engine events: buffered until a step waits for them (the app requests page 0 before answering
  // measure_start, so its images can arrive before the page knows the document serial)

  let doc = null;            // document serial of the run
  const buffer = [];         // { type: "image" | "error" | "cancelled", doc, ... }
  const waiters = [];        // { match, resolve }

  function deliver(ev) {
    const i = waiters.findIndex((w) => w.match(ev));
    if (i >= 0) {
      const [w] = waiters.splice(i, 1);
      w.resolve(ev);
      return;
    }
    buffer.push(ev);
    if (buffer.length > BUFFER_MAX) buffer.shift();
  }

  function waitFor(match) {
    const i = buffer.findIndex(match);
    if (i >= 0) return Promise.resolve(buffer.splice(i, 1)[0]);
    return new Promise((resolve) => waiters.push({ match, resolve }));
  }

  class Stop extends Error {
    constructor(code, extra) { super(code); this.code = code; this.extra = extra || {}; }
  }

  // The image event of `kind` for `page` of this run's document; an engine error or a cancel stops.
  async function waitImage(page, kind) {
    const ev = await waitFor((e) => e.doc === doc &&
      ((e.type === "image" && e.page === page && e.kind === kind) || e.type === "error" || e.type === "cancelled"));
    if (ev.type === "error") throw new Stop("engine_error", { engine_error: ev.code });
    if (ev.type === "cancelled") throw new Stop("cancelled");
    return ev;
  }

  let progress = null;  // last gl-progress of this run: { page, stage, elapsed_ms }

  T.event.listen("gl-image", (e) => deliver({ type: "image", ...e.payload }));
  T.event.listen("gl-error", (e) => deliver({ type: "error", doc: e.payload.doc, code: e.payload.code }));
  T.event.listen("gl-cancelled", (e) => deliver({ type: "cancelled", doc: e.payload.doc }));
  T.event.listen("gl-progress", (e) => {
    if (e.payload.doc === doc) progress = { page: e.payload.page, stage: e.payload.stage, elapsed_ms: e.payload.elapsed_ms };
  });

  // ---------------------------------------------------------------------------------------------
  // helpers

  const frame = () => new Promise((r) => requestAnimationFrame(() => r()));
  const now = () => performance.now();
  const r1 = (v) => Math.round(v * 10) / 10;

  function jsHeapMb() {
    const m = performance.memory;
    return m && typeof m.usedJSHeapSize === "number" ? r1(m.usedJSHeapSize / 1048576) : undefined;
  }

  // Only finite numbers, bools and fixed names go to the app; anything else is left out.
  function clean(o) {
    const out = {};
    for (const [k, v] of Object.entries(o)) {
      if (typeof v === "number" && Number.isFinite(v)) out[k] = v;
      else if (typeof v === "boolean" || typeof v === "string") out[k] = v;
    }
    return out;
  }

  function codeOf(e) {
    const c = e && typeof e === "object" ? e.code : e;
    return typeof c === "string" && /^[A-Za-z0-9_]+$/.test(c) ? c : "E_UNKNOWN";
  }

  function sizeCanvas() {
    const r = canvas.getBoundingClientRect();
    const dpr = window.devicePixelRatio || 1;
    canvas.width = Math.max(1, Math.round(r.width * dpr));
    canvas.height = Math.max(1, Math.round(r.height * dpr));
  }

  function clearCanvas() {
    ctx.fillStyle = "#3a3a3a";
    ctx.fillRect(0, 0, canvas.width, canvas.height);
  }

  function drawFit(bmp) {
    const cw = canvas.width, ch = canvas.height;
    const s = Math.min(cw / bmp.width, ch / bmp.height);
    const w = bmp.width * s, h = bmp.height * s;
    clearCanvas();
    ctx.imageSmoothingEnabled = true;
    ctx.drawImage(bmp, (cw - w) / 2, (ch - h) / 2, w, h);
  }

  let ctrl = null;  // AbortController of this run's fetches

  async function fetchRgba(url) {
    const r = await fetch(url, { cache: "no-store", signal: ctrl.signal });
    if (r.status !== 200) {
      const code = +r.headers.get("X-Gl-Error");
      throw new Stop(r.status === 204 ? "cancelled" : (code ? "engine_error" : "fetch"), code ? { engine_error: code } : {});
    }
    const w = +r.headers.get("X-Gl-W"), h = +r.headers.get("X-Gl-H");
    const renderMs = +r.headers.get("X-Gl-Render-Ms");
    const waitMs = +r.headers.get("X-Gl-Wait-Ms");
    const buf = await r.arrayBuffer();
    const bmp = await createImageBitmap(new ImageData(new Uint8ClampedArray(buf), w, h));
    return { bmp, renderMs, waitMs };
  }

  let gen = 0;
  const tileUrl = (page, zoom, x0, y0, g) =>
    GL + "tile?page=" + page + "&zoom=" + zoom + "&x0=" + x0 + "&y0=" + y0 + "&gen=" + g;

  // ---------------------------------------------------------------------------------------------
  // the run

  const run = { label: 0, active: false, cancel: false, stepName: "", stepT0: 0 };

  function setStep(name) {
    run.stepName = name;
    run.stepT0 = now();
    progress = null;
    tick();
  }

  function tick() {
    if (!run.active) return;
    const s = Math.floor((now() - run.stepT0) / 1000);
    const p = progress && progress.stage === "read" ? ", reading page" : "";
    $("rstep").textContent = run.stepName + " · " + s + " s" + p;
  }
  setInterval(tick, 250);

  function logLine(s) {
    const d = document.createElement("div");
    d.textContent = s;
    $("log").appendChild(d);
  }

  const f0 = (v) => (typeof v === "number" ? v.toFixed(0) : "–");

  async function step(o, text) {
    if (o.js_heap_mb === undefined) o.js_heap_mb = jsHeapMb();
    const info = await invoke("measure_step", { step: JSON.stringify(clean(o)) });
    logLine(text + " · RSS " + f0(info.rss_mb) + " MB, peak " + f0(info.hwm_mb) + " MB" +
      (info.direct ? " · direct mode (" + info.direct_reason + ")" : ""));
    return info;
  }

  async function firstView(start, tResolved) {
    setStep("First view");
    const low = await waitImage(0, "lowres");
    const a = await fetchRgba(GL + "img?id=" + low.id);
    drawFit(a.bmp);
    await frame();
    const tLow = now();
    a.bmp.close();
    const fit = await waitImage(0, "fit");
    const b = await fetchRgba(GL + "img?id=" + fit.id);
    drawFit(b.bmp);
    await frame();
    const tFit = now();
    b.bmp.close();
    const fv = {
      step: "first_view", page: 0,
      lowres_ms: r1(start.since_fd_ms + (tLow - tResolved)),
      fit_ms: r1(start.since_fd_ms + (tFit - tResolved)),
      list_ms: low.list_ms, engine_lowres_ms: low.total_ms, engine_fit_ms: fit.total_ms,
      pt_w: fit.pt_w, pt_h: fit.pt_h,
    };
    await step(fv, "First view: low-res " + (fv.lowres_ms / 1000).toFixed(2) + " s, fit " + (fv.fit_ms / 1000).toFixed(2) + " s");
    return fit;
  }

  async function tiles(plan) {
    for (let i = 0; i < plan.tiles.length; i++) {
      setStep("400 % tile " + (i + 1) + " of " + plan.tiles.length);
      const [x0, y0] = plan.tiles[i];
      gen++;
      const t0 = now();
      const t = await fetchRgba(tileUrl(0, plan.zoom, x0, y0, gen));
      clearCanvas();
      ctx.imageSmoothingEnabled = false;
      ctx.drawImage(t.bmp, Math.round((canvas.width - t.bmp.width) / 2), Math.round((canvas.height - t.bmp.height) / 2));
      await frame();
      const total = now() - t0;
      t.bmp.close();
      const handoff = Math.max(0, total - t.waitMs);
      await step({
        step: "tile", page: 0, tile: i + 1, x0, y0,
        render_ms: t.renderMs, wait_ms: t.waitMs, handoff_ms: r1(handoff), total_ms: r1(total),
      }, "Tile " + (i + 1) + ": total " + f0(total) + " ms (render " + f0(t.renderMs) + ", handoff " + f0(handoff) + ")");
    }
  }

  async function refill(plan) {
    setStep("Full-screen refill");
    clearCanvas();
    gen++;
    const g = gen, queue = plan.refill.slice();
    let maxMs = 0, renderSum = 0, done = 0;
    const t0 = now();
    async function worker() {
      while (queue.length) {
        const [x0, y0] = queue.shift();
        const ts = now();
        const t = await fetchRgba(tileUrl(0, plan.zoom, x0, y0, g));
        ctx.imageSmoothingEnabled = false;
        ctx.drawImage(t.bmp, x0 - plan.view_x0, y0 - plan.view_y0);
        t.bmp.close();
        maxMs = Math.max(maxMs, now() - ts);
        renderSum += t.renderMs;
        done++;
      }
    }
    await Promise.all(Array.from({ length: Math.min(MAX_INFLIGHT, queue.length) }, worker));
    await frame();
    const ms = now() - t0;
    await step({ step: "refill", page: 0, tiles: done, ms: r1(ms), max_ms: r1(maxMs), render_ms: r1(renderSum) },
      "Refill: " + done + " tiles in " + f0(ms) + " ms");
  }

  async function showAndWait(page, fitWidth) {
    const t0 = now();
    await invoke("show_page", { page, fitWidth });
    const ev = await waitImage(page, "fit");
    return { ms: now() - t0, ev };
  }

  async function pages(plan, fitWidth) {
    for (let i = 0; i < plan.pages.length; i++) {
      const p = plan.pages[i];
      setStep("Page " + (i + 1) + " of " + plan.pages.length);
      const { ms, ev } = await showAndWait(p, fitWidth);
      await step({ step: "page", page: p, ms: r1(ms), engine_ms: ev.total_ms, list_ms: ev.list_ms },
        "Page #" + (i + 1) + ": " + f0(ev.total_ms) + " ms in the engine, " + f0(ms) + " ms here");
    }
  }

  async function walk(n, fitWidth) {
    const t0 = now();
    let walked = 0, maxMs = 0, cancelled = false;
    try {
      for (let p = 0; p < n; p++) {
        if (run.cancel) { cancelled = true; break; }
        setStep("Walk: page " + (p + 1) + " of " + n);
        const { ev } = await showAndWait(p, fitWidth);
        walked++;
        maxMs = Math.max(maxMs, ev.total_ms);
        if (walked % WALK_EVERY === 0 && walked < n) {
          await step({ step: "walk_progress", page: p, walked, ms: r1(now() - t0), max_ms: r1(maxMs) },
            "Walk: " + walked + " pages");
        }
      }
    } catch (e) {
      if (!(e instanceof Stop) || e.code !== "cancelled") throw e;
      cancelled = true;
    }
    await step({ step: "walk", walked, ms: r1(now() - t0), max_ms: r1(maxMs), cancelled },
      "Walk: " + walked + " of " + n + " pages" + (cancelled ? " (cancelled)" : "") + ", slowest " + f0(maxMs) + " ms");
    return cancelled;
  }

  async function runFile() {
    $("err").hidden = true;
    $("log").textContent = "";
    run.label = +$("label").value;
    run.cancel = false;
    ctrl = new AbortController();
    gen = 0;
    doc = null;
    buffer.length = 0;
    $("run").disabled = true;
    let start;
    try {
      start = await invoke("measure_start", { label: run.label });
    } catch (e) {
      $("run").disabled = false;
      const c = codeOf(e);
      if (c !== "E_PICK_CANCELLED") showErr("Open failed: " + c + (e && e.recorded ? " (recorded)" : ""));
      await refresh();
      return;
    }
    const tResolved = now();
    doc = start.doc;
    run.active = true;
    $("rlabel").textContent = LABELS[run.label];
    $("runview").hidden = false;
    sizeCanvas();
    clearCanvas();
    logLine(LABELS[run.label] + ": " + start.pages + " pages");
    let cancelled = false;
    try {
      const fit = await firstView(start, tResolved);
      const plan = await invoke("measure_plan", {
        pages: start.pages, ptW: fit.pt_w, ptH: fit.pt_h, viewW: canvas.width, viewH: canvas.height,
      });
      await tiles(plan);
      await refill(plan);
      await pages(plan, start.fit_width);
      if ($("walk").checked && await walk(start.pages, start.fit_width)) cancelled = true;
    } catch (e) {
      // A cancel also aborts the tile fetches in flight (an AbortError, not a Stop). Only a cancel is
      // reported as cancelled; any other failure ends the run as a failed step.
      if (run.cancel || (e instanceof Stop && e.code === "cancelled")) {
        cancelled = true;
        logLine("Cancelled");
      } else {
        const c = e instanceof Stop ? e.code : codeOf(e);
        showErr("Step failed: " + c + (e instanceof Stop && e.extra.engine_error ? " (engine code " + e.extra.engine_error + ")" : ""));
      }
    }
    run.active = false;
    ctrl.abort();
    try { await invoke("measure_end", { cancelled }); } catch (e) { showErr("Could not end the run: " + codeOf(e)); }
    try { await invoke("close_doc"); } catch (_) { /* engine gone */ }
    doc = null;
    $("runview").hidden = true;
    $("run").disabled = false;
    await refresh();
  }

  async function cancelRun() {
    run.cancel = true;
    if (ctrl) ctrl.abort();
    try { await invoke("cancel"); } catch (_) { /* engine gone */ }
    // A tile fetch aborted above ends its step with an AbortError: treat it as the cancel.
    deliver({ type: "cancelled", doc });
  }

  function showErr(s) {
    $("err").textContent = s;
    $("err").hidden = false;
  }

  // ---------------------------------------------------------------------------------------------
  // summary and export

  const VERDICT = { pass: ["PASS", "pass"], fail: ["FAIL", "fail"], "n/a": ["–", "na"], missing: ["missing", "missing"] };

  function cell(tr, value, verdict) {
    const td = document.createElement("td");
    const [word, cls] = VERDICT[verdict] || ["", ""];
    td.textContent = value + (word && word !== "–" ? " " + word : "");
    if (cls) td.className = cls;
    tr.appendChild(td);
  }

  const sec = (v) => (typeof v === "number" ? (v / 1000).toFixed(2) + " s" : "–");
  const msf = (v) => (typeof v === "number" ? v.toFixed(0) + " ms" : "–");
  const mb = (v) => (typeof v === "number" ? v.toFixed(0) + " MB" : "–");

  async function refresh() {
    let st;
    try {
      st = await invoke("measure_state");
    } catch (e) {
      showErr("Could not read the results: " + codeOf(e));
      return;
    }
    const sel = $("label");
    sel.textContent = "";
    LABELS.forEach((l, i) => {
      const o = document.createElement("option");
      o.value = String(i);
      o.textContent = l + (st.used.includes(l) ? " (done)" : "");
      sel.appendChild(o);
    });
    sel.value = String(st.next_label); // default: the next unused label
    if (st.recovered.length) {
      $("recovered").textContent = "The last run of " + st.recovered.join(", ") +
        " did not finish (the app was probably killed): recorded as a probable kill.";
      $("recovered").hidden = false;
    }
    if (st.recover_error) {
      $("recovered").textContent = "Could not check the saved results for an unfinished run (" + codeOf(st.recover_error) + ").";
      $("recovered").hidden = false;
    }
    const m = st.marks;
    $("marks").textContent = "First view ≤ " + m.first_view_ms / 1000 + " s (" + m.first_view_labels.join(", ") +
      ") · 400 % tile on screen ≤ " + m.tile_ms / 1000 + " s · refill ≤ " + m.refill_ms / 1000 +
      " s · pages ≤ " + m.page_ms + " ms (median) · peak ≤ " + (m.peak_mb / 1024).toFixed(1) + " GB · no kills";
    const tb = $("summary");
    tb.textContent = "";
    for (const s of JSON.parse(st.summary)) {
      const tr = document.createElement("tr");
      const th = document.createElement("td");
      th.textContent = s.label + (s.complete ? "" : (s.killed ? "" : " (incomplete)"));
      tr.appendChild(th);
      cell(tr, s.killed ? "killed" : "none", s.kill);
      cell(tr, sec(s.first_view_ms), s.first_view);
      cell(tr, msf(s.tile_max_ms), s.tile);
      cell(tr, msf(s.refill_ms), s.refill);
      cell(tr, msf(s.page_median_ms) + " / " + msf(s.page_max_ms), s.page);
      cell(tr, mb(s.peak_mb), s.peak);
      cell(tr, s.direct ? "yes" : "no", "");
      tb.appendChild(tr);
    }
    $("count").textContent = st.records + " lines saved on the phone.";
  }

  async function exportResults() {
    $("export").disabled = true;
    $("exported").textContent = "";
    const d = new Date();
    try {
      const r = await invoke("measure_export", {
        year: d.getFullYear(), month: d.getMonth() + 1, day: d.getDate(), hour: d.getHours(), minute: d.getMinutes(),
      });
      $("exported").textContent = "Saved " + r.lines + " lines (" + r.bytes + " bytes).";
    } catch (e) {
      const c = codeOf(e);
      $("exported").textContent = c === "E_SAVE_CANCELLED" ? "Export cancelled." : "Export failed: " + c;
    } finally {
      $("export").disabled = false;
    }
  }

  $("run").addEventListener("click", runFile);
  $("cancel").addEventListener("click", cancelRun);
  $("export").addEventListener("click", exportResults);
  $("back").addEventListener("click", async () => {
    if (run.active) await cancelRun();
    location.href = "index.html";
  });
  refresh();
})();
