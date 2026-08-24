// The traffic-filter rule table: rows added and struck out in place, saved in one go.
//
// ## What the server already does, and what this adds
//
// Every state this file touches is also rendered by `rooms/_rule_table.html`, and the page works
// with this script absent: a blank row is always present so one rule can be added per save, the
// remove control is a checkbox applied on save rather than a link needing a handler, and the
// tag/subtype cells arrive disabled or enabled to match the kind each row already has.
//
// So this is not the editor — it is the difference between one round trip per rule and none:
//
// * **more rows at once**, by revealing an Add button that is `hidden` in the markup. Hidden there
//   rather than styled away behind a root class, so a script that fails to load leaves no dead
//   control — the one blank row is still there and still works.
// * **live narrowing**: choosing a kind enables the cell that kind uses and disables the other.
//   A disabled input is not submitted, which is the point rather than a side effect: a tag left
//   over from when a row was a bounce cannot ride along and be refused.
// * **the unsaved-changes notice**, which cannot exist without a script at all — with none, every
//   change IS a submit and there is nothing outstanding to warn about.
// * **the empty-table question**, revealed the moment the last row is struck out rather than after
//   the round trip that would otherwise be the first time anybody is asked it.
//
// ## Which kind narrows with what comes from the markup, not from here
//
// Each `<option>` carries `data-narrows` rendered from the model's own `Kind::narrows_with`. A list
// of kinds in this file would be a second copy of a closed set that pahoa owns, and the copy that
// drifts is always the one nobody compiles.
(function () {
  "use strict";

  var forms = document.querySelectorAll("[data-rule-form]");
  if (!forms.length) return;

  // The row a new one is cloned from, captured before anything is bound to it.
  function blankRow(form) {
    var row = form.querySelector(".rule-new");
    return row ? row.cloneNode(true) : null;
  }

  // The highest `rules[N]` index in the form, so a new row gets one nothing else is using.
  //
  // Rocket starts a new element whenever the index changes, so the indices need only be distinct
  // between neighbours — but distinct everywhere is cheaper to reason about than distinct enough,
  // and it means removing a row never has to renumber the ones after it.
  function highestIndex(form) {
    var highest = -1;
    form.querySelectorAll("[data-rule-kind]").forEach(function (select) {
      var match = /^rules\[(\d+)\]/.exec(select.name || "");
      if (match) highest = Math.max(highest, parseInt(match[1], 10));
    });
    return highest;
  }

  // A row counts as a rule when it names something to drop and is not struck out. This is the same
  // question the server asks of a submission, and the two have to agree or the empty-table choice
  // appears at a different moment from the one it applies to.
  function isLive(row) {
    var kind = row.querySelector("[data-rule-kind]");
    var remove = row.querySelector("[data-rule-remove]");
    return !!(kind && kind.value && !(remove && remove.checked));
  }

  // Enable the cell this row's kind narrows with, disable the other.
  //
  // With no kind chosen, BOTH stay enabled: the row is not a rule yet, and disabling a field before
  // knowing whether it applies would leave somebody unable to type the tag they came to type.
  function narrow(row) {
    var kind = row.querySelector("[data-rule-kind]");
    var tag = row.querySelector("[data-rule-tag]");
    var subtype = row.querySelector("[data-rule-subtype]");
    if (!kind || !tag || !subtype) return;

    var chosen = kind.options[kind.selectedIndex];
    var narrows = chosen ? chosen.dataset.narrows || "" : "";
    var undecided = !kind.value;

    tag.disabled = !undecided && narrows !== "tag";
    subtype.disabled = !undecided && narrows !== "subtype";
    // Marked as well as disabled: `:disabled` styling alone is easy to miss in a dense table, and
    // the cell is what the reader is looking at rather than the control inside it.
    tag.parentNode.classList.toggle("inapplicable", tag.disabled);
    subtype.parentNode.classList.toggle("inapplicable", subtype.disabled);
  }

  function refresh(form) {
    var rows = form.querySelectorAll(".rule-row");
    var live = 0;
    rows.forEach(function (row) {
      narrow(row);
      var remove = row.querySelector("[data-rule-remove]");
      row.classList.toggle("removing", !!(remove && remove.checked));
      if (isLive(row)) live++;
    });

    // The two ways of saying what an empty table means: a slot picks between them, a room is simply
    // told what saving one does. Both are `hidden` in the markup when the table has rules.
    var empty = live === 0;
    var meaning = form.querySelector("[data-empty-meaning]");
    if (meaning) {
      meaning.hidden = !empty;
      // **Disabled as well as hidden, and this is the half that matters.** The radios are
      // `required`, and a required control inside a hidden fieldset blocks submission with a
      // validation bubble the browser cannot point at anything — so the form would simply refuse to
      // save, silently, whenever the table had rules.
      meaning.querySelectorAll("input").forEach(function (input) {
        input.disabled = !empty;
      });
    }
    var notice = form.querySelector("[data-empty-notice]");
    if (notice) notice.hidden = !empty;
  }

  function markDirty(form) {
    var notice = form.querySelector("[data-dirty-notice]");
    if (notice) notice.hidden = false;
  }

  forms.forEach(function (form) {
    var template = blankRow(form);
    var body = form.querySelector("[data-rules]");
    var add = form.querySelector("[data-add-rule]");

    if (add && template && body) {
      add.hidden = false;
      add.addEventListener("click", function () {
        var row = template.cloneNode(true);
        var index = highestIndex(form) + 1;
        row.querySelectorAll("[name]").forEach(function (field) {
          field.name = field.name.replace(/^rules\[\d+\]/, "rules[" + index + "]");
        });
        body.appendChild(row);
        refresh(form);
        markDirty(form);
        var kind = row.querySelector("[data-rule-kind]");
        if (kind) kind.focus();
      });
    }

    // One delegated listener rather than one per control, because rows arrive after this runs.
    form.addEventListener("change", function (event) {
      if (!event.target.closest(".rule-row")) return;
      refresh(form);
      markDirty(form);
    });
    // `input` as well as `change`, so typing a tag counts as an edit rather than only leaving the
    // field does. It cannot change the row count, so it only marks.
    form.addEventListener("input", function (event) {
      if (!event.target.closest(".rule-row")) return;
      markDirty(form);
    });

    refresh(form);
  });
})();
