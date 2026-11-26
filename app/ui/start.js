// Greenline start screen (tasks 2bf.13 / 2bf.14).
// Shows Continue, the Recent list, the Open PDF button, the default-app card, the Settings sheet and
// the About sheet. Preferences live in window.GLPrefs (prefs.js).
// It drives the viewer through window.GLViewer (viewer.js) and talks to the app through
// window.__TAURI__.core.invoke. File names are display-only: this file never logs anything.
(function () {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const MAX_RECENTS = 20;
  const DISMISS_KEY = "gl.defaultCard.dismissed";
  const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  const PLACEHOLDER_SVG =
    '<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">' +
    '<path fill="currentColor" fill-rule="evenodd" d="M6 2h8l5 5v15H6V2zm7 1.5V8h4.5L13 3.5z"/>' +
    '<rect x="8.5" y="11" width="7" height="1.6" rx=".8" fill="currentColor"/>' +
    '<rect x="8.5" y="14.5" width="7" height="1.6" rx=".8" fill="currentColor"/>' +
    '<rect x="8.5" y="18" width="4.5" height="1.6" rx=".8" fill="currentColor"/></svg>';

  let recents = [];
  let refreshSeq = 0;
  let dismissedMem = false;
  let incomingBusy = false;
  let sheetOpen = null; // which sheet is open: "about", "settings" or null (only one at a time)
  let sheetPushed = false;
  let menuAnchor = null;

  // --- app and viewer access (looked up on every call so fakes and late loads work) -------------

  function invoke(cmd, args) {
    try {
      const t = window.__TAURI__;
      if (!t || !t.core || typeof t.core.invoke !== "function") return Promise.reject(new Error("no app"));
      return Promise.resolve(t.core.invoke(cmd, args));
    } catch (e) {
      return Promise.reject(e);
    }
  }
  const viewer = () => window.GLViewer || null;
  function viewerCall(name, arg) {
    try {
      const v = viewer();
      return v && typeof v[name] === "function" ? v[name](arg) : undefined;
    } catch (_) {
      return undefined;
    }
  }

  // --- small helpers ------------------------------------------------------------------------------

  const pad2 = (n) => (n < 10 ? "0" : "") + n;

  // "Today 14:05", "Yesterday", "12 Nov" (with the year when it is not this year).
  function formatDate(ms, now) {
    if (typeof ms !== "number" || !isFinite(ms)) return "";
    const d = new Date(ms);
    const n = new Date(now === undefined ? Date.now() : now);
    const day = (x) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
    const diff = Math.round((day(n) - day(d)) / 86400000);
    if (diff === 0) return "Today " + pad2(d.getHours()) + ":" + pad2(d.getMinutes());
    if (diff === 1) return "Yesterday";
    return d.getDate() + " " + MONTHS[d.getMonth()] + (d.getFullYear() !== n.getFullYear() ? " " + d.getFullYear() : "");
  }

  const isInt = (v) => typeof v === "number" && isFinite(v) && Math.floor(v) === v;

  function pageText(page, pages) {
    if (isInt(page) && isInt(pages) && pages > 0) return "Page " + (page + 1) + " of " + pages;
    if (isInt(pages) && pages > 0) return pages + (pages === 1 ? " page" : " pages");
    return "";
  }

  function makeEl(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = text;
    return e;
  }

  function makeThumb(src) {
    const t = makeEl("span", "thumb");
    if (typeof src === "string" && src.indexOf("data:image/") === 0) {
      const img = document.createElement("img");
      img.alt = "";
      // A preview that fails to load shows the placeholder, never a broken-image icon.
      img.addEventListener("error", () => { t.textContent = ""; t.innerHTML = PLACEHOLDER_SVG; }, { once: true });
      img.src = src;
      t.appendChild(img);
    } else {
      t.innerHTML = PLACEHOLDER_SVG; // constant markup, never file data
    }
    return t;
  }

  // The tappable body of a card or row: thumbnail, name (one line), then a small line under it.
  function makePick(thumb, name, meta) {
    const b = makeEl("button", "pick");
    b.type = "button";
    b.appendChild(makeThumb(thumb));
    const txt = makeEl("span", "txt");
    txt.appendChild(makeEl("span", "nm", name));
    if (meta) txt.appendChild(makeEl("span", "meta", meta));
    b.appendChild(txt);
    return b;
  }

  // --- messages (friendly one-liners) ---------------------------------------------------------------

  function hideMsg() {
    $("smsg").hidden = true;
  }
  function showMsg(text, action) {
    $("smsg_t").textContent = text;
    const a = $("smsg_act");
    if (action) {
      a.textContent = action.label;
      a.hidden = false;
      a.onclick = () => { hideMsg(); action.run(); };
    } else {
      a.hidden = true;
      a.onclick = null;
    }
    $("smsg").hidden = false;
  }

  // Shows the result of GLViewer.open. rec is the recent entry that was tapped (if any).
  function handleResult(r, rec) {
    if (!r) return; // the viewer was busy
    if (!r.error) {
      hideMsg();
      closeMenu();
      return;
    }
    const code = r.error;
    if (code === "E_PICK_CANCELLED" || code === "E_NONE") return;
    if (code === "E_NO_PERMISSION") {
      showMsg("This file can't be reopened from here any more. Open it again from where it is stored.",
        rec ? { label: "Remove from list", run: () => removeRecent(rec) } : null);
    } else if (code === "E_NOT_SEEKABLE" || code === "E_PICK_OPEN_FAILED" || code === "E_OPEN_FAILED") {
      showMsg("Couldn't open this PDF.", null);
    } else {
      showMsg("Couldn't open this PDF (code " + String(code) + ").", null);
    }
  }

  // --- opening ----------------------------------------------------------------------------------------

  async function openSource(source, startPage, rec) {
    closeMenu();
    let r = null;
    try {
      const v = viewer();
      if (!v || typeof v.open !== "function") return;
      r = await v.open(source, startPage);
    } catch (_) {
      r = { error: "E_OPEN_FAILED" };
    }
    handleResult(r, rec || null);
  }

  const openRecent = (rec) =>
    openSource({ kind: "recent", id: rec.id }, isInt(rec.last_page) ? rec.last_page : 0, rec);

  function openPicker() {
    return openSource({ kind: "picker" });
  }

  // Something may be waiting from another app (Files, Gmail, Share). E_NONE is the normal answer.
  async function checkIncoming() {
    if (incomingBusy) return;
    const v = viewer();
    if (!v || typeof v.open !== "function") return;
    incomingBusy = true;
    try {
      handleResult(await v.open({ kind: "incoming" }), null);
    } catch (_) {
      /* nothing to show */
    } finally {
      incomingBusy = false;
    }
  }

  function resume() {
    closeMenu();
    if (viewerCall("resume") === false) refresh();
  }

  // --- recents ------------------------------------------------------------------------------------------

  function cleanRecents(list) {
    const out = [];
    if (!Array.isArray(list)) return out;
    for (const r of list) {
      if (!r || typeof r !== "object" || typeof r.id !== "string" || !r.id) continue;
      out.push({
        id: r.id,
        name: typeof r.name === "string" && r.name ? r.name : "PDF document",
        pages: isInt(r.pages) ? r.pages : null,
        last_page: isInt(r.last_page) ? r.last_page : null,
        opened: typeof r.opened === "number" ? r.opened : null,
        thumb: typeof r.thumb === "string" ? r.thumb : null,
      });
      if (out.length >= MAX_RECENTS) break;
    }
    return out;
  }

  async function removeRecent(rec) {
    closeMenu();
    try {
      await invoke("remove_recent", { id: rec.id });
    } catch (_) {
      showMsg("Couldn't remove this file from the list.", null);
      return;
    }
    recents = recents.filter((x) => x.id !== rec.id);
    render();
  }

  function rowMeta(rec) {
    const parts = [formatDate(rec.opened), pageText(rec.last_page, rec.pages)].filter(Boolean);
    return parts.join(" · ");
  }

  function render() {
    const open = !!viewerCall("isOpen");
    const cur = open ? viewerCall("current") : null;
    const cont = $("cont");
    cont.textContent = "";
    let showCont = false;
    if (open && cur) {
      const rec = recents.find((r) => r.id === cur.recentId) || null;
      const b = makePick(rec && rec.thumb, rec ? rec.name : "PDF document", pageText(cur.page, cur.pages));
      b.onclick = resume;
      cont.appendChild(b);
      showCont = true;
    } else if (recents.length) {
      const rec = recents[0];
      const b = makePick(rec.thumb, rec.name, pageText(rec.last_page, rec.pages));
      b.onclick = () => openRecent(rec);
      cont.appendChild(b);
      showCont = true;
    }
    $("contsec").hidden = !showCont;

    const list = $("rlist");
    list.textContent = "";
    for (const rec of recents) {
      const li = makeEl("li", "row");
      li.dataset.id = rec.id;
      const b = makePick(rec.thumb, rec.name, rowMeta(rec));
      b.onclick = () => openRecent(rec);
      const m = makeEl("button", "iconbtn", "⋮");
      m.type = "button";
      m.setAttribute("aria-label", "Options for " + rec.name);
      m.setAttribute("aria-haspopup", "menu");
      m.onclick = () => openMenu(m, [{ label: "Remove from list", run: () => removeRecent(rec) }]);
      li.appendChild(b);
      li.appendChild(m);
      list.appendChild(li);
    }
    $("recsec").hidden = recents.length === 0;
    $("empty").hidden = recents.length > 0 || showCont;
  }

  async function refreshDefaultCard() {
    let show = false;
    try {
      const r = await invoke("default_pdf_app");
      show = !!r && r.is_default === false && !isDismissed();
    } catch (_) {
      show = false;
    }
    $("defcard").hidden = !show;
  }

  function isDismissed() {
    if (dismissedMem) return true;
    try { return window.localStorage.getItem(DISMISS_KEY) === "1"; } catch (_) { return false; }
  }
  function dismissDefaultCard() {
    dismissedMem = true;
    try { window.localStorage.setItem(DISMISS_KEY, "1"); } catch (_) { /* remembered for this run only */ }
    $("defcard").hidden = true;
  }

  async function refresh() {
    const seq = ++refreshSeq;
    let list = [];
    try {
      list = cleanRecents(await invoke("list_recents"));
    } catch (_) {
      list = [];
    }
    if (seq === refreshSeq) {
      recents = list;
      render();
    }
    await refreshDefaultCard();
  }

  // --- popup menu ---------------------------------------------------------------------------------------

  function openMenu(anchor, items) {
    const menu = $("menu");
    menu.textContent = "";
    for (const it of items) {
      const b = makeEl("button", "", it.label);
      b.type = "button";
      b.setAttribute("role", "menuitem");
      b.onclick = () => { closeMenu(); it.run(); };
      menu.appendChild(b);
    }
    menuAnchor = anchor;
    $("menu_back").hidden = false;
    menu.hidden = false;
    const a = anchor.getBoundingClientRect();
    const w = menu.offsetWidth, h = menu.offsetHeight;
    const vw = document.documentElement.clientWidth, vh = window.innerHeight;
    let left = Math.min(Math.max(a.right - w, 8), Math.max(8, vw - w - 8));
    let top = a.bottom + 4;
    if (top + h > vh - 8) top = Math.max(8, a.top - h - 4);
    menu.style.left = left + "px";
    menu.style.top = top + "px";
    const first = menu.querySelector("button");
    if (first) first.focus();
  }

  function closeMenu() {
    if ($("menu").hidden) return;
    $("menu").hidden = true;
    $("menu_back").hidden = true;
    const a = menuAnchor;
    menuAnchor = null;
    try { if (a && a.isConnected) a.focus(); } catch (_) { /* ignore */ }
  }

  // --- About sheet ----------------------------------------------------------------------------------------

  function mupdfText(i) {
    return "MuPDF " + i.mupdf_version + (i.mupdf_version_ok ? " (verified)" : " (version check FAILED)");
  }

  // The app version comes from the app itself (engine_info.app_version, the Cargo package version), so it
  // cannot go stale in the HTML. Only a plain version token is shown; anything else gets the fixed fallback.
  function versionText(i) {
    const v = i && i.app_version;
    return typeof v === "string" && /^[0-9A-Za-z][0-9A-Za-z.+-]{0,31}$/.test(v) ? "Version " + v : "Version not available";
  }

  async function fillMupdf() {
    try {
      const i = await invoke("engine_info");
      $("a_version").textContent = versionText(i);
      $("a_mupdf").textContent = mupdfText(i);
    } catch (_) {
      $("a_version").textContent = "Version not available";
      $("a_mupdf").textContent = "MuPDF version not available";
    }
  }

  // About and Settings are full-screen sheets that share one history entry, so Android Back closes
  // whichever is open.
  function openSheet(which, id, focusId) {
    closeMenu();
    if (sheetOpen) return false;
    sheetOpen = which;
    $(id).hidden = false;
    $(id).scrollTop = 0;
    try { history.pushState({ glSheet: 1 }, ""); sheetPushed = true; } catch (_) { sheetPushed = false; }
    $(focusId).focus();
    return true;
  }

  // viaBack: true when Android Back already popped the history entry.
  function closeSheet(viaBack) {
    if (!sheetOpen) return;
    $(sheetOpen === "settings" ? "settings" : "about").hidden = true;
    sheetOpen = null;
    if (sheetPushed) {
      sheetPushed = false;
      if (!viaBack) { try { history.back(); } catch (_) { /* ignore */ } }
    }
    try { $("menu_btn").focus(); } catch (_) { /* ignore */ }
  }

  function openAbout() {
    if (!openSheet("about", "about", "about_close")) return;
    $("a_version").textContent = "Version …";
    $("a_mupdf").textContent = "MuPDF …";
    fillMupdf();
  }

  function closeAbout(viaBack) {
    if (sheetOpen === "about") closeSheet(viaBack);
  }

  // --- Settings sheet ---------------------------------------------------------------------------------------

  const prefs = () => window.GLPrefs || null;

  // Shows the saved page direction on the two-choice control.
  function syncSettings() {
    const p = prefs();
    const cur = p ? p.get("pageDirection") : "vertical";
    for (const v of ["vertical", "sideways"]) {
      const b = $("dir_" + v);
      b.setAttribute("aria-checked", v === cur ? "true" : "false");
    }
  }

  function setPageDirection(v) {
    const p = prefs();
    if (p) p.set("pageDirection", v);
    syncSettings();
  }

  function openSettings() {
    if (!openSheet("settings", "settings", "settings_close")) return;
    syncSettings();
  }

  function closeSettings(viaBack) {
    if (sheetOpen === "settings") closeSheet(viaBack);
  }
  async function engineInfo() {
    $("d_err").hidden = true;
    try {
      const i = await invoke("engine_info");
      const v = mupdfText(i);
      $("a_version").textContent = versionText(i);
      $("a_mupdf").textContent = v;
      $("mupdf").textContent = v;
      $("page").textContent = i.page_size + " bytes";
      $("rss").textContent = (typeof i.rss_mb === "number" ? i.rss_mb.toFixed(1) : "?") + " MB";
      $("out").hidden = false;
    } catch (_) {
      // A fixed code only: the error text of a failed call can carry paths or names.
      $("d_err").textContent = "E_ENGINE_INFO";
      $("d_err").hidden = false;
    }
  }

  // --- wiring -----------------------------------------------------------------------------------------------

  function onViewerExit() {
    closeMenu();
    refresh();
    // A file that came from another app: go back to that app instead of staying here.
    const cur = viewerCall("current");
    if (cur && cur.incoming) {
      invoke("move_to_back").catch(() => {});
      viewerCall("clearIncoming"); // only the first time (2bf review finding 4)
    }
  }

  function init() {
    $("open_pdf").addEventListener("click", openPicker);
    $("menu_btn").addEventListener("click", () => openMenu($("menu_btn"), [
      { label: "Settings", run: openSettings },
      { label: "About", run: openAbout },
    ]));
    $("menu_back").addEventListener("click", closeMenu);
    $("smsg_ok").addEventListener("click", hideMsg);
    $("def_dismiss").addEventListener("click", dismissDefaultCard);
    $("about_close").addEventListener("click", () => closeAbout(false));
    $("settings_close").addEventListener("click", () => closeSettings(false));
    $("dir_vertical").addEventListener("click", () => setPageDirection("vertical"));
    $("dir_sideways").addEventListener("click", () => setPageDirection("sideways"));
    $("d_info").addEventListener("click", engineInfo);
    $("d_measure").addEventListener("click", () => { location.href = "measure.html"; });

    // Android Back closes the open sheet (About or Settings). When the viewer was opened on top of the sheet and has just been
    // closed again, the entry we land on is the sheet's own: leave it open then.
    window.addEventListener("popstate", (e) => {
      if (!sheetOpen) return;
      if (e.state && e.state.glSheet) return;
      closeSheet(true);
    });
    document.addEventListener("keydown", (e) => {
      if (e.key !== "Escape") return;
      if (!$("menu").hidden) closeMenu();
      else if (sheetOpen) closeSheet(false);
    });

    viewerCall("onExit", onViewerExit);
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "visible") { checkIncoming(); refresh(); }
    });
    try {
      const t = window.__TAURI__;
      if (t && t.event && typeof t.event.listen === "function") {
        Promise.resolve(t.event.listen("gl-incoming", () => { checkIncoming(); })).catch(() => {});
      }
    } catch (_) { /* no events: load and visibility still cover it */ }
    // Nothing emits "gl-incoming" yet (the Kotlin plugin event does not reach this page; 2bf review
    // finding 2), and a file sent while Greenline stays visible (split screen, a download
    // notification) changes neither visibility nor focus reliably. So look every 2 s while visible:
    // one cheap call that answers E_NONE when nothing waits.
    window.addEventListener("focus", () => checkIncoming());
    setInterval(() => { if (document.visibilityState === "visible") checkIncoming(); }, 2000);

    return Promise.all([refresh(), checkIncoming()]);
  }

  window.GLStart = {
    refresh, render, checkIncoming, formatDate, openAbout, closeAbout, openSettings, closeSettings, openMenu, closeMenu, hideMsg,
    ready: init(),
  };
})();
