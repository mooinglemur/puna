// The room page's lifecycle panel: submit without a reload, then watch.
//
// A cold start is an image pull plus a save restored from a network filesystem, and a restart
// crosses two reconcile passes, so both are tens of seconds of a visible state. This turns that
// into a page that says what is happening rather than one somebody refreshes by hand.
//
// ## It renders nothing
//
// Every panel this swaps in was rendered by `rooms/panel.html`, fetched from `/room/<id>/panel`.
// Building the markup here instead would mean two sets of branches deciding what a room's state
// looks like and who is offered a control, and the one that drifts is the one nobody reviews, so
// the page would go on working while telling somebody the wrong thing about their room.
//
// ## Two endpoints, on purpose
//
// `/status` is a single row as JSON and is what the timer hits; `/panel` is markup and is fetched
// only when `/status` says something changed. A settled page costs a few hundred bytes a second,
// and the expensive call happens once per transition rather than once per tick.
//
// Polling rather than SSE, deliberately: an EventStream holds a connection per waiting player
// through the gateway for the whole cold start, and Rocket holds a task per request to serve it.
//
// Without this file every control is still a form POST and a redirect, and the transient states
// still advance on the `<noscript>` meta refresh. Slower, never wrong.
// Claiming a slot from the roster, in one click.
//
// **Its own IIFE, ahead of the panel's, deliberately.** The lifecycle panel below returns early on
// a page without one, and hanging an unrelated roster feature off that guard is exactly how the
// clipboard controls ended up dead on `/admin/users` at M21: a feature working everywhere except
// the one page whose markup the guard was written for.
//
// The anchor's `href` is a real page: unscripted, the click lands on `/claim/<token>`, which
// describes the slot and asks for a confirmation. That page exists for its own reasons (it is what
// a chat client unfurls when the link is sent to somebody) so this is a shortcut over it rather
// than a substitute for it.
//
// **The POST is what claims, and the GET never does.** It used to: `GET /claim/<token>` redeemed a
// single-use token, so a staff member who clicked this instead of copying it took the slot for
// themselves, and any prefetch holding their session spent the link before its recipient saw it.
(function () {
  "use strict";

  document.addEventListener("click", function (event) {
    var link = event.target.closest("a[data-claim]");
    if (!link) return;
    // Anything but a plain left click means the reader asked for something else (a new tab, a
    // download, a context menu) and every one of those wants the page rather than a mutation
    // fired from under them.
    if (event.defaultPrevented || event.button !== 0) return;
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;

    event.preventDefault();
    if (link.dataset.claiming) return;
    link.dataset.claiming = "1";
    link.textContent = "claiming…";

    fetch(link.dataset.claim, {
      method: "POST",
      headers: { Accept: "application/json" },
      // The session is a cookie, and a fetch does not send one unless told to.
      credentials: "same-origin",
    })
      .then(function (response) {
        if (!response.ok) throw new Error(response.status);
        return response.json();
      })
      .then(function (result) {
        // The cell holds the link and the copy control; both are spent now, so the whole cell
        // becomes what a reload would have rendered for a claimed slot.
        var cell = link.closest("td") || link.parentNode;
        cell.textContent = result.owner_name || "claimed";
      })
      .catch(function () {
        // **Falls back to the page rather than reporting failure in place.** A claim that did not
        // land is nearly always a link somebody else spent first, and `/claim/<token>` is the thing
        // that says so in words, where an error in a table cell would say only that something
        // went wrong.
        window.location.href = link.href;
      });
  });
})();

