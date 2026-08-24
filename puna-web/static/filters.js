// The traffic-filter rule table: rows added and struck out in place, saved in one go.
//
// ## What the server already does, and what this adds
//
// Every state this file touches is also rendered by `rooms/_rule_table.html`, and the page works
// with this script absent: a blank row is always present so one rule can be added per save, the
// remove control is a real submit button that posts `rules[N].remove=true` for the row it sits in,
// and the tag/subtype cells arrive disabled or enabled to match the kind each row already has.
//
// So this is not the editor — it is the difference between one round trip per rule and none:
//
// * **removal in place.** The button's default action is suppressed and the row simply goes, so the
//   table always reads as what the next save will mean rather than as a list with marks against it.
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

  // A row counts as a rule when it names something to drop. This is the same question the server
  // asks of a submission, and the two have to agree or the empty-table choice appears at a
  // different moment from the one it applies to.
  function isLive(row) {
    var kind = row.querySelector("[data-rule-kind]");
    return !!(kind && kind.value);
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

  // **Offer only the directions this kind can actually travel.**
  //
  // Most kinds are one-way — a `Set` is something a slot sends, a `PrintJSON` something it receives
  // — and pahoa refuses the impossible pairing outright, because a rule that cannot match looks
  // exactly like a filter that is not working. Which is what happened: a chat filter written
  // `from_slot` `PrintJSON` was accepted here, stored, pushed, and answered `400`, while the room
  // page went on showing it as the room's filter.
  //
  // The route refuses it too. This is so it cannot be typed, which is the better place to stop it.
  function steer(row) {
    var kind = row.querySelector("[data-rule-kind]");
    var direction = row.querySelector("[data-rule-direction]");
    if (!kind || !direction) return;

    var chosen = kind.options[kind.selectedIndex];
    var travels = chosen ? (chosen.dataset.travels || "").split(" ") : [];
    // No kind chosen yet: both stay on offer, because the row is not a rule yet.
    var open = !kind.value || travels.length === 0;

    var allowed = null;
    Array.prototype.forEach.call(direction.options, function (option) {
      var ok = open || travels.indexOf(option.value) !== -1;
      // Hidden AND disabled: `hidden` on an `<option>` is not honored everywhere, and a disabled
      // option cannot be selected wherever it is still drawn.
      option.hidden = !ok;
      option.disabled = !ok;
      if (ok && allowed === null) allowed = option.value;
    });

    // If the direction on the row is now impossible, move it to the one that works. Silent because
    // there is nothing to choose between: a one-way kind has exactly one answer, and leaving the
    // old value would submit the rule this whole function exists to prevent.
    if (allowed !== null && travels.length && travels.indexOf(direction.value) === -1) {
      direction.value = allowed;
    }
    // **Deliberately NOT greyed the way an inapplicable tag cell is.** A one-way kind leaves nothing
    // to choose, but the direction still applies and is still submitted — dimming it would say the
    // opposite, which is what `.inapplicable` means two columns over. A picker holding one option
    // says "no choice here" on its own.
  }

  function refresh(form) {
    var live = 0;
    form.querySelectorAll(".rule-row").forEach(function (row) {
      narrow(row);
      steer(row);
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

    // **The remove button is a submit, and this is what stops it submitting.** Without a script it
    // posts `rules[N].remove=true` and the server drops that row; with one, the row simply goes and
    // the change travels with the next save — which is what "the table is what I want in effect"
    // means. Delegated, because rows arrive after this runs.
    form.addEventListener("click", function (event) {
      var button = event.target.closest("[data-rule-remove]");
      if (!button) return;
      event.preventDefault();
      var row = button.closest(".rule-row");
      if (!row) return;
      // Focus would otherwise land on `<body>` and lose the keyboard's place in the table.
      var next = row.nextElementSibling || row.previousElementSibling;
      row.remove();
      if (next) {
        var landing = next.querySelector("[data-rule-remove]");
        if (landing) landing.focus();
      }
      refresh(form);
      markDirty(form);
    });

    // **Open the suggestions when an empty tag or subtype box is entered.**
    //
    // A `datalist` is otherwise invisible: nothing on the control says it has suggestions, so the
    // closed set of `PrintJSON` subtypes goes unread and people type from memory. Popping it open
    // on an EMPTY field only — once there is text, the browser filters as you type and forcing the
    // list back open would fight the typing.
    //
    // `showPicker()` requires transient user activation, which a click grants and a Tab does not,
    // and it is not everywhere yet — so both are caught and the field degrades to what it does
    // today: suggestions on the down arrow.
    function suggest(field) {
      if (!field || field.disabled || field.value !== "") return;
      if (typeof field.showPicker !== "function") return;
      try {
        field.showPicker();
      } catch (e) {
        // No user activation, or no support for a datalist picker here. Nothing is lost.
      }
    }

    form.addEventListener("focusin", function (event) {
      suggest(event.target.closest("[data-rule-tag], [data-rule-subtype]"));
    });
    // Also on click, because focus arriving by Tab carries no activation and a click on an
    // already-focused field would otherwise do nothing.
    form.addEventListener("click", function (event) {
      suggest(event.target.closest("[data-rule-tag], [data-rule-subtype]"));
    });

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
