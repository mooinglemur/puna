// `options-form.js` shows a dependent control only under the option it belongs to.
//
// **The property is about a control, not about prose, and that is why it is worth a test.** The
// hints beside it are paragraphs: hiding the wrong one costs a reader an explanation. This hides a
// form field, and the field goes on being submitted while hidden, so the two failure directions are
// not symmetric:
//
//   1. Left visible under the wrong option, an organizer ticks a box that applies to nothing and
//      believes they configured something. The room opens handing out every password.
//   2. Hidden under the RIGHT option, the setting becomes unreachable for anybody using the page,
//      and the only way to reach it is the bulk panel on a room that already exists.
//
// Both render correctly, log nothing and look like the feature simply not working.
//
// Lifted by slicing the file rather than imported, for the reason `table-filter.test.js` gives:
// the file is an IIFE over live DOM lookups, so there is nothing to export. The slice is bounded by
// two comments, so a rename fails loudly here instead of silently testing nothing.
"use strict";

const fs = require("fs");
const path = require("path");

const source = path.join(__dirname, "..", "..", "static", "options-form.js");
const FROM = "    // Which option of a group is selected right now";
const TO = "    // **Dirt is measured against";

function lift() {
  const src = fs.readFileSync(source, "utf8");
  const start = src.indexOf(FROM);
  const end = src.indexOf(TO);
  if (start < 0 || end < 0) {
    throw new Error(
      "options-form.js no longer contains the block this test lifts (looked for `" +
        FROM.trim() +
        "` and `" +
        TO.trim() +
        "`)"
    );
  }
  return src.slice(start, end);
}

// The smallest thing `showDependents` can work on: a radio group with one option checked, and a
// field that declares which group and which value it belongs to.
//
// `checked` is a real value here rather than a selector result, because the whole question is what
// the function does with the option that is currently chosen.
function harness(groupName, checkedValue, dependents) {
  const radios = ["none", "room", "per_slot"].map(function (value) {
    return { name: groupName, value: value, checked: value === checkedValue };
  });

  const fields = dependents.map(function (d) {
    return {
      hidden: false,
      dataset: { onlyIn: d.onlyIn, onlyFor: d.onlyFor },
    };
  });

  const form = {
    querySelector: function (selector) {
      // The only selector the lifted code builds: `input[name="<group>"]:checked`.
      const match = /^input\[name="(.+)"\]:checked$/.exec(selector);
      if (!match) throw new Error("unexpected selector: " + selector);
      const found = radios.filter(function (r) {
        return r.name === match[1] && r.checked;
      });
      return found.length ? found[0] : null;
    },
  };

  const groups = [];
  const scope = { form: form, groups: groups, dependents: fields };
  // `groups` is empty, so `showHints` is a no-op here: the hints have their own behavior and this
  // is about the control beside them.
  const body = new Function(
    "form",
    "groups",
    "dependents",
    lift() + "\n return { showHints: showHints, showDependents: showDependents };"
  );
  scope.api = body(form, groups, fields);
  scope.fields = fields;
  scope.radios = radios;
  return scope;
}

exports.run = function (t) {
  // --- THE OPTION IT BELONGS TO -----------------------------------------------------------------
  {
    const h = harness("slot_auth", "per_slot", [
      { onlyIn: "slot_auth", onlyFor: "per_slot" },
    ]);
    h.api.showDependents();
    t.check("shown under the option it declares", h.fields[0].hidden === false);
  }

  // --- AND EVERY OTHER OPTION OF THE SAME GROUP -------------------------------------------------
  // Enumerated rather than spot-checked: "anything other than per-slot" is the actual requirement,
  // and a predicate that happened to compare against only one of them would pass a single case.
  for (const mode of ["none", "room"]) {
    const h = harness("slot_auth", mode, [
      { onlyIn: "slot_auth", onlyFor: "per_slot" },
    ]);
    h.api.showDependents();
    t.check("hidden under `" + mode + "`", h.fields[0].hidden === true);
  }

  // --- A GROUP WITH NOTHING CHECKED -------------------------------------------------------------
  // Not reachable from the two forms, both of which render a checked radio, and asserted anyway
  // because the answer must not be "show it": a field whose governing option cannot be read is one
  // nobody has chosen, and the hints beside it resolve the same case the same way.
  {
    const h = harness("slot_auth", null, [
      { onlyIn: "slot_auth", onlyFor: "per_slot" },
    ]);
    h.api.showDependents();
    t.check("hidden when nothing in the group is checked", h.fields[0].hidden === true);
  }

  // --- IT COMES BACK ----------------------------------------------------------------------------
  // `refresh` runs this on every `change`, so hiding has to be reversible: a field hidden once and
  // left hidden is the second failure direction in the note at the top of this file, and it would
  // look exactly like the setting having been removed.
  {
    const h = harness("slot_auth", "none", [
      { onlyIn: "slot_auth", onlyFor: "per_slot" },
    ]);
    h.api.showDependents();
    const wentAway = h.fields[0].hidden === true;

    for (const radio of h.radios) radio.checked = radio.value === "per_slot";
    h.api.showDependents();
    t.check("comes back when the option is selected again", wentAway && h.fields[0].hidden === false);
  }

  // --- IT READS THE GROUP NAMED ON THE FIELD ----------------------------------------------------
  // Both halves are declared at the point of use, so a field naming a group that is not the one
  // whose value happens to match must not be shown by coincidence.
  {
    const h = harness("slot_auth", "per_slot", [
      { onlyIn: "patch_policy", onlyFor: "per_slot" },
    ]);
    h.api.showDependents();
    t.check(
      "a field naming another group is not shown by a value match",
      h.fields[0].hidden === true
    );
  }

  // --- SEVERAL FIELDS, DECIDED INDEPENDENTLY ----------------------------------------------------
  {
    const h = harness("slot_auth", "room", [
      { onlyIn: "slot_auth", onlyFor: "per_slot" },
      { onlyIn: "slot_auth", onlyFor: "room" },
    ]);
    h.api.showDependents();
    t.check(
      "each field answers for its own option",
      h.fields[0].hidden === true && h.fields[1].hidden === false
    );
  }
};
