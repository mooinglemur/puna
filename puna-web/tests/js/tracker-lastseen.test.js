// The tracker's **Last seen** column, which on an enhanced tracker speaks for two events rather
// than one: the room's own activity timer, and when the slot's player last wrote a progression or a
// note. It shows whichever is more recent.
//
// **Every failure here is silent and none of it is reachable from Rust.** The column renders a
// plausible timestamp whatever this logic does, so getting the comparison backwards shows the older
// of the two and looks exactly like a quiet room; tinting the wrong branch attaches a progression's
// color to the room's check time; and sorting the raw field while displaying the other produces a
// column where "just now" sits below "3d ago", which reads as a broken sort rather than as a wrong
// one. Nothing in the Rust build parses this file.
//
// Lifted by slicing rather than imported, for the reason `table-filter.test.js` gives: `tracker.js`
// is an IIFE over live DOM lookups, so there is nothing to export. The slice is bounded by two
// comments so a rename fails loudly here instead of silently testing nothing.
"use strict";

const fs = require("fs");
const path = require("path");

const source = path.join(__dirname, "..", "..", "static", "tracker.js");
const FROM = "  // The freshest activity among `rows`";
const TO = "  // --- state, persisted in the fragment";

function lift() {
  const src = fs.readFileSync(source, "utf8");
  const start = src.indexOf(FROM);
  const end = src.indexOf(TO);
  if (start < 0 || end < 0) {
    throw new Error(
      "tracker.js no longer contains the block this test lifts (looked for `" +
        FROM.trim() +
        "` and `" +
        TO.trim() +
        "`)"
    );
  }
  return src.slice(start, end);
}

// `lastResponseAt` is when the document arrived, and `age` adds the time since to keep the
// shorthand ticking between polls. Anchoring it at *now* makes that drift term zero, so the text
// assertions below are about the age they are given rather than about how long this test took.
//
// `PunaTime` is stubbed rather than loaded: this is about which instants reach the tooltip and in
// which order, and `localtime.js` owns how one is spelled. A test that asserted the spelling here
// would be a second opinion on that, which is the thing its own lint exists to prevent.
function harness(options) {
  const lastResponseAt = Date.now();
  const window = options && options.noLocaltime ? {} : { PunaTime: { absolute: (ms) => "@" + ms } };
  const lifted = new Function(
    "lastResponseAt",
    "window",
    lift() + "\nreturn { lastSeen, freshest, mostRecent, age };"
  )(lastResponseAt, window);
  return Object.assign({ at: (msAgo) => "@" + (lastResponseAt - msAgo) }, lifted);
}

const MINUTE = 60 * 1000;
const DAY = 24 * 60 * MINUTE;

// Comfortably inside their buckets, so a millisecond of drift between building the harness and
// running a check cannot move one across a boundary and fail for nothing.
const RECENT = 90 * 1000; // "1m ago"
const OLD = 3 * DAY; // "3d ago"
const ANCIENT = 5 * DAY; // "5d ago"

const bk = { label: "BK", tone: "bk" };

function row(checked, annotated, progression) {
  const r = { last_activity_ms_ago: checked };
  if (annotated !== undefined) r.annotated_ms_ago = annotated;
  if (progression) r.progression = progression;
  return r;
}

