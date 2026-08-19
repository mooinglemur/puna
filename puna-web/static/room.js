// The room page's poller.
//
// A cold start is an image pull plus a save restored from a network filesystem, so it is seconds to
// minutes of a visible state. This turns that into a page that says what is happening rather than
// one somebody refreshes by hand.
//
// Polling rather than SSE, deliberately: an EventStream holds a connection per waiting player
// through the gateway for the whole cold start, and Rocket holds a task per request to serve it.
// A one-second GET of a single row costs neither.
(function () {
  "use strict";

  var root = document.querySelector("[data-room]");
  if (!root) return;

  var id = root.dataset.room;
  var rendered = root.dataset.state;
  var messageEl = root.querySelector("[data-room-message]");
  var elapsedEl = root.querySelector("[data-room-elapsed]");

  // 1s while a person is watching closely, 3s once they have stopped, and a hard stop at five
  // minutes: past that, something is wrong and a page quietly polling forever helps nobody.
  var QUICK_MS = 1000,
    SLOW_MS = 3000,
    SLOW_AFTER_MS = 30000,
    GIVE_UP_MS = 300000;
  var startedAt = Date.now();

  function human(ms) {
    var seconds = Math.round(ms / 1000);
    if (seconds < 60) return seconds + "s";
    return Math.floor(seconds / 60) + "m " + (seconds % 60) + "s";
  }

  function giveUp() {
    if (messageEl) {
      messageEl.textContent = "This is taking longer than expected.";
    }
    var retry = document.createElement("a");
    retry.href = location.pathname;
    retry.textContent = "Check again";
    root.appendChild(document.createTextNode(" "));
    root.appendChild(retry);
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
        // Reload on a CHANGE rather than on any particular state: the page is server-rendered, so
        // whatever it becomes is rendered correctly, and comparing against what this page was built
        // from means no state needs listing here twice.
        if (status.state !== rendered) {
          location.reload();
          return;
        }
        if (messageEl && status.message) messageEl.textContent = status.message;
        if (elapsedEl && typeof status.since_ms === "number") {
          elapsedEl.textContent = human(status.since_ms);
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
    var waited = Date.now() - startedAt;
    if (waited > GIVE_UP_MS) {
      giveUp();
      return;
    }
    setTimeout(poll, waited > SLOW_AFTER_MS ? SLOW_MS : QUICK_MS);
  }

  schedule();
})();
