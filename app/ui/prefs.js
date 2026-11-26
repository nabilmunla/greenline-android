// Greenline preferences (task PD.2): a tiny global GLPrefs kept as JSON in localStorage "gl.prefs".
//   GLPrefs.get(name)         -> the saved value, or the default when missing, corrupt or not allowed
//   GLPrefs.set(name, value)  -> saves it; unknown names and values that are not allowed are ignored
//   GLPrefs.all()             -> a copy of every pref (defaults filled in)
// To add a pref later (for example "save a copy" or "save back to Drive"), add one entry to DEFS.
// Every storage access is in try/catch. When storage is unavailable or refuses a write, the values
// live in an in-memory copy for this run, so nothing here ever throws. Never logs anything.
(function () {
  "use strict";

  const KEY = "gl.prefs";

  // name -> { def: default value, ok: (value) => is this value allowed }
  const DEFS = {
    pageDirection: { def: "vertical", ok: (v) => v === "vertical" || v === "sideways" },
    updateCheck: { def: "on", ok: (v) => v === "on" || v === "off" }, // automatic once-a-day update check
  };

  let mem = null;        // in-memory copy: used when storage cannot be read or written
  let memLeads = false;  // true once a write to storage failed: the in-memory copy is then the truth

  function defaults() {
    const out = {};
    for (const k of Object.keys(DEFS)) out[k] = DEFS[k].def;
    return out;
  }

  // Keeps only known names holding allowed values; everything else falls back to the default.
  function clean(src) {
    const out = defaults();
    if (!src || typeof src !== "object" || Array.isArray(src)) return out;
    for (const k of Object.keys(DEFS)) {
      if (Object.prototype.hasOwnProperty.call(src, k) && DEFS[k].ok(src[k])) out[k] = src[k];
    }
    return out;
  }

  function load() {
    if (memLeads && mem) return clean(mem);
    try {
      const raw = window.localStorage.getItem(KEY);
      if (raw === null || raw === undefined) return mem ? clean(mem) : defaults();
      let parsed = null;
      try { parsed = JSON.parse(raw); } catch (_) { parsed = null; }
      return clean(parsed);
    } catch (_) {
      return mem ? clean(mem) : defaults();
    }
  }

  function get(name) {
    if (!Object.prototype.hasOwnProperty.call(DEFS, name)) return undefined;
    return load()[name];
  }

  function set(name, value) {
    if (!Object.prototype.hasOwnProperty.call(DEFS, name) || !DEFS[name].ok(value)) return false;
    const next = load();
    next[name] = value;
    mem = next;
    try {
      window.localStorage.setItem(KEY, JSON.stringify(next));
      memLeads = false;
    } catch (_) {
      memLeads = true; // storage unavailable: keep this run's choice in memory
    }
    return true;
  }

  function all() {
    return load();
  }

  window.GLPrefs = { get: get, set: set, all: all };
})();