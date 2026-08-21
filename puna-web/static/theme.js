// Light, dark, or whatever the system says.
//
// **The entire mechanism is `color-scheme` on <html>.** `puna.css` puts every color through
// `light-dark()` on a custom property, and `light-dark()` resolves against the used value of
// `color-scheme` -- which is inherited, so setting it once on the root decides the whole page.
// There is no second palette here, no `prefers-color-scheme` block, and nothing to keep in step:
// the stylesheet already answered this question and this only chooses which of its two answers is
// in force.
//
//   (absent)            -> color-scheme: light dark   -- follow the system
//   data-theme=light    -> color-scheme: light
//   data-theme=dark     -> color-scheme: dark
//
// **Loaded from <head> WITHOUT `defer`, which is deliberate and is the only reason this file is
// separate from the rest.** A deferred script runs after the document is parsed, so the page would
// paint in the system theme and then snap to the reader's choice -- a white flash on every
// navigation for somebody who chose dark. It has to run before first paint, and the alternative,
// an inline <script>, would need `unsafe-inline` the day a Content-Security-Policy is added.
// Blocking on a tiny same-origin cached file is the cheaper side of that trade.
//
// So it runs in two phases: the theme is applied immediately, at parse time, and the control is
// wired at DOMContentLoaded because it does not exist yet when this first runs.
(function () {
  "use strict";

  var KEY = "puna-theme";
  var CHOICES = ["light", "dark"];

  // "system" is the ABSENCE of a stored value rather than a value of its own. A browser that has
  // never been told and one that was told "follow the system" are the same state, and spelling it
  // two ways would mean two things to check everywhere.
  function stored() {
    try {
      var value = localStorage.getItem(KEY);
      return CHOICES.indexOf(value) === -1 ? null : value;
    } catch (e) {
      // Storage can throw rather than return null -- Safari's private mode, or a browser with
      // cookies blocked for the origin. The theme still works for the life of the page.
      return null;
    }
  }

  function apply(choice) {
    if (choice) document.documentElement.dataset.theme = choice;
    else delete document.documentElement.dataset.theme;
  }

  // Phase one, before anything paints.
  apply(stored());
  // Reveals the control. Without scripting it cannot work, so it is not shown -- the same reason
  // and the same shape as `js-copy` for the clipboard buttons.
  document.documentElement.classList.add("js-theme");

  function wire() {
    var group = document.querySelector("[data-theme-toggle]");
    if (!group) return;
    var buttons = Array.prototype.slice.call(group.querySelectorAll("button[data-set]"));

    // The buttons' *appearance* comes from `[data-theme]` in the stylesheet, so the active one is
    // right from first paint with nothing running. This is the part CSS cannot do: `aria-pressed`
    // is what tells a screen reader which of three toggles is the current one.
    function mark() {
      var current = stored() || "system";
      buttons.forEach(function (button) {
        button.setAttribute("aria-pressed", button.dataset.set === current ? "true" : "false");
      });
    }

    buttons.forEach(function (button) {
      button.addEventListener("click", function () {
        var choice = button.dataset.set === "system" ? null : button.dataset.set;
        apply(choice);
        try {
          if (choice) localStorage.setItem(KEY, choice);
          else localStorage.removeItem(KEY);
        } catch (e) {
          // Unstorable, so the choice lasts until this page is left. Better than refusing it.
        }
        mark();
      });
    });

    mark();

    // Other tabs on this origin, which `storage` fires in and not in the one that wrote. A reader
    // with the room page and its tracker open changed the theme once, not for one of them.
    window.addEventListener("storage", function (event) {
      if (event.key !== KEY) return;
      apply(stored());
      mark();
    });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", wire);
  } else {
    wire();
  }
})();