(function () {
  "use strict";

  var panel = document.querySelector("[data-room]");
  if (!panel) return;

  var id = panel.dataset.room;

  // 1s while a person is watching closely, 3s once they have stopped, and a hard stop at five
  // minutes: past that, something is wrong and a page quietly polling forever helps nobody.
  var QUICK_MS = 1000,
    SLOW_MS = 3000,
    SLOW_AFTER_MS = 30000,
    GIVE_UP_MS = 300000;

  var timer = null;
  var watchingSince = 0;

  // Asked of the PANEL, which was rendered by the server, rather than decided here from a list of
  // state names. That list already exists in the planner and in `is_working`, and a third copy in a
  // file nobody type-checks is the one that drifts.
  //
  // It is also not the same question as "is `state` transient": a room asked to stop is still
  // observed `running` until the orchestrator reaches it, which can be a whole reconcile interval.
  // Deciding here would have meant this file knowing that too.
  function settled() {
    return panel.dataset.working !== "1";
  }

  function human(ms) {
    var seconds = Math.round(ms / 1000);
    if (seconds < 60) return seconds + "s";
    return Math.floor(seconds / 60) + "m " + (seconds % 60) + "s";
  }

  // Replace the panel with freshly rendered markup. `outerHTML` rather than `innerHTML` so the
  // wrapper's own data attributes come along: they are what the next poll compares against.
  function swap(markup) {
    var next = document.createElement("div");
    next.innerHTML = markup;
    var replacement = next.firstElementChild;
    if (!replacement || !replacement.dataset.room) return false;
    panel.replaceWith(replacement);
    panel = replacement;
    return true;
  }

  function refresh() {
    return fetch("/room/" + encodeURIComponent(id) + "/panel", {
      headers: { Accept: "text/html" },
    })
      .then(function (response) {
        if (!response.ok) throw new Error(response.status);
        return response.text();
      })
      .then(swap);
  }

  // A spinner with no claim attached. Shown between a click and the first answer, when the only
  // honest thing to say is that the request is in flight: naming a state here would mean
  // guessing at one the server has not confirmed, and guessing wrong in the direction of
  // reassurance is how a page tells somebody their room is starting when it was refused.
  function working(label) {
    var notice = document.createElement("div");
    notice.className = "notice working";
    var line = document.createElement("p");
    var swirl = document.createElement("span");
    swirl.className = "swirl";
    swirl.setAttribute("aria-hidden", "true");
    line.appendChild(swirl);
    line.appendChild(document.createTextNode(" " + label));
    notice.appendChild(line);
    panel.replaceChildren(notice);
  }

  function fail(message) {
    var error = document.createElement("p");
    error.className = "error";
    error.textContent = message;
    panel.replaceChildren(error);
  }

  function giveUp() {
    var message = panel.querySelector("[data-room-message]");
    if (message) message.textContent = "This is taking longer than expected.";
    var retry = document.createElement("a");
    retry.href = location.pathname;
    retry.textContent = "Check again";
    panel.appendChild(document.createTextNode(" "));
    panel.appendChild(retry);
  }

  function poll() {
    fetch("/room/" + encodeURIComponent(id) + "/status", {
      headers: { Accept: "application/json" },
    })
      .then(function (response) {
        if (!response.ok) throw new Error(response.status);
        return response.json();
      })
      .then(function (status) {
        var moved =
          status.state !== panel.dataset.state ||
          status.desired_state !== panel.dataset.desired;

        if (moved) {
          return refresh().then(function () {
            // Keep watching only while there is something to watch. A room that reached `running`
            // or came to rest has nothing further to say, and the next change to it will be
            // somebody else's action on a page that is no longer this one.
            if (!settled()) schedule();
          });
        }

        // Unchanged: keep the two live numbers current without refetching markup for them.
        var message = panel.querySelector("[data-room-message]");
        var elapsed = panel.querySelector("[data-room-elapsed]");
        if (message && status.message) message.textContent = status.message;
        if (elapsed && typeof status.since_ms === "number") {
          elapsed.textContent = human(status.since_ms);
        }
        schedule();
      })
      .catch(function () {
        // A failed poll is not a failed room: a redeploy of the web tier looks exactly like this.
        // Keep going on the slow cadence rather than declaring anything.
        schedule();
      });
  }

  function schedule() {
    var waited = Date.now() - watchingSince;
    if (waited > GIVE_UP_MS) {
      giveUp();
      return;
    }
    clearTimeout(timer);
    timer = setTimeout(poll, waited > SLOW_AFTER_MS ? SLOW_MS : QUICK_MS);
  }

  // Start watching, or restart the clock if a click happened while already watching.
  function watch() {
    watchingSince = Date.now();
    schedule();
  }

  // Every lifecycle control, wherever it sits on the page: Start and Reopen live inside the panel
  // and are replaced on every swap, Stop and Close live in the organizer section and are not. One
  // delegated listener covers both and survives the panel being replaced underneath it.
  document.addEventListener("submit", function (event) {
    var form = event.target.closest("form[data-lifecycle]");
    if (!form) return;
    event.preventDefault();

    // Keyed on the ACTION rather than derived from the button's words. Deriving it was tried and
    // was wrong in English: "Start" -> "Starting" and "Close" -> "Closing" both fall out of a rule
    // that also produces "Stoping". The action is a fixed vocabulary of three, and the server's own
    // sentence replaces whatever this says on the first poll anyway.
    var verb = form.action.replace(/.*\//, "");
    var labels = {
      start: "Starting the room",
      stop: "Stopping the room",
      close: "Closing the room",
    };
    working(labels[verb] || "Working");

    fetch(form.action, {
      method: "POST",
      headers: { Accept: "text/html" },
      // The forms carry no fields; the URL is the whole request. Sent as same-origin so the
      // session cookie goes with it, which is what authorizes stop and close.
      credentials: "same-origin",
      // --- DO NOT FOLLOW THE REDIRECT, for two reasons ------------------------------------------
      // These routes answer 303 to the room page, which is right for a form post and pure waste
      // here: the panel is refetched below regardless, so following it renders the whole page
      // (slots, generation, siblings) to throw it away, on every click.
      //
      // The second reason is the one that matters. That followed request is a GET of the room page,
      // and `GET /room/<id>` starts an idle room on navigation (D8). The `Navigation` guard sorts a
      // real navigation from a background fetch by `Sec-Fetch-Mode`, but it deliberately falls
      // back to the `Accept` header alone when that header is ABSENT, so a browser that does not
      // send it would have every Stop immediately restart the room it just stopped. No browser new
      // enough to run this file omits the header, which makes it a hazard that would never show up
      // in testing and would be very hard to believe when reported.
      //
      // `manual` makes a 3xx an opaque response: unreadable, and never followed. Anything that is
      // not a redirect (a 403 from a closed room) still arrives normally.
      redirect: "manual",
    })
      .then(function (response) {
        // A 303 the browser did not follow. `status` is 0 and `ok` is false on an opaque response,
        // so this has to be checked before either.
        if (response.type === "opaqueredirect") {
          return refresh().then(watch);
        }
        if (response.status === 403) {
          fail("You are not allowed to do that. Reload the page to see its current state.");
          return;
        }
        if (!response.ok) throw new Error(response.status);
        return refresh().then(watch);
      })
      .catch(function () {
        // Fall back to what would have happened without this script rather than leaving a spinner
        // spinning: the POST may well have landed, and the server-rendered page is the truth.
        form.submit();
      });
  });

  // --- ROTATING A SLOT PASSWORD -----------------------------------------------------------------
  // The value on screen is stale the instant this succeeds, so it is struck through. Deliberately
  // NOT replaced with the new one: this page would have to be told the new credential over a second
  // channel to display it, and a reload already has it. Struck-through says "this is no longer the
  // password" without pretending to know what is.
  //
  // The form still POSTs normally without this: the redirect reloads the page and shows the new
  // value, which is the same destination by a slower road.
  document.addEventListener("submit", function (event) {
    var form = event.target.closest("form[data-rotates]");
    if (!form) return;
    event.preventDefault();

    var cell = form.closest("td");
    fetch(form.action, {
      method: "POST",
      headers: { Accept: "text/html" },
      credentials: "same-origin",
      // Same reasoning as the lifecycle forms: the 303 goes to the room page, and following it
      // renders the whole thing to throw it away.
      redirect: "manual",
    })
      .then(function (response) {
        if (response.type !== "opaqueredirect" && !response.ok) throw new Error(response.status);
        var shown = cell && cell.querySelector("code");
        if (shown) shown.classList.add("stale");
        var copy = cell && cell.querySelector(".copy");
        // The copy control goes with it. Copying a password that is already superseded is the one
        // outcome here worse than no button at all.
        if (copy) copy.remove();
      })
      // Fall back to the ordinary POST rather than leaving the row looking untouched: the request
      // may well have landed, and the server-rendered page is the truth.
      .catch(function () {
        form.submit();
      });
  });

  // --- RENAMING THE ROOM ------------------------------------------------------------------------
  // The swap itself is CSS: `.titlebar:has(.rename[open])` hides the heading, so the field lands
  // where the title was with nothing running. What is added here is the part a stylesheet cannot
  // do: putting the cursor in the field, and getting out without a page load.
  //
  // Every piece degrades: unscripted, the pencil still opens the form, Enter still submits it
  // because it is a form, and the X is a link back to the room, which arrives with the form closed.
  var rename = document.querySelector("details.rename");
  if (rename) {
    var field = rename.querySelector("input[name=\"name\"]");
    // The name as the SERVER rendered it, so cancelling restores what is on the row rather than
    // whatever the field happened to hold when it was last closed.
    var original = field ? field.value : "";

    function closeRename() {
      if (field) field.value = original;
      rename.open = false;
      var summary = rename.querySelector("summary");
      // Focus goes back to the control that opened it. Without this it lands on <body>, and a
      // keyboard user who cancels has to tab in from the top of the page again.
      if (summary) summary.focus();
    }

    rename.addEventListener("toggle", function () {
      if (!rename.open || !field) return;
      field.focus();
      // Selected, not just focused: the field opens holding the current name, and the common edit
      // is a new name rather than a tweak to this one.
      field.select();
    });

    rename.addEventListener("click", function (event) {
      if (!event.target.closest(".cancel")) return;
      // Without this the link navigates to the room page, which is the unscripted cancel and works:
      // it is just a round trip to arrive back where we already are.
      event.preventDefault();
      closeRename();
    });

    rename.addEventListener("keydown", function (event) {
      if (event.key !== "Escape") return;
      // `<details>` does not close on Escape by itself; only `<dialog>` does. A field somebody is
      // typing in should, because that is what Escape means everywhere else.
      event.preventDefault();
      closeRename();
    });
  }

  // A page painted mid-transition watches from the moment it loads; one painted at rest waits for
  // somebody to do something.
  if (!settled()) watch();
})();
