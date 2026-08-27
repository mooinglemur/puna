// The room-creation form: show the explanation for the option that is actually selected.
//
// Every hint is server-rendered, one per option, and this reveals one at a time. That order matters:
// unscripted, the page shows all of them, which is verbose and completely correct — where building
// the text here would leave somebody with scripting off looking at a set of radio buttons with no
// explanation of what any of them do. The stylesheet hides the extras only once this file has said
// it is running, the same `js-` root-class trick the clipboard controls use.
//
// A `<button type="reset">` restores the markup's own `checked` and `value` attributes with no help
// from here — the defaults live in the template, in one place, and cannot drift from what the form
// first rendered. All this adds is re-running the hint pass afterwards, because a reset fires no
// `change` events.
(function () {
  "use strict";

  var form = document.getElementById("create-room");
  if (!form) return;

  var groups = form.querySelectorAll("[data-hints]");
  if (!groups.length) return;

  document.documentElement.classList.add("js-create-room");

  function refresh() {
    groups.forEach(function (group) {
      var name = group.dataset.hints;
      var chosen = form.querySelector('input[name="' + name + '"]:checked');
      group.querySelectorAll(".hint[data-for]").forEach(function (hint) {
        // `hidden` rather than a class: it is what the attribute means, and a hint hidden this way
        // is out of the accessibility tree too — a screen reader should not read three
        // explanations of an option somebody has not chosen.
        hint.hidden = !chosen || hint.dataset.for !== chosen.value;
      });
    });
  }

  form.addEventListener("change", refresh);
  // A reset restores the markup's defaults without firing `change` for any of them, so the visible
  // hints would go on describing whatever was selected a moment ago. The event fires *before* the
  // controls are restored, hence the deferral.
  form.addEventListener("reset", function () {
    window.setTimeout(refresh, 0);
  });

  refresh();
})();
