// The viewer's own slot names, marked wherever they appear in the feed.
//
// **Why this is not a source lint.** The class is applied by one helper, which a lint could pin, but
// the property is about *which* of the names on a line gets it, and the interesting case is a line
// carrying two: a check between the viewer and somebody else must mark one name and not the other,
// in both directions, and a check between two other players must mark neither. None of that is
// visible in the text of the file, and all of it is one comparison away from being wrong in a way
// that reads as the feature simply not working.
//
// The other half is the one that must not regress at all: **an anonymous or non-participant viewer
// sees exactly what they always saw.** That is not a branch anywhere, it falls out of `MY_SLOTS`
// being empty, which is precisely the kind of correctness-by-construction worth a test, because
// nothing about the code says it.
//
// Lifted by slicing rather than imported, for the reason `journal-follow.test.js` gives.
"use strict";

const fs = require("fs");
const path = require("path");

const source = path.join(__dirname, "..", "..", "static", "journal.js");

// Two blocks: the marker helpers (`isMine` lives with the filter predicates) and `name`/`who`, which
// sit up with the cell builders. Sliced separately rather than as one span, because everything
// between them is the record renderer.
const CUTS = [
  ["  function isMine(slot) {", "  // **Any record naming one of your slots**"],
  ["  // A person's name, marked when the slot behind it is one of the viewer's.", "  // **What the sending client said its name was"],
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

function harness(mySlots) {
  const cells = [];
  // The real `cell` builds a DOM node; this records what class it was asked for, which is the whole
  // of what `name` decides.
  const cell = (row, text, className) => {
    const made = { text, className };
    cells.push(made);
    return made;
  };
  const api = new Function(
    "MY_SLOTS",
    "cell",
    lift() + "\nreturn { name: name, who: who };"
  )(mySlots, cell);
  return {
    cells,
    // What class the name cell came out with, given the record's own fields.
    check(finder, receiver) {
      cells.length = 0;
      api.name(null, "finder", finder);
      api.name(null, "receiver", receiver);
      return cells.map((c) => c.className);
    },
    subject(event) {
      cells.length = 0;
      api.who(null, event);
      return cells[0];
    },
  };
}

exports.run = function (t) {
  // --- A PARTICIPANT, HOLDING SLOTS 3 AND 11 ----------------------------------------------------
  {
    const h = harness([3, 11]);

    t.check(
      "the viewer's name is marked when they are the subject",
      h.check(3, 7)[0] === "who mine"
    );
    t.check(
      "and when they are the RECIPIENT, which is the half a player watches for",
      h.check(7, 11)[1] === "who mine"
    );
    // The line that carries two names, which is where a comparison against the wrong field shows.
    t.check(
      "the other player on the same line is not marked",
      h.check(3, 7)[1] === "who" && h.check(7, 11)[0] === "who"
    );
    t.check(
      "a check between two other players marks neither",
      h.check(7, 8).every((c) => c === "who")
    );
    // Both ends theirs: a player finding something of their own, the most ordinary line there is.
    t.check(
      "both names are marked when the viewer is both ends",
      h.check(3, 3).every((c) => c === "who mine")
    );

    // `who` is what the other nine record types render through, and it carries the slot number as
    // its fallback text. The MARK has to come from the slot, not from whether a name was known.
    t.check(
      "a subject rendered through `who` is marked from its slot",
      h.subject({ player: "Kai", slot: 11 }).className === "who mine"
    );
    t.check(
      "including where the room never learned a name",
      h.subject({ slot: 3 }).className === "who mine" &&
        h.subject({ slot: 3 }).text === "slot 3"
    );
    t.check(
      "and somebody else's record is left alone",
      h.subject({ player: "Yacht", slot: 7 }).className === "who"
    );
  }

  // --- ANONYMOUS, AND A SIGNED-IN VIEWER WHO PLAYS NOTHING HERE ---------------------------------
  // Same state on the wire: the page renders `data-my-slots` empty for both, so `MY_SLOTS` is empty
  // and nothing is marked. No change for them at all, which is the requirement.
  {
    const h = harness([]);
    t.check(
      "an outsider sees no name marked, on either end",
      h.check(3, 11).every((c) => c === "who")
    );
    t.check(
      "and none through the subject path",
      h.subject({ player: "Kai", slot: 3 }).className === "who"
    );
  }

  // A slot number this build did not get as a number cannot match anything: the guard is `isMine`'s
  // and it is what stops a record with a missing field marking every name on the page.
  {
    const h = harness([3]);
    t.check(
      "a record with no slot number marks nothing",
      h.subject({ player: "Kai" }).className === "who"
    );
  }
};
