'use strict';
// A deterministic virtual clock so time-based effects (Time.every, Process.sleep,
// animation frames) are reproducible and comparable across backends. Each
// backend gets its own instance; the runner advances it explicitly.

function makeClock() {
  let now = 0;
  let seq = 1;
  let timeouts = []; // { id, at, fn }
  let intervals = []; // { id, every, next, fn }
  let frames = []; // fn

  return {
    now: () => now,
    setTimeout: (fn, ms) => { const id = seq++; timeouts.push({ id, at: now + ms, fn }); return id; },
    clearTimeout: (id) => { timeouts = timeouts.filter((t) => t.id !== id); },
    setInterval: (fn, ms) => { const id = seq++; intervals.push({ id, every: ms, next: now + ms, fn }); return id; },
    clearInterval: (id) => { intervals = intervals.filter((iv) => iv.id !== id); },
    requestAnimationFrame: (fn) => { frames.push(fn); return frames.length; },
    cancelAnimationFrame: () => { frames = []; },

    // Advance time by `ms`, firing due timeouts/intervals in chronological order.
    advance(ms) {
      const target = now + ms;
      for (;;) {
        let next = Infinity;
        for (const t of timeouts) next = Math.min(next, t.at);
        for (const iv of intervals) next = Math.min(next, iv.next);
        if (next > target || next === Infinity) break;
        now = next;
        for (const t of timeouts.filter((t) => t.at <= now)) {
          timeouts = timeouts.filter((x) => x !== t);
          t.fn();
        }
        for (const iv of intervals) {
          while (iv.next <= now) { iv.next += iv.every; iv.fn(); }
        }
      }
      now = target;
    },

    // Deliver one animation frame to all pending rAF callbacks.
    flushFrame() {
      const fs = frames;
      frames = [];
      for (const f of fs) f(now);
    },
  };
}

module.exports = { makeClock };
