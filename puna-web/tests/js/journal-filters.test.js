// A remembered journal filter cannot act on a room that does not offer it.
//
// **The keys are global and the controls are not.** `toggles.js` stores by key alone
// (`journal.gameplay-only`, `journal.personal`, `journal.hide-filler`), so one preference spans
// every room's feed. Two of the three boxes are rendered conditionally by `routes/journal.rs`:
// gameplay only where the socket is not already filtering, personal only where the viewer holds a
// slot in *that* room. So a reader who ticks one on a room where it is offered will, on a room
// where it is not, have a stored `true` and no input.
//
// The danger is a filter that applies anyway. `personal` marks a row when it names one of the
// viewer's slots, and on a room where they hold none nothing is ever marked, so a personal filter
// that fired there would hide the entire feed with no control on screen to turn it off. Silent,
// and indistinguishable from a room where nobody is playing.
//
// `applyFilters` reads the DOM rather than the store, so an absent box is off. That is the property
// here, and it is worth a running check rather than an argument: the whole point is what happens
// when a lookup returns null.
"use strict";

const fs = require("fs");
const path = require("path");

const source = path.join(__dirname, "..", "..", "static", "journal.js");
const FROM = "  var FILTERS = [";
const TO = "  if (filters) {";

function lift() {
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

// `boxes` is which inputs the page rendered, and what state `toggles.js` restored them to. An id
// that is absent from it does not exist on that page, which is the case under test.
function harness(boxes) {
  const classes = new Set();
  const log = {
    classList: {
      contains: (c) => classes.has(c),
      toggle: (c, on) => (on ? classes.add(c) : classes.delete(c)),
    },
    // Three rows and no headings, which is all the note needs to count.
    querySelectorAll: (selector) => (selector.indexOf(":not(.daybreak)") !== -1 ? [0, 1, 2] : []),
  };
  const filterNote = { textContent: "" };
  const document = { getElementById: (id) => (id in boxes ? { checked: boxes[id] } : null) };

  const make = new Function(
    "log",
    "filterNote",
    "filters",
    "document",
    lift() + "\nreturn applyFilters;"
  );
  make(log, filterNote, {}, document)();
  return { classes, note: filterNote.textContent };
}

exports.run = function (t) {
  // The room where the reader ticked it: the box is there and the filter applies.
  {
    const h = harness({ "journal-filter-personal": true });
    t.check("a remembered filter applies where its box is rendered", h.classes.has("only-personal"));
    t.check("and the feed is marked as filtered", h.classes.has("filtering"));
  }

  // **The room next door, where they hold no slot.** The preference is still `true` in
  // `localStorage`; the input is not on the page. Nothing may be hidden.
  {
    const h = harness({});
    t.check(
      "a remembered filter whose box this room does not render applies nothing",
      h.classes.size === 0
    );
    t.check("and says nothing, because nothing is hidden", h.note === "");
  }

  // Same shape for the gameplay filter, whose absence means the SOCKET is already filtering. Firing
  // it there would narrow an already-narrowed feed against a set the viewer cannot see or change.
  {
    const h = harness({ "journal-filter-gameplay": true });
    t.check("gameplay applies where it is offered", h.classes.has("only-gameplay"));
    t.check("and nowhere else", harness({}).classes.has("only-gameplay") === false);
  }

  // The one box every viewer gets, so it carries across rooms by design.
  {
    const h = harness({ "journal-filter-filler": true });
    t.check("the filler filter applies wherever it is offered", h.classes.has("hide-filler"));
  }

  // A box present and unticked is off, which is the ordinary case and the one that must not be
  // confused with "present" by a truthiness slip on the element itself.
  {
    const h = harness({ "journal-filter-personal": false, "journal-filter-filler": false });
    t.check("boxes that are rendered and unticked filter nothing", h.classes.size === 0);
  }
};
