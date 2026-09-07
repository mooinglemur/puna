// The feed's window: how much history the page asks for, walks back for, and keeps.
//
// **Why this is not a source lint.** Every property here is a sequence over frames and clicks, and
// each failure looks like the feature working. Widening the window sends one request and must send
// the next only when the first has landed and the window is still short: too eager and it queues a
// thousand backwards seeks at a server reading a 250 MB file, too shy and it stops mid-walk, which
// is indistinguishable from a room with a short history. Narrowing must trim and must NOT ask the
// server for anything. And a page whose trim has eaten into its oldest batch can no longer say
// where it begins, so walking back from it would prepend records that stop short of what is on
// screen: a hole in the middle of the feed, on a page whose whole promise is that it omits nothing.
//
// The blocks are lifted by slicing rather than imported, for the reason `journal-follow.test.js`
// gives: `journal.js` is an IIFE over live DOM lookups, with nothing to export.
"use strict";

const fs = require("fs");
const path = require("path");

const source = path.join(__dirname, "..", "..", "static", "journal.js");

// Three blocks: the offset helpers and the trim, `prepend` (for the empty-page answer alone, which
// is what stops the walk asking forever), and the window control itself. Sliced separately because
// everything between them is the record renderer.
const CUTS = [
  ["  // --- WHERE THE PAGE BEGINS", "  // `live` is true only for an `append` frame"],
  ["  function prepend(events, start) {", "  // --- THE THREE VIEW FILTERS"],
  ["  // How many records to ask for on connect", "  function open() {"],
];

function lift() {
  const src = fs.readFileSync(source, "utf8");
  return CUTS.map(([from, to]) => {
    const start = src.indexOf(from);
    const end = src.indexOf(to);
    if (start < 0 || end < 0 || end < start) {
      throw new Error(
        "journal.js no longer contains a block this test lifts (looked for `" +
          from.trim() +
          "` and `" +
          to.trim() +
          "`)"
      );
    }
    return src.slice(start, end);
  }).join("\n");
}

// A row is 20 pixels, so a scroll position is a row index times twenty and a trim's effect on the
// reader's place is arithmetic rather than a guess.
const ROW = 20;

// `siblings` is the row's own container, so `nextElementSibling` answers what the document would.
// The trim hands a day heading's offset down to the row below it, and without that link the carry
// silently does nothing: the stub would report a page that cannot say where it begins, which is a
// real state and the wrong one.
function row(siblings, start, daybreak) {
  const made = {
    dataset: {},
    daybreak: !!daybreak,
    classList: { contains: (name) => name === "daybreak" && !!daybreak },
    get nextElementSibling() {
      const at = siblings.indexOf(made);
      return at >= 0 && at + 1 < siblings.length ? siblings[at + 1] : null;
    },
  };
  if (typeof start === "number") made.dataset.start = String(start);
  return made;
}

function makeLog(count, start) {
  const rows = [];
  for (let i = 0; i < count; i++) rows.push(row(rows, i === 0 ? start : undefined));
  return {
    rows,
    scrollTop: 0,
    get firstElementChild() {
      return rows.length ? rows[0] : null;
    },
    get childElementCount() {
      return rows.length;
    },
    get scrollHeight() {
      return rows.length * ROW;
    },
    removeChild(node) {
      rows.splice(rows.indexOf(node), 1);
    },
    replaceChildren() {
      rows.length = 0;
    },
    // The only selector this block ever passes is the record one; day headings are rows and are
    // not records.
    querySelectorAll() {
      return rows.filter((r) => !r.daybreak);
    },
  };
}