exports.run = function (t) {
  const h = harness();

  // --- the column nobody's annotations touch ----------------------------------------------------
  //
  // A viewer who may not see annotations is sent no `annotated_ms_ago` at all, so this is also the
  // anonymous reader's whole experience of the change: the column it always had, and a tooltip with
  // no labels on it, because there is only one thing in it to label.
  {
    const cell = h.lastSeen(row(OLD, undefined));
    t.check("with no annotation the cell is the check time", cell.text === "3d ago");
    t.check(
      "with no annotation the tooltip is the bare instant, unlabeled",
      cell.title === h.at(OLD)
    );
    t.check("with no annotation nothing is tinted", !cell.class);
  }

  // --- the annotation is newer ------------------------------------------------------------------
  {
    const cell = h.lastSeen(row(OLD, RECENT, bk));
    t.check("a newer annotation replaces the check time", cell.text === "1m ago");
    t.check("and carries the progression's own tone", cell.class === "prog-bk");
    t.check(
      "and the tooltip names both, check first",
      cell.title ===
        "Last check: " + h.at(OLD) + "\nNotes/progression updated: " + h.at(RECENT)
    );
  }

  // --- the check is newer -----------------------------------------------------------------------
  //
  // The tooltip still carries both: which of the two is fresher is not something a reader should
  // have to work out from whether a second line appeared.
  {
    const cell = h.lastSeen(row(RECENT, ANCIENT, bk));
    t.check("a newer check keeps the column on the check time", cell.text === "1m ago");
    t.check(
      "and is NOT tinted, because the tint describes an annotation",
      !cell.class
    );
    t.check(
      "and the tooltip still names both",
      cell.title ===
        "Last check: " + h.at(RECENT) + "\nNotes/progression updated: " + h.at(ANCIENT)
    );
  }

  // --- somebody cleared their status ------------------------------------------------------------
  //
  // Clearing is itself an edit, so the timestamp moves and the row stays recent. What it loses is
  // the color, which is the whole difference from the case above it: the server omits `progression`
  // for `unknown`, so this needs no case of its own in the renderer.
  {
    const cell = h.lastSeen(row(OLD, RECENT));
    t.check("a cleared progression keeps the fresher time", cell.text === "1m ago");
    t.check(
      "and is drawn like a row that never set one",
      !cell.class && !h.lastSeen(row(OLD, undefined)).class
    );
  }

  // --- annotated but never played ---------------------------------------------------------------
  //
  // The edge case the whole `null` discipline exists for: a real value beats never rather than
  // losing to it, and the tooltip says which half is missing instead of quietly showing one line.
  {
    const cell = h.lastSeen(row(null, RECENT, bk));
    t.check("an annotation on a slot that never checked is shown", cell.text === "1m ago");
    t.check("and is tinted", cell.class === "prog-bk");
    t.check(
      "and the tooltip says the check never happened",
      cell.title === "Last check: never\nNotes/progression updated: " + h.at(RECENT)
    );
  }

  // Neither: still never, still muted, and no tooltip claiming an instant that does not exist.
  {
    const cell = h.lastSeen(row(null, undefined));
    t.check("no check and no annotation is still never", cell.text === "never");
    t.check("and stays muted", cell.class === "hint");
  }

  // --- sorting, which must follow what is displayed ----------------------------------------------
  {
    t.check("freshest takes the smaller age", h.freshest(row(OLD, RECENT)) === RECENT);
    t.check("in both directions", h.freshest(row(RECENT, ANCIENT)) === RECENT);
    t.check("a real value beats never", h.freshest(row(null, RECENT)) === RECENT);
    t.check("in that direction too", h.freshest(row(RECENT, undefined)) === RECENT);
    t.check("and two nevers answer never", h.freshest(row(null, undefined)) === null);

    // The property the column actually needs, stated as itself: whatever the cell shows, the sort
    // key is the same number. Getting this wrong is the "just now below 3d ago" report.
    for (const r of [row(OLD, RECENT, bk), row(RECENT, ANCIENT, bk), row(null, RECENT, bk)]) {
      t.check(
        "the sort key is the age the cell displays",
        h.lastSeen(r).text === h.age(h.freshest(r)).text
      );
    }
  }

  // --- the footer -------------------------------------------------------------------------------
  //
  // Computed from the displayed rows so it cannot contradict them, which means reading the same
  // value the cells do. A footer on the raw activity timer would report a multiworld as quieter
  // than a row visible directly above it.
  {
    const rows = [row(OLD, ANCIENT), row(ANCIENT, RECENT)];
    t.check("the footer counts an annotation as activity", h.mostRecent(rows) === RECENT);
    t.check(
      "and still excludes never rather than treating it as now",
      h.mostRecent([row(null, undefined), row(OLD, undefined)]) === OLD
    );
    t.check("all never answers never", h.mostRecent([row(null, undefined)]) === null);
  }

  // --- without localtime.js ---------------------------------------------------------------------
  //
  // The tooltip degrades to absent, exactly as the single-line one already did, rather than to a
  // line reading "undefined" or to this file spelling an instant itself.
  {
    const bare = harness({ noLocaltime: true });
    const cell = bare.lastSeen(row(OLD, RECENT, bk));
    t.check("no localtime.js means no tooltip", !cell.title);
    t.check("but the cell still shows the right age", cell.text === "1m ago");
    t.check("and is still tinted", cell.class === "prog-bk");
  }
};
