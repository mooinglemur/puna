// "Exclude goal/100%", the multiworld tracker's answer to "who is still playing".
//
// **Every way of getting this wrong renders a plausible table**, which is why it is worth a test
// rather than a reading. A predicate that is too eager hides rows nobody asked it to and the reader
// has no way to tell the filter from the room: a spectator sits at `0 / 0` for the life of a room,
// so a bare `done >= total` calls every one of them complete on the first render, hides them behind
// a box labelled "goal/100%", and offers that box on a multiworld where nobody has finished
// anything. A predicate that is too shy leaves finished rows in the table, which looks like the box
// not working. And `offer` disagreeing with `exclude` is a control that appears when it can do
// nothing, or hides itself the moment it is ticked.
//
// Nothing in the Rust build parses this file. Lifted by slicing rather than imported, for the
// reason `table-filter.test.js` gives: `tracker.js` is an IIFE over live DOM lookups, so there is
// nothing to export. The slice is bounded at both ends so a rename fails loudly here instead of
// silently testing nothing.
"use strict";

const fs = require("fs");
const path = require("path");

const source = path.join(__dirname, "..", "..", "static", "tracker.js");
const FROM = "  // A slot with nothing left to do";
const TO = "  function percent(r) {";

function lift() {
  const src = fs.readFileSync(source, "utf8");
  const start = src.indexOf(FROM);
  const end = src.indexOf(TO);
  if (start < 0 || end < 0 || end < start) {
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

const isFinished = new Function(lift() + "\nreturn isFinished;")();

// The predicate the view declares for the box, kept in step with `tracker.js` by the lint in
// `tests/templates.rs`; this file is about what it answers rather than about where it is spelled.
const offer = (rows) => rows.some(isFinished);

function slot(status, done, total) {
  return { status, checks_done: done, checks_total: total };
}

exports.run = function (t) {
  // --- finished, by either route ----------------------------------------------------------------
  t.check("a goaled slot is finished", isFinished(slot("goal", 4, 12)));
  t.check("so is one that has checked everything", isFinished(slot("playing", 12, 12)));
  t.check(
    "a room ahead of its own seed still reads as finished rather than one short",
    isFinished(slot("playing", 13, 12))
  );

  // --- still playing ----------------------------------------------------------------------------
  t.check("one check short is not finished", !isFinished(slot("playing", 11, 12)));
  t.check("nor is a slot that has done nothing", !isFinished(slot("unknown", 0, 12)));

  // --- the spectator rule, which is the whole of the `checks_total > 0` guard -------------------
  //
  // A spectator owns no locations, so it is `0 / 0` forever. That is not completion, it is having
  // nothing to complete, and the column shows a dash rather than a percentage for exactly that
  // reason. Dropping the guard hides every spectator behind a box that never mentions them.
  t.check("a spectator has nothing to check, which is not the same as done", !isFinished(slot("unknown", 0, 0)));
  t.check(
    "and a spectator alone never offers the box",
    !offer([slot("unknown", 0, 0), slot("playing", 3, 12)])
  );

  // --- offer and exclude are the same question ---------------------------------------------------
  //
  // The box is revealed exactly when it would hide a row. Asked the other way round, either it
  // appears on a multiworld where it does nothing, or it stays away from one where it would help.
  t.check("nobody finished, no box", !offer([slot("playing", 1, 12), slot("unknown", 0, 12)]));
  t.check("one goal is enough", offer([slot("playing", 1, 12), slot("goal", 4, 12)]));
  t.check("so is one completed world", offer([slot("playing", 1, 12), slot("playing", 12, 12)]));
  t.check("an empty document offers nothing", !offer([]));

  // Whatever the box hides is exactly what its absence would have left, so a tick remembered from a
  // livelier room can never strand rows behind a control that is not on screen.
  {
    const rows = [slot("playing", 1, 12), slot("unknown", 0, 0)];
    t.check(
      "with the box unoffered, ticking it would filter nothing",
      !offer(rows) && rows.filter((r) => !isFinished(r)).length === rows.length
    );
  }
};