function harness(options) {
  const settings = options || {};
  const log = makeLog(settings.rows === undefined ? 500 : settings.rows, settings.start);
  const sent = [];
  const socket = {
    readyState: 1,
    send(text) {
      sent.push(JSON.parse(text));
    },
  };
  const store = Object.assign({}, settings.store);
  const toggles = {
    recall: (key) => (typeof store[key] === "string" ? store[key] : ""),
    remember: (key, value) => {
      if (value) store[key] = value;
      else delete store[key];
    },
  };
  const clicks = {};
  const buttons = [Infinity, 10000, 5000, 2500, 1000, 500].map((size) => ({
    dataset: { lines: size === Infinity ? "all" : String(size) },
    disabled: false,
    current: null,
    setAttribute(name, value) {
      if (name === "aria-current") this.current = value;
    },
    removeAttribute(name) {
      if (name === "aria-current") this.current = null;
    },
    addEventListener(name, fn) {
      if (name === "click") clicks[this.dataset.lines] = fn;
    },
  }));
  const windowRow = {
    hidden: true,
    querySelectorAll: () => buttons,
  };
  const progress = { textContent: "" };
  let pins = 0;
  let notes = 0;

  const make = new Function(
    "log",
    "socket",
    "WebSocket",
    "cursor",
    "cap",
    "backfilling",
    "rebuilding",
    "backfilled",
    "lastDay",
    "progress",
    "windowRow",
    "window",
    "WINDOW_SIZES",
    "DEFAULT_WINDOW",
    "REPLAY_MAX",
    "WINDOW_KEY",
    "nearBottom",
    "readerMoved",
    "following",
    "pinBottom",
    "refreshFilterNote",
    lift() +
      "\nreturn {" +
      "  fill: fill," +
      "  prepend: prepend," +
      "  setWindow: setWindow," +
      "  setProgress: setProgress," +
      "  cap: function () { return cap; }," +
      // What the frame handler does when a backfill page arrives, and the only reason this is
      // exposed: the walk's continuation is a two-step sequence whose middle step lives in the
      // socket handler, which is not lifted here.
      "  landed: function () { backfilling = false; }" +
      "};"
  );

  const api = make(
    log,
    socket,
    { OPEN: 1 },
    settings.cursor === undefined ? 200000 : settings.cursor,
    1000,
    false,
    false,
    0,
    null,
    progress,
    windowRow,
    { PunaToggles: settings.noToggles ? null : toggles },
    [Infinity, 10000, 5000, 2500, 1000, 500],
    1000,
    5000,
    "journal.lines",
    () => !!settings.atBottom,
    () => false,
    false,
    () => {
      pins++;
    },
    () => {
      notes++;
    }
  );

  return {
    api,
    log,
    sent,
    store,
    buttons,
    progress,
    windowRow,
    click: (label) => clicks[label](),
    pins: () => pins,
    notes: () => notes,
    // Stand a backfill page's worth of rows on the front, as `prepend` would, and re-anchor the
    // head the way its `start` does.
    row: (start, daybreak) => row(log.rows, start, daybreak),
    deliver(count, start) {
      const added = [];
      for (let i = 0; i < count; i++) added.push(row(log.rows, undefined));
      if (added.length) added[0].dataset.start = String(start);
      log.rows.unshift.apply(log.rows, added);
      api.landed();
    },
    current: () => harnessCurrent(buttons),
  };
}

function harnessCurrent(buttons) {
  const on = buttons.filter((b) => b.disabled);
  return on.length === 1 ? on[0].dataset.lines : on.length + " buttons";
}

