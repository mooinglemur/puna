// The room page's lifecycle panel: submit without a reload, then watch.
//
// A cold start is an image pull plus a save restored from a network filesystem, and a restart
// crosses two reconcile passes -- so both are tens of seconds of a visible state. This turns that
// into a page that says what is happening rather than one somebody refreshes by hand.
//
// ## It renders nothing
//
// Every panel this swaps in was rendered by `rooms/panel.html`, fetched from `/room/<id>/panel`.
// Building the markup here instead would mean two sets of branches deciding what a room's state
// looks like and who is offered a control -- and the one that drifts is the one nobody reviews, so
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
  // wrapper's own data attributes come along -- they are what the next poll compares against.
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
  // honest thing to say is that the request is in flight -- naming a state here would mean
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
        // A failed poll is not a failed room -- a redeploy of the web tier looks exactly like this.
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
      // here: the panel is refetched below regardless, so following it renders the whole page --
      // slots, generation, siblings -- to throw it away, on every click.
      //
      // The second reason is the one that matters. That followed request is a GET of the room page,
      // and `GET /room/<id>` starts an idle room on navigation (D8). The `Navigation` guard sorts a
      // real navigation from a background fetch by `Sec-Fetch-Mode` -- but it deliberately falls
      // back to the `Accept` header alone when that header is ABSENT, so a browser that does not
      // send it would have every Stop immediately restart the room it just stopped. No browser new
      // enough to run this file omits the header, which makes it a hazard that would never show up
      // in testing and would be very hard to believe when reported.
      //
      // `manual` makes a 3xx an opaque response: unreadable, and never followed. Anything that is
      // not a redirect -- a 403 from a closed room -- still arrives normally.
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

  // --- COPY TO CLIPBOARD ------------------------------------------------------------------------
  // Feature-detected before the controls are revealed, not after they are clicked.
  // `navigator.clipboard` requires a secure context, so on plain HTTP it is simply absent -- and a
  // copy button that silently does nothing is worse than no button, because the address looks
  // copied and the paste is whatever was there before.
  //
  // The class goes on <html> rather than on each button: the panel is replaced wholesale on every
  // state change, so anything reapplied per swap is something that eventually gets missed.
  if (navigator.clipboard && window.isSecureContext) {
    document.documentElement.classList.add("js-copy");
  }

  var confirmation = null;
  var confirmationTimers = [];

  function dismissConfirmation() {
    confirmationTimers.forEach(clearTimeout);
    confirmationTimers = [];
    if (confirmation) confirmation.remove();
    confirmation = null;
    window.removeEventListener("scroll", dismissConfirmation);
  }

  // A floating tooltip, appended to <body> and positioned from the button's rect.
  //
  // NOT inserted beside the button, which is where this started: an element in the flow takes
  // layout space, so the cell and the whole table jumped wider for a second and back. And it could
  // not simply be made `absolute` either -- the global `table` rule sets `overflow-x: auto`, so the
  // table is a scroll container and would clip it. `fixed` off <body> avoids both, and avoids
  // having to know which ancestor is a containing block.
  function confirmCopy(button, message, failed) {
    dismissConfirmation();

    confirmation = document.createElement("span");
    confirmation.className = failed ? "copied error" : "copied";
    // Announced, because the whole feedback is visual otherwise and the thing being confirmed is
    // that something invisible happened.
    confirmation.setAttribute("role", "status");
    confirmation.textContent = message;
    document.body.appendChild(confirmation);

    var rect = button.getBoundingClientRect();
    confirmation.style.left = rect.left + rect.width / 2 + "px";
    // Above by default. Flipped below when the button sits too near the top of the viewport for the
    // tooltip to fit -- a confirmation rendered off-screen is the same as none, and this is the one
    // case where somebody would go on to paste something they never copied.
    if (rect.top < 44) {
      confirmation.classList.add("below");
      confirmation.style.top = rect.bottom + 8 + "px";
    } else {
      confirmation.style.top = rect.top - 8 + "px";
    }

    // Positioned once rather than tracked. Over a second and a bit that is right for everything
    // except scrolling, where the anchor moves and the tooltip would not -- so scrolling takes it
    // away instead of leaving it floating over nothing.
    window.addEventListener("scroll", dismissConfirmation, { passive: true });

    // Long enough to read, short enough not to be something you wait out.
    var target = confirmation;
    confirmationTimers.push(
      setTimeout(function () {
        target.classList.add("fading");
      }, 900),
      setTimeout(function () {
        if (confirmation === target) dismissConfirmation();
        else target.remove();
      }, 1200),
    );
  }

  document.addEventListener("click", function (event) {
    var button = event.target.closest(".copy");
    if (!button) return;
    event.preventDefault();

    var text = button.dataset.copy;
    navigator.clipboard.writeText(text).then(
      function () {
        confirmCopy(button, "Copied to clipboard");
      },
      function () {
        // The API exists and refused -- a permissions policy, or a click the browser did not count
        // as a user gesture. Say so rather than claiming success: the address is still right there
        // to select by hand, and this is the case where somebody would otherwise paste the wrong
        // thing into a game client and wonder why it will not connect.
        confirmCopy(button, "Could not copy", true);
      },
    );
  });

  // --- ROTATING A SLOT PASSWORD -----------------------------------------------------------------
  // The value on screen is stale the instant this succeeds, so it is struck through. Deliberately
  // NOT replaced with the new one: this page would have to be told the new credential over a second
  // channel to display it, and a reload already has it. Struck-through says "this is no longer the
  // password" without pretending to know what is.
  //
  // The form still POSTs normally without this -- the redirect reloads the page and shows the new
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

  // A page painted mid-transition watches from the moment it loads; one painted at rest waits for
  // somebody to do something.
  if (!settled()) watch();
})();
