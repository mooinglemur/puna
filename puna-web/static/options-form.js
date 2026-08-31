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
    var flag = form.querySelector("[data-unsaved]");

    function showHints() {
      [].forEach.call(groups, function (group) {
        var name = group.dataset.hints;
        var chosen = form.querySelector('input[name="' + name + '"]:checked');
        [].forEach.call(group.querySelectorAll(".hint[data-for]"), function (hint) {
          // `hidden` rather than a class: it is what the attribute means, and a hint hidden this
          // way is out of the accessibility tree too: a screen reader should not read three
          // explanations of an option nobody has chosen.
          hint.hidden = !chosen || hint.dataset.for !== chosen.value;
        });
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