exports.run = function (t) {
  // --- WHAT THE STORE IS ALLOWED TO SAY ---------------------------------------------------------
  {
    const h = harness({ store: { "journal.lines": "2500" } });
    t.check("a remembered window is honored on load", h.api.cap() === 2500);
    t.check("and its button is the disabled one", h.current() === "2500");
    t.check("the control is revealed", h.windowRow.hidden === false);
  }
  {
    // **The whole feed is never persisted**, and this is the reader's half of that rule: a store
    // entry saying otherwise, from anywhere, must not make a page load a room's entire history.
    const h = harness({ store: { "journal.lines": "Infinity" } });
    t.check("a stored whole-feed choice is refused", h.api.cap() === 1000);
  }
  {
    const h = harness({ store: { "journal.lines": "9999" } });
    t.check("so is a size this build does not offer", h.api.cap() === 1000);
  }
  {
    const h = harness({ noToggles: true });
    t.check(
      "and a page whose toggles.js never arrived still gets a window",
      h.api.cap() === 1000 && h.current() === "1000"
    );
  }

  // --- CHOOSING ---------------------------------------------------------------------------------
  {
    const h = harness({ store: { "journal.lines": "2500" }, rows: 2500, start: 4096 });
    h.click("all");
    t.check("clicking a button sets the window", h.api.cap() === Infinity);
    t.check("and marks it as the current one", h.current() === "all");
    t.check(
      "the whole feed is not written to the store, and does not clear what was there",
      h.store["journal.lines"] === "2500"
    );
    h.click("500");
    t.check("a numeric choice is remembered", h.store["journal.lines"] === "500");
  }

  // --- WIDENING WALKS, AND STOPS ----------------------------------------------------------------
  {
    // 500 rows on the page beginning at byte 4096, widened to 2500: 2000 records short.
    const h = harness({ rows: 500, start: 4096 });
    h.api.setWindow(2500);
    t.check("widening asks for what the window is short of, not for a full page", h.sent.length === 1);
    t.check(
      "and asks from the offset the page's oldest line begins at",
      h.sent[0].before === 4096 && h.sent[0].lines === 2000
    );

    // A second ask while the first is in flight would queue a thousand backwards seeks.
    h.api.fill();
    t.check("one request in flight at a time", h.sent.length === 1);

    // The page lands, and the window is still short by five hundred.
    h.deliver(1500, 1024);
    h.api.fill();
    t.check("the walk continues while the window is short", h.sent.length === 2);
    t.check(
      "asking from the new head, for the remainder",
      h.sent[1].before === 1024 && h.sent[1].lines === 500
    );

    h.deliver(500, 512);
    h.api.fill();
    t.check("and stops of its own accord once the window is full", h.sent.length === 2);
  }
  {
    // The whole feed walks in full pages, since there is no number to be short of.
    const h = harness({ rows: 500, start: 4096 });
    h.api.setWindow(Infinity);
    t.check("the whole feed asks for a full page", h.sent[0].lines === 5000);
  }
  {
    // Nothing earlier exists, so nothing is asked for. The stopping condition of every walk.
    const h = harness({ rows: 500, start: 0 });
    h.api.setWindow(2500);
    t.check("a page holding the start of the file asks for nothing", h.sent.length === 0);
    h.api.setProgress();
    t.check(
      "and says so",
      h.progress.textContent === "The whole feed is loaded: 500 lines."
    );
  }
  {
    // **An empty backfill page is an answer**, and dropping it is a walk that asks for the same
    // region once per frame, forever.
    const h = harness({ rows: 500, start: 4096 });
    h.api.prepend([], 0);
    h.api.fill();
    t.check(
      "an empty page re-anchors the head rather than leaving it asking forever",
      h.sent.length === 0 && h.log.rows[0].dataset.start === "0"
    );
  }

  // --- A PAGE THAT CANNOT SAY WHERE IT BEGINS ---------------------------------------------------
  {
    // The ordinary condition of a page that has been open on a busy room: the trim has eaten into
    // the oldest batch, so the head carries no offset and there is nothing to walk back from.
    const h = harness({ rows: 500, start: undefined, cursor: 200000 });
    h.api.setWindow(2500);
    t.check("a trimmed page rebuilds rather than walking into a hole", h.sent.length === 1);
    t.check(
      "asking for the window that ends at the newest record it has seen",
      h.sent[0].before === 200000 && h.sent[0].lines === 2500
    );
    t.check("and throws the old window away first", h.log.rows.length === 0);
  }
  {
    // Nothing has landed yet. The replay is on its way and will anchor the page itself, so a
    // rebuild here would throw away a window that is about to arrive.
    const h = harness({ rows: 0 });
    h.api.setWindow(2500);
    t.check("an empty page asks for nothing", h.sent.length === 0);
  }

  // --- NARROWING --------------------------------------------------------------------------------
  {
    const h = harness({ rows: 1000, start: 0 });
    h.log.scrollTop = 600 * ROW;
    h.api.setWindow(500);
    t.check("narrowing trims to the window", h.log.rows.length === 500);
    t.check(
      "keeping the reader's line under the reader's eye",
      h.log.scrollTop === 100 * ROW
    );
    t.check("and asks the server for nothing at all", h.sent.length === 0);
    t.check("the count above the feed is refreshed", h.notes() === 1);
    t.check("and a reader who was not at the bottom is not sent there", h.pins() === 0);
  }
  {
    const h = harness({ rows: 1000, start: 0, atBottom: true });
    h.api.setWindow(500);
    t.check("a reader who was following stays at the bottom", h.pins() === 1);
  }
  {
    // **The common path, and it is one row wide.** Every batch opens with a day heading, so a
    // window filled exactly to its cap is one row over it and the first thing the trim reaches is
    // that heading, which is where the batch's offset was written. A heading holds no bytes, so
    // the offset is equally true of the record below it and moves down: without that, the first
    // paint of every page throws its own anchor away and every widening after it rebuilds.
    // A window of 1,000 records, as it lands: a heading carrying the batch's offset, then the
    // records. One row over the cap, so the trim takes the heading and nothing else.
    const h = harness({ rows: 0, store: { "journal.lines": "2500" } });
    h.log.rows.push(h.row(4096, true));
    for (let i = 0; i < 1000; i++) h.log.rows.push(h.row());
    h.api.setWindow(1000);
    t.check("the trim takes the heading and stops there", h.log.rows.length === 1000);
    t.check(
      "handing its offset to the record below it",
      h.log.rows[0].dataset.start === "4096"
    );
    h.api.setWindow(2500);
    t.check(
      "so the first widening after a page loads extends the window rather than rebuilding it",
      h.sent.length === 1 && h.sent[0].before === 4096
    );
  }
  {
    // The trim is what returns a page to a bounded document without reloading it, which the old
    // whole-feed button could not do at all.
    const h = harness({ rows: 4000, start: 0 });
    h.api.setWindow(Infinity);
    t.check("the whole feed holds everything", h.log.rows.length === 4000);
    h.api.setWindow(1000);
    t.check("and trimming comes back on the moment a size is chosen", h.log.rows.length === 1000);
  }
};
