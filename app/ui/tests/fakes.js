// Test helpers for the start screen (start.test.html, shot.html). Synthetic data only.
// GLFakes.make(opts) builds a fake window.__TAURI__ and a fake window.GLViewer that record their calls.
// GLFakes.load(opts) loads ../index.html into a fresh iframe of a given size with those fakes installed
// BEFORE start.js runs (viewmath.js and viewer.js are left out), and resolves when start.js is ready.
//
// Updater stubs (task 2b.9), all optional make() options:
//   updateAvailable (false), updateVersion ("0.1.9"), updateSize (13.1)  -> what update_check answers
//   updateCheckReject: a code string (or any value) update_check rejects with
//   downloadReject: value update_download rejects with (default: it succeeds); an array gives one value per call (the last repeats)
//   downloadHold: true -> update_download stays pending until fake.st.finishDownload() / failDownload(value)
//   downloadMs: delay before update_download settles (default 0; ignored with downloadHold)
//   progress: array of {done, total} answers for update_progress, one per call (the last one repeats)
//   installOpened: true (default) | false | an array of them, one per call (the last repeats)
//   installReject: value update_install rejects with
//   manualPoll: true -> the page's 500 ms setInterval (the progress poll) never fires by itself; the test calls
//     await page.tick() to fire it, and fake.timers lists the intervals that are still running (so a stopped poll is visible)
(function () {
  "use strict";

  function make(opts) {
    opts = opts || {};
    const st = {
      recents: (opts.recents || []).map((r) => Object.assign({}, r)),
      isOpen: !!opts.isOpen,
      current: opts.current || null,
      exitCbs: [],
      listeners: {},
    };
    const calls = { invoke: [], open: [], resume: 0, listen: [] };
    const timers = [];
    const seq = (v, i) => (Array.isArray(v) ? v[Math.min(i, v.length - 1)] : v);
    let progressN = 0, installN = 0, downloadN = 0;
    let finish = null, fail = null;
    st.finishDownload = () => { if (finish) finish({}); };
    st.failDownload = (v) => { if (fail) fail(v); };

    function invoke(cmd, args) {
      calls.invoke.push({ cmd: cmd, args: args });
      switch (cmd) {
        case "list_recents":
          if (opts.listReject) return Promise.reject(new Error("boom"));
          return Promise.resolve(st.recents.map((r) => Object.assign({}, r)));
        case "remove_recent":
          st.recents = st.recents.filter((r) => r.id !== args.id);
          return Promise.resolve(true);
        case "default_pdf_app":
          if (opts.defaultReject) return Promise.reject(new Error("nope"));
          return Promise.resolve({ is_default: opts.isDefault !== false });
        case "engine_info": {
          if (opts.engineInfoReject) return Promise.reject(new Error("engine_info refused for /storage/emulated/0/SENTINEL-name.pdf"));
          const info = { mupdf_version: "1.26.0", mupdf_version_ok: true, page_size: 4096, rss_mb: 123.456 };
          // app_version comes from the stub (opts.appVersion; null leaves the field out), never from the page.
          if (opts.appVersion !== null) info.app_version = opts.appVersion === undefined ? "0.0.0-stub" : opts.appVersion;
          return Promise.resolve(info);
        }
        case "move_to_back":
          return Promise.resolve(true);
        case "update_check": {
          if (opts.updateCheckReject !== undefined) return Promise.reject(opts.updateCheckReject);
          if (!opts.updateAvailable) return Promise.resolve({ available: false });
          return Promise.resolve({
            available: true,
            version: opts.updateVersion === undefined ? "0.1.9" : opts.updateVersion,
            size_mb: opts.updateSize === undefined ? 13.1 : opts.updateSize,
          });
        }
        case "update_download":
          if (opts.downloadHold) return new Promise((res, rej) => { finish = res; fail = rej; });
          return new Promise((res, rej) => {
            const bad = opts.downloadReject === undefined ? undefined : seq(opts.downloadReject, downloadN++);
            setTimeout(() => (bad !== undefined ? rej(bad) : res({})), opts.downloadMs || 0);
          });
        case "update_progress": {
          const list = opts.progress || [{ done: 0, total: 0 }];
          return Promise.resolve(Object.assign({}, list[Math.min(progressN++, list.length - 1)]));
        }
        case "update_install": {
          if (opts.installReject !== undefined) return Promise.reject(opts.installReject);
          const o = seq(opts.installOpened === undefined ? true : opts.installOpened, installN++);
          return Promise.resolve({ opened: o });
        }
        default:
          return Promise.reject(new Error("unknown command"));
      }
    }

    const tauri = {
      core: { invoke: invoke },
      event: {
        listen: function (name, cb) {
          calls.listen.push(name);
          st.listeners[name] = cb;
          return Promise.resolve(function () {});
        },
      },
    };

    const viewer = {
      open: function (source, startPage) {
        calls.open.push({ source: source, startPage: startPage });
        if (opts.open) return Promise.resolve(opts.open(source, startPage));
        if (source.kind === "incoming") return Promise.resolve({ error: "E_NONE" });
        return Promise.resolve({ pages: 12 });
      },
      resume: function () { calls.resume++; return true; },
      isOpen: function () { return st.isOpen; },
      current: function () { return st.current; },
      onExit: function (cb) { st.exitCbs.push(cb); },
      close: function () { return Promise.resolve(); },
    };

    return { tauri: tauri, viewer: opts.noViewer ? null : viewer, calls: calls, st: st, timers: timers, manualPoll: !!opts.manualPoll };
  }

  function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }

  let cachedHtml = null;
  function indexHtml() {
    if (cachedHtml === null) {
      const x = new XMLHttpRequest();
      x.open("GET", "../index.html", false);
      x.send();
      cachedHtml = x.responseText;
    }
    return cachedHtml;
  }

  // opts: make() options plus width, height, host (element to append the iframe to).
  async function load(opts) {
    const fake = make(opts);
    window.__F = fake;
    const iframe = document.createElement("iframe");
    iframe.style.cssText = "border:0;display:block;background:#888;width:" + (opts.width || 412) + "px;height:" + (opts.height || 800) + "px";
    (opts.host || document.body).appendChild(iframe);
    const base = new URL("../", location.href).href;
    const pollScript = !opts.manualPoll ? "" :
      "(function(){var F=parent.__F,si=window.setInterval,ci=window.clearInterval,n=0;" +
      "window.setInterval=function(f,ms){if(ms!==500)return si.apply(window,arguments);var id=\"m\"+(++n);F.timers.push({id:id,f:f,ms:ms});return id;};" +
      "window.clearInterval=function(id){for(var i=F.timers.length-1;i>=0;i--)if(F.timers[i].id===id)F.timers.splice(i,1);return ci.call(window,id);};})();";
    const fakeScript = "<script>window.__TAURI__=parent.__F.tauri;" + (opts.noViewer ? "" : "window.GLViewer=parent.__F.viewer;") + pollScript + "<\/script>";
    let html = indexHtml()
      .replace('<script src="viewmath.js"></script>', "")
      .replace('<script src="viewer.js"></script>', "")
      .replace("<head>", '<head><base href="' + base + '">' + fakeScript);
    const d = iframe.contentDocument;
    d.open();
    d.write(html);
    d.close();
    const w = iframe.contentWindow;
    for (let i = 0; i < 150 && !(w.GLStart && w.GLStart.ready); i++) await sleep(20);
    if (!(w.GLStart && w.GLStart.ready)) throw new Error("start.js did not load");
    await w.GLStart.ready;
    // A document written into an iframe has no history entry of its own to go back to, so a real
    // history.back() would replace the page. Fake the two calls: record pushes, and make back() fire
    // popstate for the previous entry, like the browser does.
    const stack = [{ test: 1 }];
    fake.history = stack;
    w.history.pushState = function (s) { stack.push(s); };
    w.history.back = function () {
      if (stack.length < 2) return;
      stack.pop();
      const st = stack[stack.length - 1];
      setTimeout(function () { w.dispatchEvent(new w.PopStateEvent("popstate", { state: st })); }, 0);
    };
    await sleep(20);
    return { iframe: iframe, win: w, doc: w.document, fake: fake };
  }

  // A synthetic drawing-like thumbnail (SVG data URL; the app sends small JPEGs).
  function thumb(hue) {
    const svg = '<svg xmlns="http://www.w3.org/2000/svg" width="112" height="112" viewBox="0 0 112 112">' +
      '<rect width="112" height="112" fill="#fff"/>' +
      '<g stroke="hsl(' + hue + ',45%,35%)" stroke-width="2" fill="none">' +
      '<rect x="10" y="14" width="92" height="84"/><path d="M10 50h60M70 14v84M70 62h32M30 50v48"/>' +
      '<circle cx="86" cy="32" r="9"/></g></svg>';
    return "data:image/svg+xml;charset=utf-8," + encodeURIComponent(svg);
  }

  // Synthetic recents, newest first.
  function recents(n, now) {
    now = now === undefined ? Date.now() : now;
    const names = ["Sample drawing 01.pdf", "Sample drawing 02 Level 1 plan.pdf", "Sample drawing 03.pdf", "Sample drawing 04 Sections.pdf"];
    const out = [];
    for (let i = 0; i < n; i++) {
      out.push({
        id: "id" + (i + 1),
        name: i < names.length && n <= 4 ? names[i] : "Sample drawing " + (i < 9 ? "0" : "") + (i + 1) + ".pdf",
        pages: 5 + i,
        last_page: i % 4,
        opened: now - i * 3 * 3600 * 1000 - (i > 1 ? 86400000 : 0),
        thumb: i % 2 === 0 ? thumb((i * 53) % 360) : null,
      });
    }
    return out;
  }

  window.GLFakes = { make: make, load: load, sleep: sleep, recents: recents, thumb: thumb };
})();
