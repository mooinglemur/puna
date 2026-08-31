// The bulk panel: move slots between two lists, and select them in bulk by a rule.
//
// ## What it does and does not own
//
// The markup is `rooms/bulk.html` and the work is the route. This moves `<option>`s between two
// `<select multiple>`s and highlights them. Nothing here decides what an action means, and nothing
// here talks to the room.
//
// ## Why the right pane is the form field
//
// `<select multiple name="slots">` submits its **selected** options, not its contents. So the
// staged set and the posted set are only the same thing if everything on the right is selected at
// submit time, which is what `form.submit` does below. The alternative, hidden inputs kept in step
// with the list, is two representations of one set, and the one that drifts is the one nobody
// looks at.
//
// Without this file the panes do not move and the `<noscript>` says so. The room page's per-slot
// controls are the unscripted path, and they are unaffected.
(function () {
  "use strict";

  var form = document.getElementById("bulk");
  if (!form) return;

  var available = document.getElementById("available");
  var staged = document.getElementById("staged");
  var selector = document.getElementById("selector");
  var value = document.getElementById("selector-value");
  var options = document.getElementById("selector-options");
  var count = document.getElementById("selection-count");

  // `1 slot`, `3 slots`. The server side has puna_core::text::count for the same job; this is the
  // one page that counts things in the browser, so it carries its own three lines rather than
  // earning a shared file.
  function slots(n) {
    return n + (n === 1 ? " slot" : " slots");
  }

  // Which suggestion list feeds the value box, per selector. `unclaimed` takes no value at all.
  var SOURCES = { game: "games", claimant: "claimants", unclaimed: null };

  function move(from, to) {
    // Collected before moving: `selectedOptions` is live, so removing an option while walking it
    // skips the next one -- the classic way a "move all" quietly moves half.
    var picked = Array.prototype.slice.call(from.selectedOptions);
    picked.forEach(function (option) {
      option.selected = false;
      to.appendChild(option);
    });
    renumber(to);
    report(picked.length + " moved");
  }

  // Both lists stay in slot order, so a staged set of forty is readable rather than a record of the
  // order somebody happened to click.
  function renumber(list) {
    var sorted = Array.prototype.slice.call(list.options).sort(function (a, b) {
      return Number(a.value) - Number(b.value);
    });
    sorted.forEach(function (option) {
      list.appendChild(option);
    });
  }

  // **Replaces the highlight rather than adding to it.** To build "game X and game Y", apply X,
  // stage it, then apply Y -- which is why there is no separate clear control: applying anything
  // clears what came before.
  function matches(option, kind, needle) {
    if (kind === "unclaimed") return option.dataset.claimed !== "true";
    var hay = kind === "game" ? option.dataset.game : option.dataset.owner;
    if (!needle) return false;
    // Substring and case-insensitive, which is the whole reason the value box is a text input with
    // a datalist rather than a picker: with a hundred games, typing three characters beats
    // scrolling. Deliberately looser than pahoa's exact matching, because this selects rows in a
    // browser rather than naming a thing to a room.
    return (hay || "").toLowerCase().indexOf(needle) !== -1;
  }

  function applySelection() {
    var kind = selector.value;
    var needle = value.value.trim().toLowerCase();
    var total = 0;

    [available, staged].forEach(function (list) {
      Array.prototype.forEach.call(list.options, function (option) {
        var hit = matches(option, kind, needle);
        option.selected = hit;
        if (hit) total++;
      });
    });

    // Said out loud, because a selection spread across two lists of hundreds is not something a
    // glance confirms -- and "Apply selection" doing nothing looks identical to it matching zero.
    report(
      total === 0
        ? "Nothing matched, so nothing is selected."
        : slots(total) + " selected across both lists."
    );
  }

  // **The one thing `Apply` cannot say on its own**, because it replaces rather than adds: "select
  // everything except X" is apply-then-invert. Acting on both panes for the same reason `Apply`
  // does: it is a selection operation, so which pane a slot happens to be sitting in is not part
  // of the question.
  function invertSelection() {
    var total = 0;
    [available, staged].forEach(function (list) {
      Array.prototype.forEach.call(list.options, function (option) {
        option.selected = !option.selected;
        if (option.selected) total++;
      });
    });
    report(
      total === 0
        ? "Everything was selected, so nothing is now."
        : slots(total) + " selected across both lists."
    );
  }

  function report(message) {
    if (count) count.textContent = message;
  }

  // The value box follows the selector: a different suggestion list, and disabled entirely for the
  // one rule that takes no value.
  function syncSelector() {
    var source = SOURCES[selector.value];
    value.disabled = !source;
    value.value = value.disabled ? "" : value.value;
    // Copied rather than re-pointed, because `list` wants an id and swapping it between two
    // datalists leaves the browser caching the first one in some engines.
    if (options) {
      options.replaceChildren();
      var from = source ? document.getElementById(source) : null;
      if (from) {
        Array.prototype.forEach.call(from.options, function (option) {
          var copy = document.createElement("option");
          copy.value = option.value;
          options.appendChild(copy);
        });
      }
    }
  }

  document.getElementById("stage").addEventListener("click", function () {
    move(available, staged);
  });
  document.getElementById("unstage").addEventListener("click", function () {
    move(staged, available);
  });
  document.getElementById("apply-selection").addEventListener("click", applySelection);
  document.getElementById("invert-selection").addEventListener("click", invertSelection);
  selector.addEventListener("change", syncSelector);
  // Enter in the value box applies rather than submitting the form, which would run whichever
  // action happened to be the first button.
  value.addEventListener("keydown", function (event) {
    if (event.key === "Enter") {
      event.preventDefault();
      applySelection();
    }
  });

  form.addEventListener("submit", function (event) {
    if (!staged.options.length) {
      event.preventDefault();
      report("Nothing is staged. Move some slots into Target Slots first.");
      return;
    }

    var confirmation = event.submitter && event.submitter.dataset.confirm;
    if (confirmation) {
      // The count belongs in the sentence: a control whose reach is "everything staged" should say
      // how many that is at the moment it is clicked.
      var message =
        confirmation +
        "\n\n" +
        slots(staged.options.length) +
        (staged.options.length === 1 ? " is staged." : " are staged.");
      if (!window.confirm(message)) {
        event.preventDefault();
        return;
      }
    }

    // **Selected, not merely present.** See the note at the top: this is what makes the staged list
    // and the posted list the same set.
    Array.prototype.forEach.call(staged.options, function (option) {
      option.selected = true;
    });
  });

  syncSelector();
})();
