// `table.js` applies the filters it finds already set, rather than waiting for an event.
//
// **This is a property about initial state, which is the one thing a source lint reads badly.** The
// shipped bug was that `attach` wired an `input` listener and a `change` listener and stopped: every
// assertion anybody would write about that file passed, because the listeners were there and correct
// and the filtering they ran was correct too. What was missing is that neither event ever fires for
// a value the *browser* restored.
//
// Two reports, one cause:
//
//   1. "only my slots" comes back ticked after a reload and the slots are not filtered.
//   2. A typed search expression survives a moderation action, since `moderation.js` reloads the
//      page once a command lands, and the table comes back showing every row.
//
// Both are session-history form restoration, which sets `value` and `checked` directly and fires
// nothing, because nothing was input and nothing changed.
//
// Lifted by slicing the file rather than imported, for the reason `journal-follow.test.js` gives:
// `table.js` is an IIFE over live DOM lookups, so there is nothing to export. The slice is bounded
// by two comments so a rename fails loudly here instead of silently testing nothing.
"use strict";

const fs = require("fs");
const path = require("path");

const source = path.join(__dirname, "..", "..", "static", "table.js");
const FROM = "  // Numeric when every value on both sides parses";
const TO = "  // Wire every sortable table under `root`";

function lift() {
  const src = fs.readFileSync(source, "utf8");
  const start = src.indexOf(FROM);
  const end = src.indexOf(TO);
  if (start < 0 || end < 0) {
    throw new Error(
      "table.js no longer contains the block this test lifts (looked for `" +
        FROM.trim() +
        "` and `" +
        TO.trim() +
        "`)"
    );
  }
  return src.slice(start, end);
}

// The smallest thing `attach` can work on: a table with three rows, one of them the viewer's, plus
// whichever controls the caller says are on the page and what state they arrived in.
//
// The headers list is empty on purpose. Sorting is wired here but never runs unless a header is
// clicked, and this is about what happens with nobody clicking anything.
function harness(controls) {
  const rows = [
    { textContent: "Lemur   A Link to the Past", dataset: { mine: "" }, hidden: false },
    { textContent: "Yacht   Super Metroid", dataset: {}, hidden: false },
    { textContent: "Mongoose   Timespinner", dataset: {}, hidden: false },
  ];

  const search = controls.search === undefined ? null : { value: controls.search, listeners: {} };
  const onlyMine = controls.mine === undefined ? null : { checked: controls.mine, listeners: {} };
  for (const el of [search, onlyMine]) {
    if (el) el.addEventListener = (name, fn) => (el.listeners[name] = fn);
  }

  const table = {
    id: "slots",
    tBodies: [{ rows, appendChild() {} }],
    querySelectorAll: () => [],
  };
  const document = {
    querySelector(selector) {
      if (selector.indexOf("data-filters") !== -1) return search;
      if (selector.indexOf("data-only") !== -1) return onlyMine;
      return null;
    },
  };

  const make = new Function("document", lift() + "\nreturn attach;");
  return {
    rows,
    search,
    onlyMine,
    attach: () => make(document)(table),
    // What the reader can actually see, which is the only thing either report was about.
    visible: () => rows.filter((row) => !row.hidden).length,
  };
}

exports.run = function (t) {
  // --- REPORT 1: a restored checkbox ------------------------------------------------------------
  // `checked` is set by the browser before any script runs, and `change` does not fire for it.
  {
    const h = harness({ mine: true });
    h.attach();
    t.check(
      "a checkbox that arrives ticked filters the table without being clicked",
      h.visible() === 1
    );
  }

  // --- REPORT 2: a restored search box ----------------------------------------------------------
  // `moderation.js` reloads once a command lands, so this is the state every moderation action
  // leaves behind on a page where somebody had searched for the slot they were acting on.
  {
    const h = harness({ search: "metroid" });
    h.attach();
    t.check(
      "a search box that arrives populated filters the table without being typed in",
      h.visible() === 1 && h.rows[1].hidden === false
    );
  }

  // Both at once, and they intersect rather than override: the searched row is not the viewer's, so
  // nothing survives. One place decides visibility, which is what stops the two controls fighting.
  {
    const h = harness({ search: "metroid", mine: true });
    h.attach();
    t.check("both restored controls apply together", h.visible() === 0);
  }

  // --- AND IT MUST NOT FILTER WHAT NOBODY ASKED TO FILTER ---------------------------------------
  // The other direction, which matters because the fix runs unconditionally: an empty box and an
  // unticked checkbox have to leave every row alone, or a page nobody has touched comes up blank.
  {
    const h = harness({ search: "", mine: false });
    h.attach();
    t.check("controls that arrive empty hide nothing", h.visible() === 3);
  }

  // A table with no controls at all, which is most of them. `refilter` still runs and must be a
  // no-op rather than a crash on a null it did not check.
  {
    const h = harness({});
    let threw = false;
    try {
      h.attach();
    } catch (e) {
      threw = true;
    }
    t.check("a table with no filter controls attaches and shows everything", !threw && h.visible() === 3);
  }

  // --- THE EVENTS STILL WORK ---------------------------------------------------------------------
  // The fix adds a call; it must not have replaced the listeners that handle somebody typing.
  {
    const h = harness({ search: "" });
    h.attach();
    h.search.value = "timespinner";
    h.search.listeners.input();
    t.check("typing into the box still filters", h.visible() === 1);
  }
};
