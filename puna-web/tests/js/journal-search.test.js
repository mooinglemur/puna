// The feed's search box, which keeps filtering while the feed keeps arriving.
//
// **A sequence over rows, which is why this is not a source lint.** The checkbox filters are a class
// on the feed and a stylesheet rule, so a lint can hold the two spellings together and be done. A
// substring cannot be a selector, so this half is a decision made per row in two places, and the
// interesting properties are about what happens between them:
//
//   * a row arriving while a needle is set must be matched against it, or the feed keeps tailing
//     and quietly stops obeying the filter for exactly the lines somebody is watching for;
//   * a needle that changes must re-judge rows that were already there, in both directions: a
//     narrowed search hides more, a widened one has to bring rows back;
//   * a row the search hides must be counted as hidden, or the "showing N of M" line contradicts
//     the feed above it.
//
// Lifted by slicing rather than imported, for the reason `journal-follow.test.js` gives: `journal.js`
// is an IIFE over live DOM lookups, so there is nothing to export. `journal-filters.test.js` lifts
// the block below this one and covers the checkbox half; the two share the `needle` this owns,
// which is why that harness passes one in.
"use strict";

const fs = require("fs");
const path = require("path");

const source = path.join(__dirname, "..", "..", "static", "journal.js");
const FROM = "  // What the reader typed, reduced to what a row is compared against.";
const TO = "  function marks(event) {";

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

// A row as the feed builds one: its rendered text, cached lower-cased at build time, and a class
// list the stylesheet keys off.
function row(text, unfilterable) {
  const classes = new Set(unfilterable ? ["entry", "unfilterable"] : ["entry"]);
  return {
    searchText: text.toLowerCase(),
    classList: {
      contains: (c) => classes.has(c),
      toggle: (c, on) => (on ? classes.add(c) : classes.delete(c)),
    },
    hidden: () => classes.has("unmatched"),
  };
}

function harness() {
  const rows = [
    row("[12:00:01] Lemur sent Progressive Sword to Yacht (Sanctuary)"),
    row("[12:00:04] Yacht found their Bow (Eastern Palace)"),
    row("[12:00:09] Mongoose sent Bombos to Lemur (Desert Palace)"),
    // pahoa's "records were dropped here" marker, and a day heading: neither is a record, and both
    // are exempt from every filter on this page.
    row("⚠ 12 records were dropped here", true),
  ];
  const log = { children: rows };

  // `setNeedle` is the app's own, not the harness's: trimming and lower-casing are the properties
  // being checked below, and a harness that normalized on the way in would assert them of itself.
  const api = new Function(
    "log",
    "needle",
    lift() +
      "\nreturn { applySearch: applySearch, refreshSearch: refreshSearch," +
      " setNeedle: setNeedle };"
  )(log, "");

  return {
    rows,
    log,
    search(text) {
      api.setNeedle(text);
      api.refreshSearch();
    },
    // A row arriving on a live feed, matched at build time the way `line` does it.
    arrive(text) {
      const fresh = row(text);
      api.applySearch(fresh);
      rows.push(fresh);
      return fresh;
    },
    visible: () => rows.filter((r) => !r.hidden()).length,
  };
}

exports.run = function (t) {
  // --- THE ORDINARY CASE ------------------------------------------------------------------------
  {
    const h = harness();
    t.check("no needle hides nothing", h.visible() === 4);

    h.search("lemur");
    t.check(
      "a needle hides the rows that do not carry it",
      h.visible() === 3 && h.rows[1].hidden()
    );
    // Case-insensitive, and matched against what is on screen: a location and a time of day are
    // both in the row's text, so both are searchable.
    h.search("  SANCTUARY  ");
    t.check(
      "matching ignores case, and the box's own stray spaces",
      h.visible() === 2 && !h.rows[0].hidden()
    );
    h.search("12:00:0");
    t.check("a time of day is searchable, because it is on screen", h.visible() === 4);
  }

  // --- THE EXEMPTION --------------------------------------------------------------------------
  // The gap marker is the only evidence a history is incomplete, and the day headings are what stop
  // a filtered feed reading as one where the clock runs backwards. Neither may be searched away.
  {
    const h = harness();
    h.search("nothing whatsoever matches this");
    t.check(
      "a needle that matches nothing still leaves the unfilterable rows",
      h.visible() === 1 && !h.rows[3].hidden()
    );
  }

  // --- THE FEED KEEPS ARRIVING ------------------------------------------------------------------
  // The property the whole design turns on: a search does not stop the tail, and a line arriving
  // after the box was typed into is judged by it rather than let through.
  {
    const h = harness();
    h.search("lemur");
    const shown = h.arrive("[12:01:00] Lemur sent Hookshot to Mongoose (Swamp Palace)");
    const hiddenOne = h.arrive("[12:01:02] Yacht found their Flippers (Lake Hylia)");
    t.check("a matching line that arrives later is shown", !shown.hidden());
    t.check("a line that arrives later and does not match is hidden", hiddenOne.hidden());
    // Which is the pair the count is made of: the total rose by two, the shown count by one.
    t.check("so the total grows while the shown count does not", h.visible() === 4);
  }

  // --- WIDENING BRINGS ROWS BACK ----------------------------------------------------------------
  // The direction a one-way implementation gets wrong: setting the class on a miss and never
  // clearing it leaves a row hidden by a needle nobody is searching for any more, and clearing the
  // box would leave the feed permanently short.
  {
    const h = harness();
    h.search("bombos");
    t.check("narrow", h.visible() === 2);
    h.search("");
    t.check("clearing the box brings every row back", h.visible() === 4);
    h.search("sword");
    h.search("o");
    t.check("widening the needle brings rows back too", h.visible() === 4);
  }
};
