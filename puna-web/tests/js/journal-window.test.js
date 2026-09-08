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
  ["  // --- WHERE THE PAGE BEGINS", "  // Older records, on the front."],
  ["  function prepend(events, starts, start) {", "  // --- THE THREE VIEW FILTERS"],
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
function row(siblings, start, daybreak, text) {
  const made = {
    dataset: {},
    daybreak: !!daybreak,
    textContent: text || "",
    classList: { contains: (name) => name === "daybreak" && !!daybreak },
    get nextElementSibling() {
      const at = siblings.indexOf(made);
      return at >= 0 && at + 1 < siblings.length ? siblings[at + 1] : null;
    },
    remove() {
      const at = siblings.indexOf(made);
      if (at >= 0) siblings.splice(at, 1);
    },
  };
  if (typeof start === "number") made.dataset.start = String(start);
  return made;
}

// A record's width in the file, so a fixture's offsets are the real thing's shape: consecutive,
// increasing, and far enough apart that an off-by-one row is an off-by-a-hundred offset.
const RECORD = 100;

function makeLog(count, start) {
  const rows = [];
  // **Every row carries its own offset**, which is the property the whole design turns on: the trim
  // drops rows off the top and whatever is left still says where the page begins.
  for (let i = 0; i < count; i++) {
    rows.push(row(rows, typeof start === "number" ? start + i * RECORD : undefined));
  }
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
    get firstChild() {
      return rows.length ? rows[0] : null;
    },
    removeChild(node) {
      rows.splice(rows.indexOf(node), 1);
    },
    replaceChildren() {
      rows.length = 0;
    },
    appendChild(fragment) {
      rows.push.apply(rows, fragment.children);
    },
    // A fragment is a list of rows, so inserting one is a splice. `before` is always the head here.
    insertBefore(fragment, before) {
      const at = before === null ? rows.length : rows.indexOf(before);
      rows.splice.apply(rows, [at < 0 ? 0 : at, 0].concat(fragment.children));
    },
    // Two selectors are passed to this: the record one, and the day headings the dedup collects.
    querySelectorAll(selector) {
      return selector === ".daybreak"
        ? rows.filter((r) => r.daybreak)
        : rows.filter((r) => !r.daybreak);
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

  // Enough of a renderer to run `prepend` on real events. A record's `at` stands in for its day,
  // so a batch's calendar is readable straight off the fixture: what matters here is which rows are
  // headings and what day the batch ends on, not how either is drawn.
  const document = {
    createDocumentFragment() {
      const children = [];
      return {
        children,
        appendChild(node) {
          children.push(node);
        },
        get firstElementChild() {
          return children.length ? children[0] : null;
        },
      };
    },
  };
  const at = (value) => value;
  const dayKey = (when) => "day-" + when;
  // Both take the offset of the record they stand for, and a heading takes the one belonging to the
  // record it introduces. A stub that dropped it would report a page that cannot say where it
  // begins, which is a real state and the wrong one.
  const daybreak = (when, begins) => row(log.rows, begins, true, dayKey(when));
  const line = (event, arriving, begins) => row(log.rows, begins);

  const make = new Function(
    "log",
    "document",
    "at",
    "dayKey",
    "daybreak",
    "line",
    "socket",
    "WebSocket",
    "cursor",
    "cap",
    "backfilling",
    "loaded",
    "stuckToBottom",
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
    "followBottom",
    "refreshFilterNote",
    lift() +
      "\nreturn {" +
      "  fill: fill," +
      "  append: append," +
      "  prepend: prepend," +
      "  setWindow: setWindow," +
      "  setProgress: setProgress," +
      "  cap: function () { return cap; }," +
      // The reader's number: records on the page, maintained rather than queried. Exposed so the
      // tests can hold it against the document, which is the whole reason maintaining it is safe.
      "  loaded: function () { return loaded; }," +
      // The day the page ends on, which is what decides whether the next live record draws a
      // heading. It is a plain variable in the file and has no other way out.
      "  lastDay: function () { return lastDay; }," +
      // What the frame handler does when a backfill page arrives, and the only reason this is
      // exposed: the walk's continuation is a two-step sequence whose middle step lives in the
      // socket handler, which is not lifted here.
      "  landed: function () { backfilling = false; }" +
      "};"
  );

  const api = make(
    log,
    document,
    at,
    dayKey,
    daybreak,
    line,
    socket,
    { OPEN: 1 },
    settings.cursor === undefined ? 200000 : settings.cursor,
    1000,
    // backfilling, loaded, stuckToBottom, lastDay. `loaded` starts as whatever the fixture put on
    // the page, since that is what the running file's own counter would have reached.
    false,
    log.rows.filter((r) => !r.daybreak).length,
    true,
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
    () => {},
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
    row: (start, daybreak) => row(log.rows, start, daybreak),
    // A backfill page arriving, **through the real `prepend`** rather than by pushing rows onto the
    // list: it is what maintains the record count, and a helper that bypassed it would leave the
    // count and the document disagreeing for a reason the file is not responsible for.
    //
    // The records carry no timestamp, so no day headings are drawn and a page of N records is N
    // rows. That keeps the arithmetic in the walk tests about the window rather than about the
    // calendar.
    deliver(count, start) {
      const events = [];
      const starts = [];
      for (let i = 0; i < count; i++) {
        events.push({});
        starts.push(start + i * RECORD);
      }
      api.prepend(events, starts, start);
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
    // --- CANCELLING A WALK BY NARROWING ---------------------------------------------------------
    // Every button but the current one stays live while a walk is running, which is the point: a
    // whole-feed load on a busy room is dozens of round trips over tens of seconds, and a reader who
    // changes their mind has to be able to say so.
    const h = harness({ rows: 500, start: 4096 });
    h.api.setWindow(Infinity);
    h.api.setProgress();
    t.check(
      "a walk says it is loading, and how much it holds so far",
      h.progress.textContent === "Loading earlier records… 500 lines so far."
    );

    h.api.setWindow(500);
    t.check(
      "narrowing stops the loading message at once, rather than counting through a cancellation",
      h.progress.textContent === "Keeping the last 500 lines of history loaded."
    );

    // The page already on the wire cannot be unsent, and it lands into a window nobody wants.
    h.api.landed();
    h.api.fill();
    t.check("and no further page is asked for", h.sent.length === 1);
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
      h.progress.textContent === "The entire history is loaded: 500 lines."
    );
  }
  {
    // **An empty backfill page is an answer**, and dropping it is a walk that asks for the same
    // region once per frame, forever. The head's own offset is exact and is not zero, so nothing
    // else on the page could say that there is nothing before it.
    const h = harness({ rows: 500, start: 4096 });
    h.api.prepend([], null, 0);
    h.api.fill();
    t.check(
      "an empty page re-anchors the head rather than leaving it asking forever",
      h.sent.length === 0 && h.log.rows[0].dataset.start === "0"
    );
  }

  // --- THE TRIM MOVES THE ANCHOR, AND THAT IS THE WHOLE POINT -----------------------------------
  // A page open on a busy room trims records off the top continuously. With one offset per FRAME
  // that anchor went stale the moment it did, and widening had to throw the window away and fetch
  // it again from the end: a cleared feed, a lost place, and a duplicate day heading behind it.
  // One offset per RECORD means whatever is left at the top still says where the page begins.
  {
    const h = harness({ rows: 1000, start: 4096 });
    h.api.setWindow(500);
    t.check(
      "a trimmed page still knows where it begins, exactly",
      h.log.rows[0].dataset.start === String(4096 + 500 * RECORD)
    );

    h.api.setWindow(2500);
    t.check("so widening extends it rather than rebuilding", h.sent.length === 1);
    t.check(
      "walking back from the record now at the top",
      h.sent[0].before === 4096 + 500 * RECORD && h.sent[0].lines === 2000
    );
    t.check("with the window it already had left alone", h.log.rows.length === 500);
  }
  {
    // Nothing has landed yet. The replay is on its way and will anchor the page itself.
    const h = harness({ rows: 0 });
    h.api.setWindow(2500);
    t.check("an empty page asks for nothing", h.sent.length === 0);
  }
  {
    // A day heading is not a record and holds no bytes, so it takes the offset of the record it
    // introduces. Without that, a trim stopping on a heading would leave the page unable to say
    // where it begins, which is the state per-record offsets exist to make unreachable.
    const h = harness({ rows: 0 });
    h.log.rows.push(h.row(4096, true), h.row(4096), h.row(4196));
    h.api.setWindow(2);
    t.check(
      "a trim that stops on a day heading leaves the page anchored",
      h.log.rows.length === 2 && h.log.rows[0].dataset.start === "4096"
    );
  }
  {
    // `lastDay` describes the page's TAIL, and a backfill lands on the FRONT of a page whose bottom
    // has not moved. Claiming its own last day as the page's would drop the heading at the next real
    // day change, which is invisible until somebody reads a feed that spans midnight.
    const h = harness({ rows: 500, start: 8192 });
    h.api.prepend([{ at: 1 }], [4096], 4096);
    t.check(
      "a backfill leaves the day the page ends on alone",
      h.api.lastDay() === null
    );
  }
  {
    // **Each row takes ITS OWN offset, not the batch's.** Handing every row the offset the frame
    // began at is the shape this whole change removes: the page would go on claiming to start where
    // the batch did, however far the trim had eaten into it, and the walk would then ask for a
    // region that stops short of what is on screen. The day headings are what make it visible here,
    // since each takes the offset of the record below it.
    const h = harness({ rows: 500, start: 8192 });
    h.api.prepend([{ at: 1 }, { at: 1 }, { at: 2 }], [4096, 4196, 4296], 4096);
    t.check(
      "every row a backfill builds carries the offset of its own record",
      h.log.rows
        .slice(0, 5)
        .map((r) => r.dataset.start)
        .join(",") === "4096,4096,4196,4296,4296"
    );
  }

  // --- THE NUMBER BESIDE THE BUTTONS ------------------------------------------------------------
  // It says how much of the feed is on the page and whether more is coming, in every mode, and it
  // is live: a whole-feed window that has finished loading still has records arriving at the bottom
  // of it, and the count is the only thing on the page that shows the feed is still moving.
  {
    const h = harness({ rows: 500, start: 4096 });
    h.api.setProgress();
    t.check(
      "a window being held says what it is holding",
      h.progress.textContent === "Keeping the last 500 lines of history loaded."
    );

    h.api.append([{ at: 1 }, { at: 1 }], false, [9000, 9100]);
    t.check(
      "and a record arriving moves the number without anything being pressed",
      h.progress.textContent === "Keeping the last 502 lines of history loaded."
    );
  }
  {
    const h = harness({ rows: 500, start: 0 });
    h.api.setProgress();
    t.check(
      "a page holding the oldest record in the room says the whole feed is loaded",
      h.progress.textContent === "The entire history is loaded: 500 lines."
    );
    h.api.append([{ at: 1 }], false, [9000]);
    t.check(
      "and that number moves too, which is what says a loaded feed is still live",
      h.progress.textContent === "The entire history is loaded: 501 lines."
    );
  }
  {
    const h = harness({ rows: 1, start: 0 });
    h.api.setProgress();
    t.check(
      "one record is one line",
      h.progress.textContent === "The entire history is loaded: 1 line."
    );
  }

  // --- THE REPORTED BUG: A COUNTER THAT REMEMBERED THE LAST WALK --------------------------------
  // The note used to count the records a walk had pulled in, and nothing reset it when a new walk
  // began. Toggling between the whole feed and a fixed window added the old total to the new one, so
  // the number read wildly high and went on climbing from there. The count is the page's own record
  // count now, which has no such state to forget.
  {
    const h = harness({ rows: 500, start: 4096 });
    h.api.setWindow(Infinity);
    // A page lands and the walk continues, which is what the frame handler does with it.
    h.deliver(1000, 2048);
    h.api.fill();
    h.api.setProgress();
    t.check(
      "a walk counts what is on the page",
      h.progress.textContent === "Loading earlier records… 1500 lines so far."
    );

    // Back to a fixed window and out to the whole feed again: the same page, so the same number.
    h.api.setWindow(500);
    h.api.setWindow(Infinity);
    h.api.setProgress();
    t.check(
      "and going back out to the whole feed starts from what is there, not from the last walk",
      h.progress.textContent === "Loading earlier records… 500 lines so far."
    );
  }

  // --- THE COUNT AGREES WITH THE DOCUMENT -------------------------------------------------------
  // It is maintained rather than queried, because the note is written on every arriving frame and a
  // whole-feed window is 160,000 rows. That is the trade, and this is what makes it safe: the four
  // places that can change it, driven together, against a count taken off the document.
  {
    const h = harness({ rows: 0, store: { "journal.lines": "10000" } });
    const real = () => h.log.rows.filter((r) => !r.daybreak).length;

    h.api.append([{ at: 1 }, { at: 1 }, { at: 2 }], false, [100, 200, 300]);
    t.check("after an append", h.api.loaded() === real() && real() === 3);

    h.api.prepend([{ at: 0 }, { at: 0 }], [0, 50], 0);
    t.check("after a backfill", h.api.loaded() === real() && real() === 5);

    // Narrowing past what is on the page: the trim takes rows off the top, headings among them, and
    // only the records may count against the reader's number.
    h.api.setWindow(500);
    h.api.setWindow(4);
    t.check("after a trim that takes a day heading", h.api.loaded() === real());
    t.check("and the trim counted rows, not records", h.log.rows.length === 4);
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
    // A window as it lands on a first paint: a heading, then the records, one row over the cap.
    // The trim takes the heading, and what is left is anchored on its own record.
    const h = harness({ rows: 0, store: { "journal.lines": "2500" } });
    h.log.rows.push(h.row(4096, true));
    for (let i = 0; i < 1000; i++) h.log.rows.push(h.row(4096 + i * RECORD));
    h.api.setWindow(1000);
    t.check("the trim takes the heading and stops there", h.log.rows.length === 1000);
    h.api.setWindow(2500);
    t.check(
      "and the first widening after a page loads extends the window",
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
