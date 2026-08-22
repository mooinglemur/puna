// The room page's moderation column: confirm, wait for the room, then make somebody read the answer.
//
// ## What this replaces, and what happens without it
//
// Every control in that column is a LINK to the console with the command and the slot already
// chosen. This intercepts the click and runs it here instead. With scripting off — or if this file
// fails to load — the link is followed and the console does the same job on its own page, with the
// same fields and the same route. Nothing here is the only way to do anything.
//
// ## It renders no markup
//
// The dialog, every field and the result pane are in `rooms/show.html`; this only chooses which
// fields apply and fills them in. Building the form here would put "what does each command ask
// for?" in a second place, in the file with no type checking — and `build()` in `console.rs` is
// already the authority, since it is what refuses a command with a field missing.
//
// ## Why a <dialog>, and why the result has to be dismissed
//
// `showModal()` makes the rest of the page inert. That is the requirement: a hint that was refused
// and a hint that was granted look identical once the dialog is gone, so the answer stays up until
// somebody closes it. A `popover` would have been less code and light-dismisses on a stray click,
// which is exactly the wrong behavior for "did this work?".
(function () {
  "use strict";

  var dialog = document.getElementById("moderate");
  if (!dialog || typeof dialog.showModal !== "function") return;

  var form = dialog.querySelector("[data-mod-form]");
  var working = dialog.querySelector("[data-mod-working]");
  var workingLabel = dialog.querySelector("[data-mod-working-label]");
  var result = dialog.querySelector("[data-mod-result]");
  var resultTitle = dialog.querySelector("[data-mod-result-title]");
  var resultLines = dialog.querySelector("[data-mod-result-lines]");
  var names = document.getElementById("mod-names");

  // What each command asks for, and what to say about it before it runs.
  //
  // `confirm: false` is the pair that change nothing a misclick cannot undo — a lock is reversed by
  // unlocking, and a kicked client reconnects — so they go straight to the room. Everything else
  // sends items or hints into somebody's game and cannot be taken back, so it asks first.
  var COMMANDS = {
    lock_slot: { confirm: false, working: "Locking" },
    kick: { confirm: false, working: "Kicking" },
    hint: {
      confirm: true,
      fields: ["item", "force"],
      title: "Hint an item",
      explain:
        "Tells this slot where an item is. Unless you force it, this spends the slot's own hint " +
        "points and may grant fewer than asked, or none — the room's answer is the truth.",
      working: "Hinting",
    },
    hint_location: {
      confirm: true,
      fields: ["location", "force"],
      title: "Hint a location",
      explain: "Tells this slot what is in one of their own locations.",
      working: "Hinting",
    },
    send_location: {
      confirm: true,
      fields: ["location"],
      title: "Check a location",
      explain:
        "Checks the location and sends out whatever it holds, as though the player had found it. " +
        "This cannot be undone.",
      working: "Checking",
    },
    send_item: {
      confirm: true,
      fields: ["item"],
      title: "Send an item",
      explain: "Gives this slot the item outright. This cannot be undone.",
      working: "Sending",
    },
    collect: {
      confirm: true,
      title: "Collect",
      explain:
        "Gathers every item belonging to this slot that is still out in the multiworld, as though " +
        "they had finished. This cannot be undone.",
      working: "Collecting",
    },
    release: {
      confirm: true,
      title: "Release",
      explain:
        "Sends this slot's remaining items to the players they belong to. Everyone else's game " +
        "moves forward, and this cannot be undone.",
      working: "Releasing",
    },
  };

  // The lock control is one command pointing two ways, and the wording has to follow, or a page
  // full of "Locking" while unlocking is the sort of thing nobody quite trusts afterwards.
  function describe(spec, link) {
    if (link.dataset.command !== "lock_slot") return spec;
    var locking = link.dataset.locked === "true";
    return {
      confirm: false,
      working: locking ? "Locking" : "Unlocking",
    };
  }

  function show(pane) {
    [form, working, result].forEach(function (section) {
      if (section) section.hidden = section !== pane;
    });
  }

  function fill(link, spec) {
    form.querySelector("[data-mod-kind]").value = link.dataset.command;
    form.querySelector("[data-mod-slot]").value = link.dataset.slot;
    // Only meaningful for the lock command; empty otherwise, which `build()` reads as absent.
    form.querySelector("[data-mod-locked]").value = link.dataset.locked || "";

    var wanted = spec.fields || [];
    Array.prototype.forEach.call(form.querySelectorAll("[data-mod-field]"), function (row) {
      var name = row.dataset.modField;
      var applies = wanted.indexOf(name) !== -1;
      row.hidden = !applies;
      var input = row.querySelector("input");
      if (!input) return;
      // **Cleared every time.** Left alone, an item typed for slot 3 would still be sitting there
      // when the dialog reopens for slot 9 — pre-filled, plausible, and about the wrong player.
      if (input.type === "checkbox") input.checked = false;
      else input.value = "";
      // Removed rather than merely hidden: a hidden field still submits, and a stray `location` on
      // a `send_item` is a field pahoa was not asked for.
      input.disabled = !applies;
    });

    form.querySelector("[data-mod-title]").textContent = spec.title || "Confirm";
    form.querySelector("[data-mod-explain]").textContent = spec.explain || "";
    if (names) names.replaceChildren();
  }

  function lines(into, texts) {
    into.replaceChildren();
    (texts || []).forEach(function (text) {
      var line = document.createElement("p");
      // `textContent`, never `innerHTML`: these are the room's own words and, through item and
      // location names, text out of an uploaded seed.
      line.textContent = text;
      into.appendChild(line);
    });
  }

  function finish(body) {
    resultTitle.textContent = body.heading || (body.ok ? "Done" : "Refused");
    result.className = body.ok ? "ok" : "not-ok";
    lines(resultLines, body.lines);
    show(result);
    var dismiss = dialog.querySelector("[data-mod-dismiss]");
    if (dismiss) dismiss.focus();
  }

  function submit(spec) {
    workingLabel.textContent = (spec.working || "Working") + "…";
    show(working);

    var body = new FormData(form);
    fetch(form.action, {
      method: "POST",
      headers: { Accept: "application/json" },
      credentials: "same-origin",
      body: new URLSearchParams(body),
    })
      .then(function (response) {
        // The route answers JSON for every outcome it authored, including a refusal — so a body
        // that will not parse is a failure further out than the handler, and saying so beats
        // showing an empty result pane.
        return response.json().catch(function () {
          throw new Error(response.status);
        });
      })
      .then(finish)
      .catch(function () {
        finish({
          ok: false,
          heading: "No answer",
          lines: [
            "The request did not complete, so whether the command ran is unknown. Check the " +
              "room's command history before running it again.",
          ],
        });
      });
  }

  // --- suggestions ------------------------------------------------------------------------------
  // pahoa matches item and location names EXACTLY, which is right — a program should not be guessed
  // at — and makes an empty text box hostile. These come from the generation's own datapackage,
  // scoped to the target slot's game, which is the game the command will be read in.
  var pending = null;
  function suggest(input) {
    var kind = input.dataset.modSuggest;
    var slot = form.querySelector("[data-mod-slot]").value;
    var query = input.value.trim();
    // Two characters, because one matches most of a game's table and the list would be a wall.
    if (!names || !slot || query.length < 2) {
      if (names) names.replaceChildren();
      return;
    }

    clearTimeout(pending);
    // Debounced: a keystroke each would be a query each, and the answer for "swo" is almost
    // always the answer for "sw" narrowed.
    pending = setTimeout(function () {
      var url =
        "/room/" +
        encodeURIComponent(location.pathname.split("/")[2]) +
        "/slot/" +
        encodeURIComponent(slot) +
        "/names?kind=" +
        encodeURIComponent(kind) +
        "&q=" +
        encodeURIComponent(query);
      fetch(url, { headers: { Accept: "application/json" }, credentials: "same-origin" })
        .then(function (response) {
          if (!response.ok) throw new Error(response.status);
          return response.json();
        })
        .then(function (body) {
          names.replaceChildren();
          (body.names || []).forEach(function (name) {
            var option = document.createElement("option");
            option.value = name;
            names.appendChild(option);
          });
        })
        // Silence is correct here: suggestions are an aid, and the field still accepts a typed
        // name. An error message about an autocomplete would be noise over a working control.
        .catch(function () {});
    }, 150);
  }

  form.addEventListener("input", function (event) {
    var input = event.target.closest("[data-mod-suggest]");
    if (input) suggest(input);
  });

  // --- opening ----------------------------------------------------------------------------------
  var active = null;

  document.addEventListener("click", function (event) {
    var link = event.target.closest("a[data-command]");
    if (!link) return;
    // Modified clicks are somebody deliberately opening the console in a tab. Left alone.
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.button !== 0) return;
    event.preventDefault();

    var spec = describe(COMMANDS[link.dataset.command], link);
    if (!spec) return;
    active = spec;

    fill(link, spec);
    dialog.showModal();

    if (spec.confirm) {
      show(form);
      var first = form.querySelector("[data-mod-field]:not([hidden]) input");
      if (first) first.focus();
    } else {
      // No confirmation: a lock is undone by unlocking and a kicked client reconnects, so a dialog
      // asking "are you sure" for these would be a click somebody learns to dismiss without
      // reading — which is what makes the confirmations on the others worth anything.
      submit(spec);
    }
  });

  form.addEventListener("submit", function (event) {
    event.preventDefault();
    if (active) submit(active);
  });

  dialog.addEventListener("click", function (event) {
    if (event.target.closest("[data-mod-cancel]")) dialog.close();
    if (event.target.closest("[data-mod-dismiss]")) {
      dialog.close();
      // The row's own state may have moved -- a lock flips the control, a release changes nothing
      // visible but the history does. Reloading is the honest way to show it, and it happens after
      // the operator has read the answer rather than underneath them.
      location.reload();
    }
  });

  // Escape closes a <dialog> natively, which is right while the form is up and wrong once an answer
  // is showing: the whole point is that the result gets read. Cancelled instead, and the dismiss
  // button is the way out.
  dialog.addEventListener("cancel", function (event) {
    if (!result.hidden || !working.hidden) event.preventDefault();
  });
})();
