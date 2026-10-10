// Room option forms: show the explanation for the option actually selected, and say when something
// has been changed but not saved.
//
// Used by the creation panel on a generation's page and by a room's own options page. Only the
// MECHANISM is shared: every word either page shows is server-rendered, because the two are
// deliberately worded differently: creating a room describes what it will do, and changing one
// describes what it will do to a room people may be connected to right now.
//
// Every hint is rendered, one per option, and this reveals one at a time. That order matters:
// unscripted the page shows all of them, which is verbose and completely correct, where building
// the text here would leave somebody with scripting off looking at radio buttons with no
// explanation of what any of them do. The stylesheet hides the extras only once this file has said
// it is running.
(function () {
  "use strict";

  // Any form carrying explained option groups. Two pages, and a third would need nothing here.
  var forms = [].slice.call(
    document.querySelectorAll("#create-room, [data-options-form]")
  );
  if (!forms.length) return;

  document.documentElement.classList.add("js-options-form");

  forms.forEach(function (form) {
    var groups = form.querySelectorAll("[data-hints]");
    var dependents = form.querySelectorAll("[data-only-for]");
    var flag = form.querySelector("[data-unsaved]");

    // Which option of a group is selected right now, or `null` for a group with nothing checked.
    function chosenValue(name) {
      var chosen = form.querySelector('input[name="' + name + '"]:checked');
      return chosen ? chosen.value : null;
    }

    function showHints() {
      [].forEach.call(groups, function (group) {
        var chosen = chosenValue(group.dataset.hints);
        [].forEach.call(group.querySelectorAll(".hint[data-for]"), function (hint) {
          // `hidden` rather than a class: it is what the attribute means, and a hint hidden this
          // way is out of the accessibility tree too: a screen reader should not read three
          // explanations of an option nobody has chosen.
          hint.hidden = chosen === null || hint.dataset.for !== chosen;
        });
      });
    }

    // **A whole control that belongs to one option of a group, rather than a hint explaining one.**
    //
    // Separate from the hints above because the two differ in what being hidden MEANS. A hint is
    // prose and hiding it costs a reader nothing; this hides a form control, and the control goes
    // on being submitted either way.
    //
    // That is deliberate and is what keeps the page honest with scripting off, where every option's
    // hint shows at once and so does this: the server reads a dependent control only when the
    // option it depends on was the one submitted, so a box ticked under the wrong radio means
    // nothing rather than applying invisibly. Hiding it is therefore a tidiness, and tidiness is
    // exactly the kind of thing that may depend on a script. Were this ever used for a control
    // whose value the server honors unconditionally, hiding it here would be a trap: the reader
    // would not see the setting they are about to save.
    //
    // `data-only-in` names the group and `data-only-for` the value, both at the point of use,
    // because a dependent control is not necessarily inside the group it depends on: this one is
    // its own labelled row in the grid, a sibling of the radios it follows.
    function showDependents() {
      [].forEach.call(dependents, function (el) {
        var chosen = chosenValue(el.dataset.onlyIn);
        el.hidden = chosen === null || el.dataset.onlyFor !== chosen;
      });
    }

    // **Dirt is measured against the browser's own record of what the server sent.**
    //
    // `defaultChecked` and `defaultValue` are the markup's values, not the current ones, so this
    // needs nothing stashed at load and, the part that makes it worth having, it goes back to
    // clean by itself the moment somebody sets a control back where it was. A snapshot taken in
    // JavaScript would do the same until the first time it drifted from the DOM, and then would
    // quietly report a form as unsaved forever.
    function changed() {
      return [].some.call(form.elements, function (el) {
        if (!el.name || el.disabled) return false;
        if (el.type === "checkbox" || el.type === "radio") {
          return el.checked !== el.defaultChecked;
        }
        if (el.tagName === "SELECT" || el.type === "hidden") return false;
        return el.value !== el.defaultValue;
      });
    }

    function refresh() {
      showHints();
      showDependents();
      if (flag) flag.hidden = !changed();
    }

    form.addEventListener("change", refresh);
    // `input` as well, so typing in the room-name field is reflected as it happens rather than when
    // focus leaves it, by which time somebody has usually already looked for the warning.
    form.addEventListener("input", refresh);
    // A reset restores the markup's defaults without firing `change` for any of them, so both the
    // hints and the flag would go on describing the state from a moment ago. The event fires
    // *before* the controls are restored, hence the deferral.
    form.addEventListener("reset", function () {
      window.setTimeout(refresh, 0);
    });

    refresh();
  });
})();
