/* Greenline viewer math (task 2bf.6).
 *
 * Classic script, no modules, no DOM, no timers: loads over file:// and in the
 * Tauri WebView and defines one global, GLMath. Pure functions only.
 *
 * Conventions
 *   - A "view" is {s, ox, oy}. s = CSS px per page point. (ox, oy) = the page
 *     point (in points) that sits at the viewport's top-left corner. So a page
 *     point (X, Y) is drawn at viewport CSS px ((X-ox)*s, (Y-oy)*s).
 *   - ptW/ptH = page size in points; viewW/viewH = viewport size in CSS px.
 *   - Engine tiles are addressed in pixels at the engine level L (device px per
 *     point; viewer.js calls it TILE_ZOOM, 4 = 400 %): x0 = tx*TILE, y0 = ty*TILE.
 *   - Views are returned as new objects; inputs are never mutated.
 */
(function (root) {
  "use strict";

  // ---- thresholds (from docs/PLAN.md, Part 2b-fix, decision 4) ----
  var C = {
    TILE: 1024,            // tile edge, px at the engine level
    LEVELS: [2, 4],        // engine levels available for tiles
    MAX_PX_PER_PT: 16,     // 1600 % (task 2c.2; was 400 %): device px per point at full zoom
    ZOOMED_FACTOR: 1.05,   // "zoomed" means s > fit * 1.05
    TAP_MOVE_PX: 10,       // tap: moved less than this ...
    TAP_MAX_MS: 300,       // ... and shorter than this
    DOUBLE_TAP_MS: 250,    // second tap within this of the first tap's end
    DOUBLE_TAP_PX: 40,     // and within this distance of the first tap
    SWIPE_MIN_DX: 60,      // swipe: |dx| at least this
    SWIPE_RATIO: 2,        // and |dx| >= RATIO * |dy|
    SWIPE_MAX_MS: 400,     // and (duration <= this ...
    SWIPE_MIN_VEL: 0.4     // ... or |dx|/duration > this, px per ms)
  };

  function hypot(dx, dy) { return Math.sqrt(dx * dx + dy * dy); }

  // CSS px per page point so the whole page fits the viewport.
  function fitScale(ptW, ptH, viewW, viewH) {
    return Math.min(viewW / ptW, viewH / ptH);
  }

  // Largest allowed scale for a device pixel ratio: 4 device px per point.
  function maxScale(dpr) { return C.MAX_PX_PER_PT / dpr; }

  // True when the view is zoomed in beyond the fit tolerance (5 %).
  function isZoomed(s, fit) { return s > fit * C.ZOOMED_FACTOR; }

  // Clamp s to [fit, max]. If max < fit (degenerate), fit wins.
  function clampScale(s, fit, max) {
    return Math.max(fit, Math.min(s, max));
  }

  function clampAxis(o, pt, viewPx, s) {
    var visible = viewPx / s;           // viewport extent in points
    if (pt <= visible) return (pt - visible) / 2;   // page smaller: centre it
    return Math.max(0, Math.min(o, pt - visible));  // keep viewport inside page
  }

  // Clamp the offset of a view. Per axis: page smaller than the viewport ->
  // centred (offset is negative); otherwise the viewport stays inside the page.
  function clampOffset(view, ptW, ptH, viewW, viewH) {
    return {
      s: view.s,
      ox: clampAxis(view.ox, ptW, viewW, view.s),
      oy: clampAxis(view.oy, ptH, viewH, view.s)
    };
  }

  // New view for two fingers moving from (p0a, p0b) to (p1a, p1b), points as
  // {x, y} in viewport CSS px. Scale follows the distance ratio; the page point
  // under the old midpoint ends up under the new midpoint (pinch + pan in one).
  // Not clamped. If the fingers started on the same spot, scale is unchanged.
  function pinch(view, p0a, p0b, p1a, p1b) {
    var d0 = hypot(p0b.x - p0a.x, p0b.y - p0a.y);
    var d1 = hypot(p1b.x - p1a.x, p1b.y - p1a.y);
    var s1 = d0 > 1e-9 ? view.s * (d1 / d0) : view.s;
    var m0x = (p0a.x + p0b.x) / 2, m0y = (p0a.y + p0b.y) / 2;
    var m1x = (p1a.x + p1b.x) / 2, m1y = (p1a.y + p1b.y) / 2;
    var ax = view.ox + m0x / view.s;    // anchor page point under old midpoint
    var ay = view.oy + m0y / view.s;
    return { s: s1, ox: ax - m1x / s1, oy: ay - m1y / s1 };
  }

  // pinch() followed by clamping. limits = {ptW, ptH, viewW, viewH, fit, max}.
  // The scale is clamped first, then the offset is recomputed for the clamped
  // scale so the anchor stays under the new midpoint as well as possible, then
  // the offset is clamped (which only moves it at page edges).
  function pinchClamped(view, p0a, p0b, p1a, p1b, limits) {
    var raw = pinch(view, p0a, p0b, p1a, p1b);
    var s1 = clampScale(raw.s, limits.fit, limits.max);
    var m1x = (p1a.x + p1b.x) / 2, m1y = (p1a.y + p1b.y) / 2;
    // page point under the new midpoint in the unclamped result (= the anchor)
    var ax = raw.ox + m1x / raw.s;
    var ay = raw.oy + m1y / raw.s;
    return clampOffset({ s: s1, ox: ax - m1x / s1, oy: ay - m1y / s1 },
      limits.ptW, limits.ptH, limits.viewW, limits.viewH);
  }

  // New view at scale s1 keeping viewport point (px, py) fixed (double-tap).
  // Not clamped.
  function zoomAt(view, s1, px, py) {
    var ax = view.ox + px / view.s;
    var ay = view.oy + py / view.s;
    return { s: s1, ox: ax - px / s1, oy: ay - py / s1 };
  }

  // Engine level for the current scale. need = s*dpr device px per point.
  // Returns 0 ("use the fit image") when need <= fitImagePxPerPt (if given),
  // else the smallest level in `levels` that is >= need, else the largest level.
  function chooseLevel(s, dpr, levels, fitImagePxPerPt) {
    var lv = levels || C.LEVELS;
    var need = s * dpr;
    var eps = 1e-9;
    if (typeof fitImagePxPerPt === "number" && need <= fitImagePxPerPt + eps) return 0;
    for (var i = 0; i < lv.length; i++) {
      if (lv[i] >= need - eps) return lv[i];
    }
    return lv[lv.length - 1];
  }

  // Tiles covering the visible part of the page at an engine level, clipped to
  // the page. Returns [{tx, ty, x0, y0}] row-major; x0/y0 are px at `level`.
  // The page is ceil(ptW*level) x ceil(ptH*level) px, like viewer.js pagePx.
  function visibleTiles(view, viewW, viewH, level, ptW, ptH, tile) {
    var T = tile || C.TILE;
    var pw = Math.ceil(ptW * level), ph = Math.ceil(ptH * level);
    var nx = Math.ceil(pw / T), ny = Math.ceil(ph / T);
    // visible rectangle in px at this level, clipped to the page
    var xa = Math.max(0, view.ox * level);
    var ya = Math.max(0, view.oy * level);
    var xb = Math.min(pw, (view.ox + viewW / view.s) * level);
    var yb = Math.min(ph, (view.oy + viewH / view.s) * level);
    var out = [];
    if (xb <= xa || yb <= ya) return out;
    var tx0 = Math.floor(xa / T), tx1 = Math.min(nx - 1, Math.ceil(xb / T) - 1);
    var ty0 = Math.floor(ya / T), ty1 = Math.min(ny - 1, Math.ceil(yb / T) - 1);
    for (var ty = ty0; ty <= ty1; ty++) {
      for (var tx = tx0; tx <= tx1; tx++) {
        out.push({ tx: tx, ty: ty, x0: tx * T, y0: ty * T });
      }
    }
    return out;
  }

  // Classify ONE completed gesture.
  // samples: [{t (ms), id (pointer id), x, y, type: 'down'|'move'|'up'}] in time order.
  // state: {zoomed, axis, lastTapT, lastTapX, lastTapY}. axis is 'h' (sideways,
  //   the default; also used when missing or unknown) or 'v' (vertical).
  //   lastTapT is the END (up) time of the previous tap, or null.
  // Returns 'tap' | 'double-tap' | 'pan' | 'swipe-left' | 'swipe-right' |
  //   'swipe-up' | 'swipe-down' | 'pinch'.
  //   - >= 2 pointers down at once at any moment -> 'pinch'.
  //   - single pointer, moved < 10 px (max distance from the start point, so a
  //     jittery finger that returns still counts) and shorter than 300 ms -> 'tap';
  //     'double-tap' if it started <= 250 ms after lastTapT and within 40 px of
  //     the last tap.
  //   - swipe only when not zoomed. axis 'h': |dx| >= 60, |dx| >= 2|dy|, and
  //     (duration <= 400 ms or |dx|/duration > 0.4 px/ms). dx < 0 -> 'swipe-left'
  //     (next). axis 'v': same with |dy| in place of |dx| and |dx| in place of
  //     |dy|; dy < 0 -> 'swipe-up' (next), dy > 0 -> 'swipe-down' (previous).
  //     A move along the other axis is not a swipe (falls through to 'pan').
  //   - everything else (including empty input) -> 'pan'.
  function classify(samples, state) {
    state = state || {};
    var n = samples ? samples.length : 0;
    if (n === 0) return "pan";
    var down = {}, count = 0, i, sm;
    for (i = 0; i < n; i++) {
      sm = samples[i];
      if (sm.type === "up") {
        if (down[sm.id]) { delete down[sm.id]; count--; }
      } else if (!down[sm.id]) {
        down[sm.id] = true; count++;
        if (count >= 2) return "pinch";
      }
    }
    var first = samples[0], last = samples[n - 1];
    var dx = last.x - first.x, dy = last.y - first.y;
    var dur = last.t - first.t;
    var moved = 0;
    for (i = 0; i < n; i++) {
      var d = hypot(samples[i].x - first.x, samples[i].y - first.y);
      if (d > moved) moved = d;
    }
    if (moved < C.TAP_MOVE_PX && dur < C.TAP_MAX_MS) {
      if (typeof state.lastTapT === "number") {
        var gap = first.t - state.lastTapT;
        var dist = hypot(first.x - state.lastTapX, first.y - state.lastTapY);
        if (gap >= 0 && gap <= C.DOUBLE_TAP_MS && dist <= C.DOUBLE_TAP_PX) return "double-tap";
      }
      return "tap";
    }
    if (!state.zoomed) {
      var adx = Math.abs(dx), ady = Math.abs(dy);
      if (state.axis === "v") {
        if (ady >= C.SWIPE_MIN_DX && ady >= C.SWIPE_RATIO * adx &&
            (dur <= C.SWIPE_MAX_MS || ady / dur > C.SWIPE_MIN_VEL)) {
          return dy < 0 ? "swipe-up" : "swipe-down";
        }
      } else if (adx >= C.SWIPE_MIN_DX && adx >= C.SWIPE_RATIO * ady &&
          (dur <= C.SWIPE_MAX_MS || adx / dur > C.SWIPE_MIN_VEL)) {
        return dx < 0 ? "swipe-left" : "swipe-right";
      }
    }
    return "pan";
  }

  root.GLMath = {
    // constants
    TILE: C.TILE, LEVELS: C.LEVELS, MAX_PX_PER_PT: C.MAX_PX_PER_PT,
    ZOOMED_FACTOR: C.ZOOMED_FACTOR, TAP_MOVE_PX: C.TAP_MOVE_PX,
    TAP_MAX_MS: C.TAP_MAX_MS, DOUBLE_TAP_MS: C.DOUBLE_TAP_MS,
    DOUBLE_TAP_PX: C.DOUBLE_TAP_PX, SWIPE_MIN_DX: C.SWIPE_MIN_DX,
    SWIPE_RATIO: C.SWIPE_RATIO, SWIPE_MAX_MS: C.SWIPE_MAX_MS,
    SWIPE_MIN_VEL: C.SWIPE_MIN_VEL,
    // functions
    fitScale: fitScale, maxScale: maxScale, isZoomed: isZoomed,
    clampScale: clampScale, clampOffset: clampOffset,
    pinch: pinch, pinchClamped: pinchClamped, zoomAt: zoomAt,
    chooseLevel: chooseLevel, visibleTiles: visibleTiles, classify: classify
  };
})(typeof window !== "undefined" ? window : this);
