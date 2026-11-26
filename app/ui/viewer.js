// Greenline viewer: touch-first (tasks 2b.6, 2bf.5, 2bf.7). Open a PDF into a canvas; pinch to zoom
// (fit .. 400 %), drag to pan, swipe at fit to change page, double-tap to zoom in / back to fit, tap to
// show or hide the toolbar. View math and gesture rules live in viewmath.js (GLMath, tested on its own).
// Page direction (PD.3): the "pageDirection" pref picks the swipe axis, read each time the viewer is
// shown. "vertical" (default): swipe up = next page, down = previous. "sideways": left = next, right =
// previous. Zoomed in, a drag only pans and never changes page.
//
// Pixels never go through IPC. Commands (open_picked, show_page, cancel, close_doc) and events
// (gl-progress, gl-image, gl-cancelled, gl-error, gl-direct, gl-memory) carry numbers and fixed codes
// only; images and tiles are fetched as raw RGBA over the async `gl` scheme (http://gl.localhost/ on
// Android): fetch -> arrayBuffer -> ImageData -> createImageBitmap -> drawImage.
// Response headers: X-Gl-W, X-Gl-H, X-Gl-Render-Ms (engine render), X-Gl-Wait-Ms (handler -> engine
// answer), X-Gl-Gen. Handoff = fetch total - wait (protocol + WebView transfer + bitmap).
//
// Tiles: nothing is requested while a finger is down. 150 ms after the view settles, the smallest engine
// level in {2, 4} (device px per point) that covers the screen is requested; until then whatever is
// cached (any level) is drawn scaled over the fit image. The view generation `gen` is bumped only when
// the view settles, so the engine skips tiles of views the user has already left.
//
// Confidentiality: this file never logs (no console calls) and never shows a path, URI or name.
"use strict";
(() => {
  const T = window.__TAURI__;
  const invoke = T.core.invoke;
  const $ = (id) => document.getElementById(id);
  const M = window.GLMath;

  const TILE = M.TILE;
  const LEVELS = M.LEVELS;    // engine tile levels, device px per point (4 = 400 %)
  const FIT_WIDTH = 1600;     // engine FIT_WIDTH (the measured first-view configuration)
  const MAX_TILES = 32;       // raw tiles are up to 4 MiB each: hard cap, bitmap.close() on eviction
  // The engine renders one tile at a time, and on Android each pending gl:// request holds a WebView
  // request thread until it is answered (IPC to http://ipc.localhost, e.g. Cancel, needs one too).
  const MAX_INFLIGHT = 3;
  const OVERLAY_AFTER_MS = 500;
  const SETTLE_MS = 150;      // view still this long -> request tiles
  const CHROME_HIDE_MS = 3000;
  const TAP_WAIT_MS = 260;    // a single tap waits this long for a second one
  const FIT_LRU = 5;          // fit images of recently seen pages kept for instant swipe-back

  const GL = (() => {
    try {
      const u = T.core.convertFileSrc("", "gl");
      return u.endsWith("/") ? u : u + "/";
    } catch (_) {
      return /^https?:$/.test(location.protocol) ? location.protocol + "//gl.localhost/" : "gl://localhost/";
    }
  })();

  const canvas = $("canvas");
  const ctx = canvas.getContext("2d");
  const dprNow = () => window.devicePixelRatio || 1;

  const S = {
    doc: null,          // document serial from open_picked; events of other docs are dropped
    pages: 0,
    page: 0,
    openMs: 0,
    // Current page image: { bmp, rank, w, h, own }. rank 1 low-res, 1.5 a fit image remembered from an
    // earlier visit (replaced by the engine's fresh fit image), 2 fit. `own`: closed when replaced
    // (remembered fit images belong to the LRU instead).
    img: null,
    ptW: 0, ptH: 0,     // page size in points, from the engine (0 until the first image)
    view: { s: 1, ox: 0, oy: 0 }, // GLMath view: CSS px per point, page point at the top-left
    vw: 1, vh: 1,       // viewport, CSS px
    slide: 0,           // page-swipe offset along the page axis (see `axis`), CSS px (fit only)
    directPage: null,   // page the memory guard put in direct mode (gl-direct), until another page
    directReason: "",
    memNote: "",        // last gl-memory release, for the info panel
    gen: 0,             // view generation: bumped when the view settles or the page changes
    level: 0,           // tile level wanted for the settled view (0: the fit image is enough)
    waiting: false,     // show_page sent, no image yet
    showT0: 0,
    lastProgress: 0,    // performance.now() of the last gl-progress for this doc
    progressMs: 0,      // its elapsed_ms
    stopped: false,     // the user cancelled: no new tile requests until the view changes
    stats: { first: null, tiles: 0, last: null, handoffs: [], gestureFetches: 0 },
  };

  // ---------------------------------------------------------------------------------------------
  // tile cache (LRU via Map insertion order) and in-flight fetches

  const cache = new Map();     // key -> ImageBitmap
  const inflight = new Map();  // key -> { ctrl, gen }
  const failed = new Set();    // keys the engine refused (not retried until the page is shown again)

  function cachePut(key, bmp) {
    const old = cache.get(key);
    if (old) { cache.delete(key); old.close(); }
    cache.set(key, bmp);
    while (cache.size > MAX_TILES) {
      const [k, b] = cache.entries().next().value;
      cache.delete(k);
      b.close();
    }
  }

  function cacheTouch(key) {
    const b = cache.get(key);
    if (b) { cache.delete(key); cache.set(key, b); }
    return b;
  }

  function cacheClear() {
    for (const b of cache.values()) b.close();
    cache.clear();
  }

  function abortAll() {
    for (const f of inflight.values()) f.ctrl.abort();
    inflight.clear();
  }

  const tileKey = (level, tx, ty) => S.doc + "/" + S.page + "/" + level + "/" + tx + "/" + ty;

  // Fit images of recently seen pages: page -> { bmp, rank: 1.5, w, h, own: false, ptW, ptH }.
  const fitLru = new Map();

  function lruPut(page, bmp, w, h) {
    const old = fitLru.get(page);
    if (old) { fitLru.delete(page); if (old.bmp !== bmp) old.bmp.close(); }
    fitLru.set(page, { bmp, rank: 1.5, w, h, own: false, ptW: S.ptW, ptH: S.ptH });
    while (fitLru.size > FIT_LRU) {
      const [p, e] = fitLru.entries().next().value;
      fitLru.delete(p);
      if (!(S.img && S.img.bmp === e.bmp)) e.bmp.close();
      else S.img.own = true; // still on screen: closed when it is replaced
    }
  }

  function lruClear() {
    for (const e of fitLru.values()) if (!(S.img && S.img.bmp === e.bmp)) e.bmp.close();
    fitLru.clear();
  }

  function setImg(img) {
    if (S.img && S.img.own && (!img || S.img.bmp !== img.bmp)) S.img.bmp.close();
    S.img = img;
  }

  // ---------------------------------------------------------------------------------------------
  // page direction (PD.3): the axis page swipes and slides run along

  let axis = "v"; // "v": pages are stacked up/down (pref "vertical", default); "h": left/right ("sideways")

  // Read the pref (a missing GLPrefs, or a throwing one, means vertical) and update the pill glyphs
  // (CSS turns the one chevron per button by data-glyph). Called each time the viewer is shown.
  function applyDirection() {
    let a = "v";
    try { if (window.GLPrefs && window.GLPrefs.get("pageDirection") === "sideways") a = "h"; } catch (_) { /* vertical */ }
    const changed = a !== axis;
    axis = a;
    $("v_prev").dataset.glyph = a === "v" ? "up" : "left";
    $("v_next").dataset.glyph = a === "v" ? "down" : "right";
    if (changed) { stopAnim(); S.slide = 0; }
  }

  // ---------------------------------------------------------------------------------------------
  // geometry

  function sizeCanvas() {
    const r = canvas.getBoundingClientRect();
    const dpr = dprNow();
    S.vw = Math.max(1, r.width);
    S.vh = Math.max(1, r.height);
    const w = Math.max(1, Math.round(r.width * dpr));
    const h = Math.max(1, Math.round(r.height * dpr));
    if (canvas.width !== w || canvas.height !== h) {
      canvas.width = w;
      canvas.height = h;
      return true;
    }
    return false;
  }

  const hasPage = () => S.ptW > 0 && S.ptH > 0;
  const fitS = () => (hasPage() ? M.fitScale(S.ptW, S.ptH, S.vw, S.vh) : 1);
  const maxS = () => Math.max(fitS(), M.maxScale(dprNow()));
  const zoomed = () => hasPage() && M.isZoomed(S.view.s, fitS());
  const limits = () => ({ ptW: S.ptW, ptH: S.ptH, viewW: S.vw, viewH: S.vh, fit: fitS(), max: maxS() });
  const clampV = (v) => M.clampOffset({ s: M.clampScale(v.s, fitS(), maxS()), ox: v.ox, oy: v.oy }, S.ptW, S.ptH, S.vw, S.vh);
  const fitView = () => clampV({ s: fitS(), ox: 0, oy: 0 });
  // Device px per point of the current page image (the fit image is FIT_WIDTH wide).
  const imgPxPerPt = () => (S.img && hasPage() ? S.img.w / S.ptW : 0);

  // Visible tiles at `level` for the current view, nearest the centre first (the engine renders in
  // request order).
  function wantedTiles(level) {
    const v = S.view;
    const cx = (v.ox + S.vw / v.s / 2) * level, cy = (v.oy + S.vh / v.s / 2) * level;
    return M.visibleTiles(v, S.vw, S.vh, level, S.ptW, S.ptH, TILE)
      .map((t) => ({ ...t, level, key: tileKey(level, t.tx, t.ty), d: Math.hypot(t.x0 + TILE / 2 - cx, t.y0 + TILE / 2 - cy) }))
      .sort((a, b) => a.d - b.d);
  }

  // ---------------------------------------------------------------------------------------------
  // drawing

  let frame = 0;
  function redraw() {
    if (!frame) frame = requestAnimationFrame(draw);
  }

  // Draw a page image (and, for the current page, its tiles) with page points mapped through `v`,
  // shifted by `off` CSS px along the page axis (right for "h", down for "v").
  function drawPage(img, ptW, ptH, v, off, tiles) {
    const dpr = dprNow(), k = v.s * dpr;
    const dx = axis === "h" ? off : 0, dy = axis === "v" ? off : 0;
    ctx.setTransform(k, 0, 0, k, (dx - v.ox * v.s) * dpr, (dy - v.oy * v.s) * dpr);
    ctx.imageSmoothingEnabled = true;
    ctx.drawImage(img.bmp, 0, 0, ptW, ptH);
    if (!tiles) return;
    const sh = S.sharp;
    if (sh && sh.page === S.page) ctx.drawImage(sh.bmp, sh.x0 / sh.z, sh.y0 / sh.z, sh.w / sh.z, sh.h / sh.z);
    for (const L of LEVELS) {
      if (L <= imgPxPerPt()) continue; // no sharper than the fit image
      for (const t of M.visibleTiles(v, S.vw, S.vh, L, ptW, ptH, TILE)) {
        const b = cacheTouch(tileKey(L, t.tx, t.ty));
        if (b) ctx.drawImage(b, t.x0 / L, t.y0 / L, b.width / L, b.height / L);
      }
    }
  }

  function draw() {
    frame = 0;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.fillStyle = "#3a3a3a";
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    if (!S.img || !hasPage()) return;
    drawPage(S.img, S.ptW, S.ptH, S.view, S.slide, true);
    // While swiping, the neighbour slides in if its fit image is remembered.
    if (S.slide !== 0) {
      const n = S.page + (S.slide < 0 ? 1 : -1);
      const e = fitLru.get(n);
      if (e) {
        const nv = M.clampOffset({ s: M.fitScale(e.ptW, e.ptH, S.vw, S.vh), ox: 0, oy: 0 }, e.ptW, e.ptH, S.vw, S.vh);
        const extent = axis === "v" ? S.vh : S.vw;
        drawPage(e, e.ptW, e.ptH, nv, S.slide + (S.slide < 0 ? extent : -extent), false);
      }
    }
    ctx.setTransform(1, 0, 0, 1, 0, 0);
  }

  // ---------------------------------------------------------------------------------------------
  // settling and tiles

  let settleTimer = 0;
  function settle() {
    clearTimeout(settleTimer);
    settleTimer = setTimeout(requestTiles, SETTLE_MS);
    redraw();
  }

  function requestTiles() {
    if (gest || anim) return; // still moving: settle() runs again at the end
    S.gen++;
    S.stopped = false;
    S.level = 0; // 2c.2: no grid tiles; one sharp region per settled view instead
    abortAll();
    requestSharp();
    showStats();
  }

  // ---------------------------------------------------------------------------------------------
  // sharp view (task 2c.2): one render of the visible area (plus a margin) at the screen's own
  // resolution, instead of a grid of tiles. 2c.1 showed each render's time is mostly one walk of the
  // page's drawing list, so one region costs about one tile: 1.6-3x faster than six tiles on the host.
  // The previous sharp image stays on screen (scaled) until the new one lands.

  const SHARP_MARGIN = 0.12;         // extra on each side, so a small pan stays sharp
  const SHARP_MAX_PX = 8.0e6;        // ~32 MB RGBA at most per region
  const DIRECT_MAX_PX_PER_PT = 4;    // direct mode (no list) stays at the old 400 % limit
  let sharpCtrl = null;
  let regionGen = 0; // every sharp request gets a newer gen; the engine stops older ones (DS.1)

  // Abandon the sharp request in flight: the fetch, and the engine's draw of it (an aborted fetch alone
  // leaves the engine drawing a region nobody will see, and the next view waits behind it).
  function abortSharp() {
    if (!sharpCtrl) return;
    const g = sharpCtrl.glGen;
    sharpCtrl.abort();
    sharpCtrl = null;
    Promise.resolve().then(() => invoke("drop_sharp", { gen: g })).catch(() => { /* engine gone */ });
  }

  function dropSharp() {
    abortSharp();
    if (S.sharp) { S.sharp.bmp.close(); S.sharp = null; }
  }

  // The region to render for the current view, in device pixels at zoom z (px per point), or null when
  // the fit image is already sharp enough.
  function sharpRegion() {
    if (!S.img || !hasPage()) return null;
    const v = S.view, dpr = dprNow();
    let z = v.s * dpr;
    if (S.directPage === S.page) z = Math.min(z, DIRECT_MAX_PX_PER_PT);
    z = Math.min(z, M.MAX_PX_PER_PT);
    if (z <= imgPxPerPt() * 1.1) return null;
    const mx = (S.vw / v.s) * SHARP_MARGIN, my = (S.vh / v.s) * SHARP_MARGIN;
    const px0 = Math.max(0, v.ox - mx), py0 = Math.max(0, v.oy - my);
    const px1 = Math.min(S.ptW, v.ox + S.vw / v.s + mx), py1 = Math.min(S.ptH, v.oy + S.vh / v.s + my);
    if (px1 <= px0 || py1 <= py0) return null;
    // Keep the region within the pixel budget and 4096 per side (zoom drops a little if needed).
    z = Math.min(z, Math.sqrt(SHARP_MAX_PX / ((px1 - px0) * (py1 - py0))), 4096 / (px1 - px0), 4096 / (py1 - py0));
    const x0 = Math.floor(px0 * z), y0 = Math.floor(py0 * z);
    const w = Math.min(4096, Math.ceil(px1 * z) - x0), h = Math.min(4096, Math.ceil(py1 * z) - y0);
    return { z, x0, y0, w, h };
  }

  async function requestSharp() {
    abortSharp();
    if (S.doc === null || S.stopped || gest) return;
    const r = sharpRegion();
    if (!r) return;
    // Already sharp for this view (same zoom, region inside the one on screen): nothing to do.
    const cur = S.sharp;
    if (cur && cur.page === S.page && Math.abs(cur.z - r.z) < 1e-6 &&
        r.x0 >= cur.x0 && r.y0 >= cur.y0 && r.x0 + r.w <= cur.x0 + cur.w && r.y0 + r.h <= cur.y0 + cur.h) return;
    if (gest) S.stats.gestureFetches++; // must stay 0
    const ctrl = new AbortController();
    const gen = ++regionGen, doc = S.doc, page = S.page;
    ctrl.glGen = gen;
    sharpCtrl = ctrl;
    const url = GL + "tile?page=" + page + "&zoom=" + r.z + "&x0=" + r.x0 + "&y0=" + r.y0 + "&w=" + r.w + "&h=" + r.h + "&gen=" + gen;
    const t0 = performance.now();
    let bmp = null;
    try {
      const resp = await fetch(url, { signal: ctrl.signal, cache: "no-store" });
      if (resp.status === 204) return; // superseded by a newer view
      if (resp.status !== 200) {
        showMsg("Sharp view failed (" + resp.status + (resp.headers.get("X-Gl-Error") ? ", code " + resp.headers.get("X-Gl-Error") : "") + ")", false);
        return;
      }
      const w = +resp.headers.get("X-Gl-W"), h = +resp.headers.get("X-Gl-H");
      const renderMs = +resp.headers.get("X-Gl-Render-Ms"), waitMs = +resp.headers.get("X-Gl-Wait-Ms");
      const buf = await resp.arrayBuffer();
      bmp = await createImageBitmap(new ImageData(new Uint8ClampedArray(buf), w, h));
      if (ctrl.signal.aborted || doc !== S.doc || page !== S.page) return;
      if (S.sharp) S.sharp.bmp.close();
      S.sharp = { bmp, page, z: r.z, x0: r.x0, y0: r.y0, w, h };
      bmp = null;
      noteTile(renderMs, waitMs, performance.now() - t0);
      redraw();
    } catch (_) {
      // aborted by a newer view, or failed: the fit image (and any older sharp image) stays
    } finally {
      if (bmp) bmp.close();
      if (sharpCtrl === ctrl) sharpCtrl = null;
    }
  }

  function updateTiles() {
    if (S.doc === null || !S.level || !hasPage()) { abortAll(); return; }
    const want = wantedTiles(S.level);
    const keys = new Set(want.map((t) => t.key));
    // Abort what is no longer wanted. A still-wanted fetch keeps going: if it is the tile the engine
    // is rendering now it lands; if it was still queued, the newer gen makes the engine skip it (204)
    // and it is re-requested below with the current gen.
    for (const [k, f] of inflight) {
      if (!keys.has(k)) { f.ctrl.abort(); inflight.delete(k); }
    }
    if (S.stopped || gest) return;
    for (const t of want) {
      if (inflight.size >= MAX_INFLIGHT) break;
      if (!cache.has(t.key) && !inflight.has(t.key) && !failed.has(t.key)) fetchTile(t);
    }
  }

  // A fetch finished: start the next wanted tile (not on abort, which the caller already handles).
  function kick() {
    queueMicrotask(() => { if (!gest) updateTiles(); });
  }

  async function fetchTile(t) {
    if (gest) S.stats.gestureFetches++; // must stay 0: no requests while a finger is down
    const ctrl = new AbortController();
    const gen = S.gen, doc = S.doc;
    inflight.set(t.key, { ctrl, gen });
    const url = GL + "tile?page=" + S.page + "&zoom=" + t.level + "&x0=" + t.x0 + "&y0=" + t.y0 + "&gen=" + gen;
    const t0 = performance.now();
    let bmp = null;
    try {
      const r = await fetch(url, { signal: ctrl.signal, cache: "no-store" });
      // 204: skipped, superseded by a newer gen (kick() re-requests it if still wanted) or stopped
      // by a cancel (S.stopped keeps it from being re-requested).
      if (r.status === 204) return;
      if (r.status !== 200) {
        if (doc === S.doc) failed.add(t.key);
        showMsg("Tile failed (" + r.status + (r.headers.get("X-Gl-Error") ? ", code " + r.headers.get("X-Gl-Error") : "") + ")", false);
        return;
      }
      const w = +r.headers.get("X-Gl-W"), h = +r.headers.get("X-Gl-H");
      const renderMs = +r.headers.get("X-Gl-Render-Ms"), waitMs = +r.headers.get("X-Gl-Wait-Ms");
      const buf = await r.arrayBuffer();
      bmp = await createImageBitmap(new ImageData(new Uint8ClampedArray(buf), w, h));
      const total = performance.now() - t0;
      if (doc !== S.doc || inflight.get(t.key)?.ctrl !== ctrl) return; // view moved on
      inflight.delete(t.key);
      cachePut(t.key, bmp);
      bmp = null;
      noteTile(renderMs, waitMs, total);
      redraw();
    } catch (_) {
      // AbortError on a view change; anything else just leaves the tile missing.
    } finally {
      if (bmp) bmp.close();
      if (inflight.get(t.key)?.ctrl === ctrl) inflight.delete(t.key);
      if (!ctrl.signal.aborted && doc === S.doc) kick();
    }
  }

  function noteTile(renderMs, waitMs, total) {
    const st = S.stats;
    const handoff = Math.max(0, total - waitMs);
    st.tiles++;
    st.last = { renderMs, waitMs, total, handoff };
    st.handoffs.push(handoff);
    if (st.handoffs.length > 200) st.handoffs.shift();
    showStats();
  }

  function p95(a) {
    if (!a.length) return 0;
    const s = [...a].sort((x, y) => x - y);
    return s[Math.min(s.length - 1, Math.floor(s.length * 0.95))];
  }

  function showStats() {
    const st = S.stats, f = (v) => v.toFixed(0);
    let s = "first view " + (st.first === null ? "–" : (st.first / 1000).toFixed(2) + " s") +
      " (open " + f(S.openMs) + " ms)";
    // 100 % = 1 device px per point (1600 % is the top).
    const sh = S.sharp && S.sharp.page === S.page ? S.sharp : null;
    s += "\nzoom " + Math.round(S.view.s * dprNow() * 100) + " %, sharp image " +
      (sh ? sh.w + "x" + sh.h + " at " + Math.round(sh.z * 100) + " %" : "none") + (sharpCtrl ? " (drawing…)" : "");
    if (st.last) {
      const l = st.last;
      s += "\nsharp #" + st.tiles + ": render " + f(l.renderMs) + " ms, engine wait " + f(l.waitMs) +
        " ms, handoff " + f(l.handoff) + " ms, total " + f(l.total) + " ms";
    }
    s += "\nrequests during gestures " + st.gestureFetches;
    if (S.memNote) s += "\n" + S.memNote;
    $("stats").textContent = s;
  }

  // ---------------------------------------------------------------------------------------------
  // page images (low-res, then fit)

  async function fetchImage(ev) {
    const doc = S.doc, page = S.page, rank = ev.kind === "fit" ? 2 : 1;
    try {
      const r = await fetch(GL + "img?id=" + ev.id, { cache: "no-store" });
      if (r.status !== 200) return;
      const w = +r.headers.get("X-Gl-W"), h = +r.headers.get("X-Gl-H");
      const buf = await r.arrayBuffer();
      const bmp = await createImageBitmap(new ImageData(new Uint8ClampedArray(buf), w, h));
      if (doc !== S.doc || page !== S.page || (S.img && S.img.rank >= rank)) { bmp.close(); return; }
      const first = !S.img;
      if (ev.pt_w > 0 && ev.pt_h > 0) { S.ptW = ev.pt_w; S.ptH = ev.pt_h; }
      if (rank === 2) {
        lruPut(page, bmp, w, h);
        setImg({ bmp, rank: 2, w, h, own: false });
        saveThumbOnce(bmp, w, h);
      } else {
        setImg({ bmp, rank, w, h, own: true });
      }
      if (first) S.view = fitView();
      if (S.stats.first === null) S.stats.first = performance.now() - S.showT0;
      S.waiting = false;
      updateOverlay();
      showStats();
      settle();
    } catch (_) {
      // leave the previous image
    }
  }

  // ---------------------------------------------------------------------------------------------
  // recent-file bookkeeping (task 2bf.15): the start screen shows a thumbnail and reopens at the last
  // page. Only the random recent id and numbers go out; the picture stays on the phone.

  let thumbSaved = false;
  function saveThumbOnce(bmp, w, h) {
    if (thumbSaved || !S.recentId) return;
    thumbSaved = true;
    try {
      const tw = 160, th = Math.max(1, Math.round(h * tw / w));
      const c = document.createElement("canvas");
      c.width = tw; c.height = Math.min(th, 240);
      const g = c.getContext("2d");
      g.fillStyle = "#fff";
      g.fillRect(0, 0, c.width, c.height);
      g.drawImage(bmp, 0, 0, tw, th);
      const data = c.toDataURL("image/jpeg", 0.6);
      if (data.length < 38000) invoke("save_thumb", { id: S.recentId, data }).catch(() => {});
    } catch (_) {
      // no thumbnail: the start screen shows a placeholder
    }
  }

  function saveLastPage() {
    if (!S.recentId || S.doc === null) return;
    invoke("set_last_page", { id: S.recentId, page: S.page, pages: S.pages }).catch(() => {});
  }

  // ---------------------------------------------------------------------------------------------
  // chrome: toolbar, page pill, info panel (all hide together; a tap brings them back)

  let chromeTimer = 0;
  function showChrome(show) {
    clearTimeout(chromeTimer);
    $("viewer").classList.toggle("bare", !show);
    if (show && !$("vinfo").classList.contains("open") && $("jump").hidden) {
      chromeTimer = setTimeout(() => showChrome(false), CHROME_HIDE_MS);
    }
  }
  const chromeShown = () => !$("viewer").classList.contains("bare");

  function updatePill() {
    $("pageno").textContent = (S.page + 1) + " / " + S.pages;
    $("v_prev").disabled = S.page <= 0;
    $("v_next").disabled = S.page >= S.pages - 1;
  }

  const REASONS = { list_guard: "list", soft_limit: "soft", trim: "trim" };
  function updateDirectBadge() {
    const on = S.doc !== null && S.directPage === S.page;
    $("v_direct").hidden = !on;
    if (on) $("v_direct").textContent = "direct: " + (REASONS[S.directReason] || S.directReason || "?");
  }

  // ---------------------------------------------------------------------------------------------
  // progress overlay and messages

  function updateOverlay() {
    const now = performance.now();
    const recent = now - S.lastProgress < 600;
    const show = S.doc !== null && ((S.waiting && now - S.showT0 >= OVERLAY_AFTER_MS) || (recent && S.progressMs >= OVERLAY_AFTER_MS));
    $("overlay").hidden = !show;
    if (show) {
      const ms = recent ? S.progressMs : now - S.showT0;
      $("ov_secs").textContent = Math.floor(ms / 1000) + " s";
    }
  }
  setInterval(updateOverlay, 250);

  function showMsg(text, retry) {
    $("vmsg_t").textContent = text;
    $("vmsg_retry").hidden = !retry;
    $("vmsg").hidden = false;
  }

  // ---------------------------------------------------------------------------------------------
  // engine events (numbers only)

  T.event.listen("gl-progress", (e) => {
    const p = e.payload;
    if (p.doc !== S.doc || p.page !== S.page) return;
    S.lastProgress = performance.now();
    S.progressMs = p.elapsed_ms;
    $("ov_what").textContent = p.stage === "read" ? "Reading page…" : "Rendering page…";
    updateOverlay();
  });
  T.event.listen("gl-image", (e) => {
    const p = e.payload;
    if (p.doc === S.doc && p.page === S.page) fetchImage(p);
  });
  T.event.listen("gl-error", (e) => {
    const p = e.payload;
    if (p.doc !== S.doc) return;
    S.waiting = false;
    updateOverlay();
    showMsg("Engine error (code " + p.code + ")", true);
  });
  T.event.listen("gl-cancelled", (e) => {
    if (e.payload.doc !== S.doc) return;
    updateOverlay();
  });
  // Memory guard: the page is drawn without a display list (slower, far less memory) until another
  // page is shown. The badge names the reason (list guard, soft limit or trim).
  T.event.listen("gl-direct", (e) => {
    const p = e.payload;
    if (p.doc !== S.doc) return;
    S.directPage = p.page;
    S.directReason = p.reason;
    updateDirectBadge();
  });
  T.event.listen("gl-memory", (e) => {
    const p = e.payload;
    if (p.doc !== S.doc) return;
    S.memNote = "memory released (" + p.reason + "), RSS " + p.rss_mb.toFixed(0) + " MB";
    showStats();
  });

  // ---------------------------------------------------------------------------------------------
  // commands

  async function showPage(page) {
    if (S.doc === null) return;
    abortAll();
    dropSharp();
    failed.clear();
    stopAnim();
    S.page = page;
    S.gen++;
    S.level = 0;
    S.slide = 0;
    const hit = fitLru.get(page);
    if (hit) {
      // Seen recently: show the remembered fit image at once; the engine's fresh one replaces it.
      fitLru.delete(page); fitLru.set(page, hit);
      setImg(hit);
      S.ptW = hit.ptW; S.ptH = hit.ptH;
      S.view = fitView();
    } else {
      setImg(null);
      S.ptW = S.ptH = 0;
    }
    if (S.directPage !== page) { S.directPage = null; S.directReason = ""; } // the engine leaves direct mode on another page
    S.stopped = false;
    S.waiting = !hit;
    S.showT0 = performance.now();
    S.lastProgress = 0;
    S.stats.first = null;
    $("vmsg").hidden = true;
    saveLastPage();
    updatePill();
    updateDirectBadge();
    showChrome(true);
    redraw();
    showStats();
    try {
      await invoke("show_page", { page, fitWidth: FIT_WIDTH });
    } catch (_) {
      S.waiting = false;
      showMsg("Engine not available", true);
    }
  }

  async function cancel() {
    S.waiting = false;
    S.stopped = true;
    S.lastProgress = 0;
    S.gen++;
    abortAll();
    if (sharpCtrl) { sharpCtrl.abort(); sharpCtrl = null; }
    updateOverlay();
    try { await invoke("cancel"); } catch (_) { /* engine gone */ }
    if (!S.img) showMsg("Cancelled", true);
  }

  // ---------------------------------------------------------------------------------------------
  // animation (double-tap zoom, page slide, spring-back)

  let anim = null; // { raf }
  let pendingPage = null; // page a running slide-out will show (cleared by stopAnim / showPage)
  function stopAnim() {
    if (anim) { cancelAnimationFrame(anim.raf); anim = null; }
    pendingPage = null;
  }

  // Animate the view to `to` (and the slide to `slideTo`) over `ms`, then run `done`.
  function animate(to, slideTo, ms, done) {
    stopAnim();
    const from = { ...S.view }, slide0 = S.slide, t0 = performance.now();
    const step = (now) => {
      const k = Math.min(1, (now - t0) / ms), e = 1 - Math.pow(1 - k, 3);
      if (to) {
        S.view = {
          s: from.s * Math.pow(to.s / from.s, e),
          ox: from.ox + (to.ox - from.ox) * e,
          oy: from.oy + (to.oy - from.oy) * e,
        };
      }
      S.slide = slide0 + (slideTo - slide0) * e;
      draw();
      if (k < 1) { anim.raf = requestAnimationFrame(step); return; }
      anim = null;
      if (done) done(); else settle();
    };
    anim = { raf: requestAnimationFrame(step) };
  }

  function zoomToggleAt(x, y) {
    if (!S.img || !hasPage()) return;
    // Double-tap goes to 400 % (or twice the fit, whichever is closer in); pinch reaches 1600 %.
    const target = Math.min(maxS(), Math.max(fitS() * 2, 4 / dprNow()));
    const to = zoomed() ? fitView() : clampV(M.zoomAt(S.view, target, x, y));
    animate(to, 0, 220);
  }

  function goFit() {
    if (hasPage()) animate(fitView(), 0, 200);
  }

  // Slide the page out along the axis (dir -1: left / up, towards the next page; +1: right / down,
  // towards the previous) and then show page `next` (default: the neighbour in that direction).
  function slidePage(dir, next) {
    if (next === undefined) next = S.page - dir;
    animate(null, dir * (axis === "v" ? S.vh : S.vw), 160, () => showPage(next));
    pendingPage = next; // after animate(): it stops the previous animation, which clears this
  }

  // Pill buttons and jump-to-page: slide to page `n` along the axis. Fast repeated taps add up (they
  // count from the page the running slide will show). Zoomed in, or before the page has an image,
  // there is nothing to slide: show it at once.
  const pageBase = () => (pendingPage === null ? S.page : pendingPage);
  function goToPage(n) {
    if (S.doc === null || n < 0 || n >= S.pages || n === pageBase()) return;
    if (n === S.page) { animate(null, 0, 120); return; } // tapped back mid-slide: return to this page
    if (!S.img || !hasPage() || zoomed()) { showPage(n); return; }
    slidePage(n > S.page ? -1 : 1, n);
  }

  // ---------------------------------------------------------------------------------------------
  // touch: one gesture runs from the first finger down to the last finger up

  const ptrs = new Map();  // pointerId -> { x, y } (viewport CSS px)
  let gest = null;         // { samples, zoomed }
  let lastTap = null;      // { t, x, y } end of the last single tap
  let tapTimer = 0;

  function pt(e) {
    const r = canvas.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  }

  canvas.addEventListener("pointerdown", (e) => {
    if (S.doc === null) return;
    canvas.setPointerCapture(e.pointerId);
    const p = pt(e);
    if (!gest) {
      gest = { samples: [], zoomed: zoomed() };
      stopAnim();
      clearTimeout(settleTimer);
      if (tapTimer) { clearTimeout(tapTimer); tapTimer = 0; } // a second tap is on its way
      if (chromeShown()) showChrome(false);
    }
    ptrs.set(e.pointerId, p);
    gest.samples.push({ t: performance.now(), id: e.pointerId, x: p.x, y: p.y, type: "down" });
  });

  canvas.addEventListener("pointermove", (e) => {
    const prev = ptrs.get(e.pointerId);
    if (!prev || !gest) return;
    const p = pt(e);
    if (gest.samples.length < 4000) gest.samples.push({ t: performance.now(), id: e.pointerId, x: p.x, y: p.y, type: "move" });
    if (!hasPage() || !S.img) { ptrs.set(e.pointerId, p); return; }
    if (ptrs.size >= 2) {
      // Pinch with the first two fingers; the page point between them stays under them.
      const [ia, ib] = [...ptrs.keys()];
      const oa = ptrs.get(ia), ob = ptrs.get(ib);
      const na = ia === e.pointerId ? p : oa, nb = ib === e.pointerId ? p : ob;
      if (ia === e.pointerId || ib === e.pointerId) {
        S.view = M.pinchClamped(S.view, oa, ob, na, nb, limits());
        gest.pinched = true;
        S.slide = 0;
      }
    } else {
      const dx = p.x - prev.x, dy = p.y - prev.y;
      if (gest.pinched || gest.zoomed || zoomed()) {
        S.view = clampV({ s: S.view.s, ox: S.view.ox - dx / S.view.s, oy: S.view.oy - dy / S.view.s });
      } else {
        S.slide += axis === "v" ? dy : dx; // at fit the page follows the finger along the axis only
      }
    }
    ptrs.set(e.pointerId, p);
    redraw();
  });

  function pointerEnd(e) {
    if (!ptrs.has(e.pointerId) || !gest) return;
    const p = pt(e);
    gest.samples.push({ t: performance.now(), id: e.pointerId, x: p.x, y: p.y, type: "up" });
    ptrs.delete(e.pointerId);
    if (ptrs.size > 0) return;
    const g = gest;
    gest = null;
    const now = performance.now();
    const kind = e.type === "pointercancel" ? "pan" : M.classify(g.samples, {
      zoomed: g.zoomed,
      axis,
      lastTapT: lastTap ? lastTap.t : null,
      lastTapX: lastTap ? lastTap.x : 0,
      lastTapY: lastTap ? lastTap.y : 0,
    });
    lastTap = kind === "tap" ? { t: now, x: p.x, y: p.y } : null;
    if (kind === "tap") {
      tapTimer = setTimeout(() => { tapTimer = 0; showChrome(!chromeShown()); }, TAP_WAIT_MS);
      if (S.slide) animate(null, 0, 120);
    } else if (kind === "double-tap") {
      zoomToggleAt(p.x, p.y);
    } else if ((kind === "swipe-left" || kind === "swipe-up") && S.page < S.pages - 1) {
      slidePage(-1); // classify only returns the pair that belongs to the axis
    } else if ((kind === "swipe-right" || kind === "swipe-down") && S.page > 0) {
      slidePage(1);
    } else if (S.slide) {
      animate(null, 0, 160); // not far or fast enough (or no page there): spring back
    } else {
      settle();
    }
  }
  canvas.addEventListener("pointerup", pointerEnd);
  canvas.addEventListener("pointercancel", pointerEnd);
  // A pointer the canvas loses without an up/cancel (the viewer hid, the system took the gesture)
  // must not stay "down" (2bf review finding 5): end it like a cancel.
  canvas.addEventListener("lostpointercapture", (e) => {
    if (ptrs.has(e.pointerId)) pointerEnd(new PointerEvent("pointercancel", { pointerId: e.pointerId, clientX: e.clientX, clientY: e.clientY }));
  });

  function resetTouch() {
    ptrs.clear();
    gest = null;
    lastTap = null;
    if (tapTimer) { clearTimeout(tapTimer); tapTimer = 0; }
    S.slide = 0;
  }
  canvas.addEventListener("contextmenu", (e) => e.preventDefault());

  // ---------------------------------------------------------------------------------------------
  // viewer in and out, Android Back (history: zoomed -> fit -> open screen)

  let backArmed = false;
  const exitHandlers = [];
  function exited() {
    for (const f of exitHandlers) { try { f(); } catch (_) { /* start screen's problem */ } }
  }

  function enterViewer(show) {
    $("viewer").hidden = !show;
    if (show) {
      applyDirection();
      sizeCanvas();
      if (hasPage()) S.view = clampV(S.view);
      showChrome(true);
      redraw();
      if (!backArmed) { history.pushState({ glViewer: 1 }, ""); backArmed = true; }
    } else {
      abortAll();
      resetTouch();
      if (backArmed) { backArmed = false; history.back(); }
      exited();
    }
  }

  window.addEventListener("popstate", () => {
    if (!backArmed) return;
    backArmed = false;
    if ($("viewer").hidden) return;
    if (zoomed()) {
      goFit();
      history.pushState({ glViewer: 1 }, "");
      backArmed = true;
    } else {
      abortAll();
      resetTouch();
      $("viewer").hidden = true;
      exited();
    }
  });

  function closeLocal() {
    abortAll();
    dropSharp();
    cacheClear();
    stopAnim();
    setImg(null);
    lruClear();
    S.doc = null;
    S.waiting = false;
    S.directPage = null;
    S.memNote = "";
    S.gen++;
    updateOverlay();
    updateDirectBadge();
  }

  // ---------------------------------------------------------------------------------------------
  // open / close (open_picked returns numbers and the provider authority only)

  const num = (v, unit) => (typeof v === "number" ? v.toFixed(1) + " " + unit : "–");
  function showOpen(r, code) {
    $("o_code").textContent = code;
    $("o_pages").textContent = r.pages ?? "–";
    $("o_seek").textContent = r.seekable == null ? "–" : (r.seekable ? "yes" : "no");
    $("o_kind").textContent = r.kind ?? "–";
    $("o_size").textContent = num(r.size_mb, "MB");
    $("o_pick").textContent = num(r.pick_to_fd_ms, "ms");
    $("o_open").textContent = num(r.open_ms, "ms");
    $("o_auth").textContent = r.authority || "–";
    $("openout").hidden = false;
  }

  // Open a document and show it. source: { kind: "picker" } | { kind: "recent", id } | { kind: "incoming" }.
  // Resolves to the OpenInfo (numbers, fixed codes, recent_id) or null; failures resolve to
  // { error: code } (E_PICK_CANCELLED and E_NONE are quiet).
  let opening = false;
  async function openFrom(source, startPage) {
    if (opening) return null;
    opening = true;
    $("v_open").disabled = true;
    try {
      const cmd = source.kind === "recent" ? "open_recent" : source.kind === "incoming" ? "open_incoming" : "open_picked";
      const r = await invoke(cmd, source.kind === "recent" ? { id: source.id } : {});
      if (!r || typeof r.doc !== "number" || !(r.pages > 0)) throw { code: "E_BAD_REPLY" };
      closeLocal();
      S.doc = r.doc;
      S.pages = r.pages;
      S.openMs = r.open_ms;
      S.recentId = r.recent_id || null;
      S.incoming = !!r.incoming;
      thumbSaved = false;
      S.stats = { first: null, tiles: 0, last: null, handoffs: [], gestureFetches: 0 };
      showOpen(r, "OK");
      enterViewer(true);
      const p = Number.isInteger(startPage) ? Math.min(Math.max(startPage, 0), r.pages - 1) : 0;
      showPage(p);
      return r;
    } catch (e) {
      const f = (e && typeof e === "object") ? e : { code: String(e) };
      // These two mean the engine had already let go of the document on screen (2bf review finding 1):
      // don't leave a dead page up.
      if ((f.code === "E_OPEN_FAILED" || f.code === "E_ENGINE") && S.doc !== null) {
        closeLocal();
        enterViewer(false);
      }
      if (f.code !== "E_PICK_CANCELLED" && f.code !== "E_NONE") {
        showOpen(f, f.code);
        if (!$("viewer").hidden) showMsg("Could not open (" + f.code + ")", false);
      }
      return { error: f.code };
    } finally {
      opening = false;
      $("v_open").disabled = false;
      // A file another app sent while this open ran was not taken: look again (finding 6).
      if (source.kind !== "incoming" && window.GLStart && window.GLStart.checkIncoming) {
        setTimeout(() => window.GLStart.checkIncoming(), 0);
      }
    }
  }

  async function closeDoc() {
    closeLocal();
    enterViewer(false);
    $("o_code").textContent = "closed";
    try { await invoke("close_doc"); } catch (_) { /* engine gone */ }
  }

  // The start screen drives the viewer through this (start.js).
  window.GLViewer = {
    open: openFrom,
    close: closeDoc,
    // Back into the viewer for the document that is still open.
    resume: () => { if (S.doc !== null) { enterViewer(true); settle(); return true; } return false; },
    isOpen: () => S.doc !== null,
    current: () => (S.doc === null ? null : { page: S.page, pages: S.pages, recentId: S.recentId, incoming: S.incoming }),
    onExit: (f) => exitHandlers.push(f),
    // After the first Back to the sending app, later exits stay in Greenline (2bf review finding 4).
    clearIncoming: () => { S.incoming = false; },
  };

  function openJump() {
    $("jump_n").value = "";
    $("jump_n").max = String(S.pages);
    $("jump").hidden = false;
    showChrome(true);
    $("jump_n").focus();
  }
  function closeJump() {
    $("jump").hidden = true;
    $("jump_n").blur();
    showChrome(true);
  }

  $("v_open").addEventListener("click", () => openFrom({ kind: "picker" }));
  $("v_back").addEventListener("click", () => enterViewer(false));
  $("v_prev").addEventListener("click", () => goToPage(pageBase() - 1));
  $("v_next").addEventListener("click", () => goToPage(pageBase() + 1));
  $("v_fit").addEventListener("click", () => { goFit(); showChrome(true); });
  $("v_info").addEventListener("click", () => { $("vinfo").classList.toggle("open"); showStats(); showChrome(true); });
  $("pageno").addEventListener("click", openJump);
  $("jump").addEventListener("submit", (e) => {
    e.preventDefault();
    const n = Math.round(+$("jump_n").value);
    closeJump();
    if (n >= 1 && n <= S.pages) goToPage(n - 1);
  });
  $("jump_cancel").addEventListener("click", closeJump);
  $("ov_cancel").addEventListener("click", cancel);
  $("vmsg_retry").addEventListener("click", () => showPage(S.page));
  $("vmsg_close").addEventListener("click", () => { $("vmsg").hidden = true; });
  // Any touch on the chrome keeps it up a little longer.
  for (const id of ["vbar", "vpill", "vinfo"]) $(id).addEventListener("pointerdown", () => showChrome(true));

  new ResizeObserver(() => {
    if ($("viewer").hidden) return;
    const wasZoomed = zoomed();
    const cx = S.view.ox + S.vw / S.view.s / 2, cy = S.view.oy + S.vh / S.view.s / 2;
    if (!sizeCanvas()) return;
    if (hasPage()) {
      if (!wasZoomed) S.view = fitView();
      else S.view = clampV({ s: S.view.s, ox: cx - S.vw / S.view.s / 2, oy: cy - S.vh / S.view.s / 2 });
    }
    settle();
  }).observe(canvas);
})();
