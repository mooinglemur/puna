// The annotation dialog's character counter, which is also the thing that disables Save.
//
// **It counts in a different unit than JavaScript's own `length`**, and that is the whole reason
// this file exists. The route counts `chars()` and the column's CHECK counts `char_length`, both of
// which are code points; `value.length` is UTF-16 code units. An emoji is two of one and one of the
// other, so the obvious spelling disables Save on a note of 600 emoji over a limit the server would
// have accepted, and the only person who sees it is somebody whose note has an emoji in it.
//
// The `maxlength` attribute this replaced had the same defect in the other direction, cutting such
// a note off at about 500 characters with nothing said.
//
// Lifted by slicing rather than imported, for the reason `table-filter.test.js` gives: `tracker.js`
// is an IIFE over live DOM lookups, so there is nothing to export. The slice is bounded by two
// comments so a rename fails loudly here instead of silently testing nothing.
"use strict";

const fs = require("fs");
const path = require("path");

const source = path.join(__dirname, "..", "..", "static", "tracker.js");
const FROM = "    // **How much of the note is used, counted the way";
const TO = "    if (noteField) noteField.addEventListener";

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

// The four things `countNote` closes over, as the smallest stubs that can answer what it asks.
function harness(limit) {
  const counter = {
    textContent: "",
    hidden: true,
    classes: new Set(),
    classList: {
      toggle(name, on) {
        if (on) counter.classes.add(name);
        else counter.classes.delete(name);
      },
    },
  };
  const noteField = {
    value: "",
    attrs: {},
    setAttribute(name, value) {
      this.attrs[name] = value;
    },
  };
  const save = { disabled: false };
  const countNote = new Function(
    "counter",
    "noteField",
    "save",
    "limit",
    lift() + "\nreturn countNote;"
  )(counter, noteField, save, limit === undefined ? 1000 : limit);

  return {
    counter,
    noteField,
    save,
    type: (text) => {
      noteField.value = text;
      countNote();
    },
    over: () => counter.classes.has("over"),
  };
}

exports.run = function (t) {
  // --- an empty box, which is what opening the dialog on an unannotated slot looks like ----------
  {
    const h = harness();
    h.type("");
    t.check("an empty note counts zero", h.counter.textContent === "0 / 1000 characters");
    t.check("the counter is revealed by the script that maintains it", h.counter.hidden === false);
    t.check("nothing is red", !h.over());
    t.check("and Save is available", h.save.disabled === false);
  }

  // --- the cutoff is the route's: 1000 passes, 1001 does not ------------------------------------
  //
  // The route refuses `> MAX_NOTE_CHARS` and the column allows `<= 1000`, so the boundary character
  // is accepted. Off by one here would disable Save on a note the server would have taken.
  {
    const h = harness();
    h.type("x".repeat(1000));
    t.check("exactly the limit is not over", !h.over() && h.save.disabled === false);
    t.check("and reads as the limit", h.counter.textContent === "1000 / 1000 characters");

    h.type("x".repeat(1001));
    t.check("one past it is over", h.over());
    t.check("and Save goes away", h.save.disabled === true);
    t.check("and the field says so to a screen reader", h.noteField.attrs["aria-invalid"] === "true");
    t.check("and the count is the real one", h.counter.textContent === "1001 / 1000 characters");
  }

  // --- trimming puts it back --------------------------------------------------------------------
  //
  // There is no state to get stuck in: the same function runs on every keystroke, so the way out is
  // the way in.
  {
    const h = harness();
    h.type("x".repeat(1200));
    t.check("over, to begin with", h.save.disabled === true);
    h.type("x".repeat(900));
    t.check("trimming re-enables Save", h.save.disabled === false);
    t.check("and clears the red", !h.over());
    t.check("and the field is valid again", h.noteField.attrs["aria-invalid"] === "false");
  }

  // --- CODE POINTS, NOT CODE UNITS --------------------------------------------------------------
  //
  // The whole point. 600 emoji is `value.length === 1200` and `char_length === 600`, and the second
  // is what the database and the route measure. Counting the first refuses a note nothing else
  // would have refused, for a reason invisible to anybody whose notes are plain text.
  {
    const h = harness();
    h.type("\u{1F600}".repeat(600));
    t.check(
      "600 astral characters count as 600, not 1200",
      h.counter.textContent === "600 / 1000 characters"
    );
    t.check("so they are not over the limit", !h.over());
    t.check("and Save stays available", h.save.disabled === false);

    // And the same alphabet still goes over when it genuinely does, so the fix is not just
    // "never disable".
    h.type("\u{1F600}".repeat(1001));
    t.check("1001 of them is still over", h.over() && h.save.disabled === true);
  }

  // A combining pair is two code points and one glyph, which the database also counts as two. This
  // pins that the unit is the DATABASE's rather than "what looks like a character", since those
  // differ and only one of them is the thing that refuses.
  {
    const h = harness();
    h.type("é");
    // If an editor ever normalizes the line above into the precomposed character this fails rather
    // than quietly asserting something else: that form is one code point, and 1 is not 2.
    t.check(
      "a combining pair counts as two, as char_length does",
      h.counter.textContent === "2 / 1000 characters"
    );
  }

  // --- a missing limit does nothing rather than something wrong ---------------------------------
  //
  // `data-limit` is server-rendered; dropped, `Number(undefined)` is `NaN` and every comparison
  // against it is false, so an unguarded version would render "NaN" into the dialog and never
  // disable anything while looking like it was working.
  {
    const h = harness(NaN);
    h.type("x".repeat(5000));
    t.check("no limit leaves the counter hidden", h.counter.hidden === true);
    t.check("and renders nothing", h.counter.textContent === "");
    t.check("and does not disable Save on a number it does not have", h.save.disabled === false);
  }
};
