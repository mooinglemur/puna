// Checkboxes that remember themselves, across page loads and across pages.
//
// ## What this is for, and what it is not
//
// A "volatile toggle" here is a view preference: something that changes how a page *shows* what it
// already has, and that a reader expects to stay put — "only my slots", "only the most recent of
// each item". It is not configuration. Nothing here reaches the server, nothing here is authorized,
// and losing the lot costs somebody one click.
//
// That last property is what makes the storage choice easy, and it is worth stating because the
// obvious alternative looks equivalent and is not.
//
// ## localStorage, not a cookie
//
// A cookie is sent on **every request to the origin**, and this site's highest-volume surface by a
// wide margin is the tracker — whose whole design is about not doing work per poll, and which polls
// JSON every few seconds per open tab. Putting view preferences in a cookie would attach them to
// every one of those, forever, to be ignored by the server every time.
//
// `localStorage` costs nothing per request, is already how `theme.js` remembers the theme, and is
// the right shape for state only the browser acts on. The one thing a cookie would buy is letting
// the SERVER render a control pre-checked; that matters where a wrong first paint is visible, and
// here it is not — the tracker's tables are empty until their fetch returns, so nothing flickers.
//
// ## The convention
//
// `<input type="checkbox" data-toggle="some.stable.key">`. This file restores every one of them at
// load and writes back on change. Anything that needs to *react* listens for `change` on the input
// as it normally would — this does not dispatch synthetic events, so a listener added afterwards
// sees a box that is already in the right state and can simply read `.checked`.
//
// Keys are namespaced by hand (`tracker.items.latest`) because they share one store across every
// page, and two pages inventing `only-mine` independently would share a preference nobody asked to
// share.
(function () {
  "use strict";

  var KEY = "puna.toggles";

  // **Wrapped, because `localStorage` THROWS rather than returning null** in Safari's private mode
  // and wherever storage is blocked for an origin. A toggle that cannot be remembered still has to
  // work for the life of the page, which is what these fallbacks buy.
  function load() {
    try {
      return JSON.parse(window.localStorage.getItem(KEY) || "{}") || {};
    } catch (e) {
      return {};
    }
  }

  function save(all) {
    try {
      window.localStorage.setItem(KEY, JSON.stringify(all));
    } catch (e) {
      // Unstorable. The page keeps working; the choice simply does not outlive it.
    }
  }

  function get(key) {
    return load()[key] === true;
  }

  function set(key, on) {
    var all = load();
    // Deleted rather than stored as `false`, so the store holds only what somebody turned ON and
    // does not grow a permanent entry for every box anybody has ever unticked.
    if (on) all[key] = true;
    else delete all[key];
    save(all);
  }

  // Restore and wire every `[data-toggle]` under `root`.
  //
  // Exposed for the same reason `PunaTables.scan` is: not every control is in the document at load
  // — `/admin/rooms` fetches a table when its section is opened. `data-toggle-bound` is the
  // idempotence, since binding twice would write the store twice per click.
  function bind(root) {
    (root || document).querySelectorAll("[data-toggle]").forEach(function (input) {
      if (input.dataset.toggleBound) return;
      input.dataset.toggleBound = "1";
      var key = input.dataset.toggle;
      input.checked = get(key);
      input.addEventListener("change", function () {
        set(key, input.checked);
      });
    });
  }

  window.PunaToggles = { get: get, set: set, bind: bind };
  bind(document);
})();
