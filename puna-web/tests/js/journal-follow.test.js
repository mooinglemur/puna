// The journal feed's follow loop, lifted out of `journal.js` and driven against a fake scroll
// container.
//
// **The properties here are sequences over frames, which is why they are not a source lint.** Two
// bugs have shipped in this loop, both invisible to any assertion over the text: it gave up
// following when a batch of opening rows grew the content (reading its own animation as a reader
// scrolling away), and it opened one line short of the bottom because the backfill control is
// revealed after the pin. A third was caught here before it shipped, when the first fix compared
// only the recorded position and so mistook the browser's own clamp for a reader.
//
// The loop is lifted by slicing the file rather than by importing it, because `journal.js` is an
// IIFE over live DOM lookups: there is nothing to export and nothing to import. The slice is
// bounded by two comments, so a rename fails loudly here rather than silently testing nothing.
"use strict";

const fs = require("fs");
const path = require("path");

const source = path.join(__dirname, "..", "..", "static", "journal.js");
const FROM = "  function nearBottom() {";
const TO = "  // `live` is true only for an `append` frame";

function slice() {
  const src = fs.readFileSync(source, "utf8");
  const start = src.indexOf(FROM);
  const end = src.indexOf(TO);
  if (start < 0 || end < 0) {
    throw new Error(
      "journal.js no longer contains the block this test lifts (looked for `" +
        FROM.trim() +
        "` and `" +
        TO.trim() +
        "`)"
    );
  }
  return src.slice(start, end);
}

function harness(lifted) {
  const frames = [];
  let now = 0;
  const log = {
    _top: 0,
    scrollHeight: 2000,
    clientHeight: 500,
    get scrollTop() {
      return this._top;
    },
    // A real scroll container clamps whatever it is given, which is the behavior the whole fix
    // turns on: the browser also clamps by itself when content shrinks underneath.
    set scrollTop(v) {
      this._top = Math.max(0, Math.min(v, this.scrollHeight - this.clientHeight));
    },
    clamp() {
      this._top = Math.max(0, Math.min(this._top, this.scrollHeight - this.clientHeight));
    },
  };
  const raf = (fn) => frames.push(fn);
  // A fake ResizeObserver that just hands back its callback, so a height change can be delivered
  // deliberately rather than waited for.
  let resized = null;
  const win = {
    ResizeObserver: function (fn) {
      resized = fn;
      return { observe() {} };
    },
  };
  const clock = { now: () => now };
  const make = new Function(
    "log",
    "requestAnimationFrame",
    "Date",
    "window",
    lifted + "\nreturn { followBottom: followBottom, readerMoved: readerMoved, pinBottom: pinBottom };"
  );
  const api = make(log, raf, clock, win);
  return {
    log,
    api,
    tick(ms) {
      now += ms;
      const due = frames.splice(0, frames.length);
      due.forEach((fn) => fn());
    },
    pending: () => frames.length,
    resize: () => resized && resized(),
  };
}

exports.run = function (t) {
  const check = t.check;
  const lifted = slice();

  // --- the reported bug: a release opens hundreds of rows, which grows the content under the view ---
  {
    const h = harness(lifted);
    h.log.scrollTop = h.log.scrollHeight; // at the bottom, 1500
    h.api.pinBottom();
    h.api.followBottom();

    // Frame 1: every arriving row is at height 0, so the content is far SHORTER and the browser
    // clamps the view up to the new maximum.
    h.log.scrollHeight = 1200;
    h.log.clamp();
    h.tick(16);

    // Frames 2..n: the rows open, so the content grows back past where it started. `scrollTop` is
    // untouched by that; only the bottom moves.
    for (let height = 1400; height <= 2600; height += 400) {
      h.log.scrollHeight = height;
      h.tick(16);
    }

    check(
      "a growing batch keeps the view at the bottom",
      h.log.scrollTop === h.log.scrollHeight - h.log.clientHeight
    );
    check("and the follow is still running", h.pending() > 0);
  }

  // --- the reader's own scroll still wins, on the very next frame ---
  {
    const h = harness(lifted);
    h.log.scrollTop = h.log.scrollHeight;
    h.api.pinBottom();
    h.api.followBottom();
    h.tick(16);

    const away = h.log.scrollHeight - h.log.clientHeight - 300;
    h.log.scrollTop = away; // a wheel tick
    h.tick(16);

    check("a reader's scroll stops the follow", h.log.scrollTop === away);
    check("and nothing is scheduled after it", h.pending() === 0);
  }

  // --- and it gives up on its own once the animation is over ---
  {
    const h = harness(lifted);
    h.log.scrollTop = h.log.scrollHeight;
    h.api.pinBottom();
    h.api.followBottom();
    for (let i = 0; i < 40; i++) h.tick(16);
    check("the follow expires rather than running forever", h.pending() === 0);
  }

  // --- the reported initial load: something above the feed appears and steals its height -----------
  {
    const h = harness(lifted);
    h.log.scrollTop = h.log.scrollHeight; // the replay landed and the page pinned
    h.api.pinBottom();
    const wasAt = h.log.scrollTop;

    // The backfill control is revealed, so the feed is shorter and its end is further down.
    h.log.clientHeight -= 34;
    h.resize();

    check(
      "a shorter feed is re-pinned to its new bottom",
      h.log.scrollTop === h.log.scrollHeight - h.log.clientHeight && h.log.scrollTop > wasAt
    );
  }

  // --- but not for somebody who is reading further up ---------------------------------------------
  {
    const h = harness(lifted);
    h.log.scrollTop = h.log.scrollHeight;
    h.api.pinBottom();
    const away = 200; // scrolled well back
    h.log.scrollTop = away;

    h.log.clientHeight -= 34;
    h.resize();

    check("a reader further up is left alone by a resize", h.log.scrollTop === away);
  }
};
