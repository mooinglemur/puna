// The room's history, live.
//
// Opens a WebSocket to Puna, asks for the last hundred records, renders them, and then follows.
// Everything it draws arrives on that socket; there is no polling and no second endpoint.
//
// WHY THE SCHEME IS DERIVED RATHER THAN WRITTEN
// TLS is terminated at the gateway, so the page is `https` in the cluster and `http` in front of a
// local `cargo run` -- and the socket has to match, or it is blocked as mixed content in one
// environment and refused as a bad scheme in the other. Hardcoding either would work exactly where
// it was written and nowhere else.
(function () {
  "use strict";

  var log = document.getElementById("journal");
  var status = document.getElementById("journal-status");
  if (!log || !status) return;

  // The FEED's id, which is not the room's and is not derivable from it. Everything this script
  // addresses is under `/journal/<id>`, so nothing it builds can name the room.
  var feed = status.dataset.feed;
  if (!feed) return;

  // The last hundred records, or everything if the room is younger than that. The server caps this
  // regardless; asking for more here would only be refused further away.
  var REPLAY_LINES = 100;

  // How many lines stay in the document.
  //
  // A busy room produces thousands a minute -- a mass release is one per location -- and a page left
  // open overnight would otherwise hold a DOM node for every check since it was opened. The trim is
  // from the top, because this feed reads downward and the oldest line is the one nobody is looking
  // at.
  var MAX_LINES = 2000;

  // Reconnect backoff, in ms. Doubling, jittered, capped.
  //
  // Jittered for the reason the load tool's is: a room that drops one viewer usually drops all of
  // them -- a redeploy, a reap, a gateway restart -- and a fixed delay would bring every open page
  // back in the same instant.
  var RETRY_MIN = 500;
  var RETRY_MAX = 30000;
  var retry = RETRY_MIN;

  var socket = null;
  var cursor = null;
  var stuckToBottom = true;
  // The local calendar day of the last line drawn, so a day break is inserted when it changes.
  // Held out here rather than per batch: a batch boundary is a network artifact and must not
  // produce a heading, and a day can change between two frames as easily as inside one.
  var lastDay = null;

  // Archipelago's own item classes, which is where the colors come from. The bits are the protocol's
  // `flags`: 1 progression, 2 useful, 4 trap. A trap that is also progression reads as a trap, which
  // is the order that matters to somebody scanning the feed for what just happened to them.
  function itemClass(flags) {
    if (!flags) return "item";
    if (flags & 4) return "item trap";
    if (flags & 1) return "item progression";
    if (flags & 2) return "item useful";
    return "item";
  }

  function at(seconds) {
    var d = new Date(seconds * 1000);
    return isNaN(d.getTime()) ? "" : d;
  }

  function cell(parent, text, className) {
    var span = document.createElement("span");
    // **`textContent`, always.** Every name in this feed is untrusted text out of an uploaded seed,
    // and chat is somebody typing into a room.
    span.textContent = text;
    if (className) span.className = className;
    parent.appendChild(span);
    return span;
  }

  function line(event) {
    var row = document.createElement("div");
    row.className = "entry " + (event.type || "unknown");

    var when = at(event.at);
    var stamp = cell(row, "", "when");
    if (when) {
      // The trailing space is inside the cell rather than between cells: `white-space: pre` keeps
      // it, and the alternative -- markup whitespace -- is exactly what askama would strip.
      stamp.textContent = "[" + when.toTimeString().slice(0, 8) + "] ";
      // The absolute instant, in the reader's own zone, through the one thing that decides how an
      // instant is spelled here. A bare `toLocaleString` would render a different order per reader
      // and no zone at all -- see localtime.js.
      if (window.PunaTime) stamp.title = window.PunaTime.absolute(when.getTime());
    } else {
      stamp.textContent = "[--:--:--] ";
    }

    switch (event.type) {
      case "check":
        cell(row, event.finder_name || "slot " + event.finder, "who");
        cell(row, " sent ", "verb");
        cell(row, event.item_name || "item " + event.item, itemClass(event.flags));
        cell(row, " to ", "verb");
        cell(row, event.receiver_name || "slot " + event.receiver, "who");
        cell(row, " (", "verb");
        cell(row, event.location_name || "location " + event.location, "where");
        cell(row, ")", "verb");
        break;

      // **The slot number is deliberately absent.** pahoa journals chat "as the room broadcast
      // it", which already begins with the speaker's name — so prefixing the slot rendered
      // `slot 1: MooingYacht1: meow`, saying the same thing twice and in the less useful order.
      case "chat":
        cell(row, event.text || "", "chat-text");
        break;

      // **pahoa's own "this history is incomplete" marker.** It is rendered loudly and never
      // filtered: it is the only evidence that records are missing, and a viewer that skipped it
      // would present a partial history as a whole one.
      case "gap":
        cell(row, "⚠ " + (event.dropped || "some") + " records were dropped here", "gap-note");
        break;

      // The room's effective configuration, in words. Dumped as raw JSON it was the one line in the
      // feed nobody could read at a glance, and it is the line that explains why a release behaved
      // the way it did.
      case "options":
        cell(row, "room options ", "kind");
        cell(row, options(event), "hint");
        break;

      case "withheld":
        cell(
          row,
          event.count + (event.count === 1 ? " record is" : " records are") +
            " not shown on this feed",
          "hint"
        );
        break;

      case "unreadable":
        cell(row, "unreadable record", "gap-note");
        break;

      // Anything this build has never heard of. Rendered as itself rather than dropped, for the same
      // reason `gap` is: the one thing a history viewer must not do is quietly omit history.
      default:
        cell(row, event.type || "unknown", "kind");
        cell(row, " " + JSON.stringify(event), "hint");
    }
    return row;
  }

  // `options` in a sentence rather than a JSON blob.
  //
  // Ordered by how often it explains something rather than by the order pahoa emits it: the release
  // and collect modes are why a world emptied itself, and everything after them is background. `at`
  // and `type` are dropped because the line already carries both.
  var OPTION_LABELS = [
    ["release_mode", "release"],
    ["collect_mode", "collect"],
    ["remaining_mode", "remaining"],
    ["countdown_mode", "countdown"],
    ["hint_cost", "hint cost"],
    ["location_check_points", "points per check"],
    ["item_cheat", "item cheat"],
    ["compatibility", "compatibility"],
    ["password_mode", "passwords"],
    ["server_password_set", "server password"],
  ];

  function options(event) {
    var parts = [];
    OPTION_LABELS.forEach(function (pair) {
      var value = event[pair[0]];
      if (value === undefined || value === null) return;
      if (typeof value === "boolean") value = value ? "on" : "off";
      if (pair[0] === "hint_cost") value = value + "%";
      parts.push(pair[1] + " " + value);
    });
    // Anything pahoa adds that this list has not learned yet still shows, so a new option is visible
    // rather than silently absent from the one record that exists to report configuration.
    Object.keys(event).forEach(function (key) {
      if (key === "at" || key === "type") return;
      var known = OPTION_LABELS.some(function (pair) {
        return pair[0] === key;
      });
      if (!known) parts.push(key + " " + event[key]);
    });
    return parts.join(", ");
  }

  // The reader's own calendar day for an instant, as a comparable key.
  //
  // **Local, not UTC**, and that is the whole point: a feed spanning midnight in Tokyo has broken a
  // day even though UTC has not. Built from the local getters for the same reason `PunaTime` builds
  // its own — a key derived from `toISOString` would be a UTC day wearing a local label.
  function dayKey(date) {
    return date.getFullYear() + "-" + date.getMonth() + "-" + date.getDate();
  }

  function daybreak(date) {
    var row = document.createElement("div");
    row.className = "entry daybreak";
    cell(row, window.PunaTime ? window.PunaTime.day(date.getTime()) : "", "day");
    return row;
  }

  function nearBottom() {
    return log.scrollHeight - log.scrollTop - log.clientHeight < 40;
  }

  function append(events, withheld) {
    if (!events.length && !withheld) return;
    stuckToBottom = nearBottom();

    var batch = document.createDocumentFragment();
    events.forEach(function (event) {
      // A heading whenever the calendar day moves, including before the first line — the feed shows
      // times of day, so without it a reader has no idea which day they are looking at, and a
      // journal that spans a week looks like one where the clock runs backwards.
      var when = at(event.at);
      if (when) {
        var key = dayKey(when);
        if (key !== lastDay) {
          batch.appendChild(daybreak(when));
          lastDay = key;
        }
      }
      batch.appendChild(line(event));
    });
    if (withheld) batch.appendChild(line({ type: "withheld", count: withheld }));
    log.appendChild(batch);

    while (log.childElementCount > MAX_LINES) log.removeChild(log.firstElementChild);
    // Only follow if the reader was already at the bottom. Yanking the view back down while
    // somebody is reading upward is the single most annoying thing a live feed can do.
    if (stuckToBottom) log.scrollTop = log.scrollHeight;
  }

  function say(text, className) {
    status.textContent = text;
    status.className = className || "notice";
  }

  function open() {
    var scheme = location.protocol === "https:" ? "wss:" : "ws:";
    socket = new WebSocket(scheme + "//" + location.host + "/journal/" + feed + "/feed");

    socket.addEventListener("open", function () {
      // The server waits for this before replaying, so it decides where the page starts. `at` is
      // accepted here too, for the day this page offers a time to scroll back to.
      socket.send(JSON.stringify({ from: { lines: REPLAY_LINES } }));
    });

    socket.addEventListener("message", function (message) {
      var frame;
      try {
        frame = JSON.parse(message.data);
      } catch (e) {
        return;
      }
      if (frame.kind === "empty") {
        say("This room has no feed history yet. It is written while the room runs.", "notice");
        return;
      }
      if (typeof frame.cursor === "number") cursor = frame.cursor;
      append(frame.events || [], frame.withheld || 0);
      if (frame.kind === "replay") {
        // The backoff resets on a connection that got as far as a replay, not on one that merely
        // opened: a server that accepts and drops immediately should not be redialled twice a
        // second.
        retry = RETRY_MIN;
        say("Live — following this room's feed.", "notice");
        log.scrollTop = log.scrollHeight;
      }
    });

    socket.addEventListener("close", function () {
      say("Reconnecting to the room's feed…", "warning");
      var wait = retry / 2 + Math.random() * (retry / 2);
      retry = Math.min(retry * 2, RETRY_MAX);
      setTimeout(open, wait);
    });

    // `close` fires after `error`, so the reconnect is scheduled there and not twice.
    socket.addEventListener("error", function () {
      if (socket && socket.readyState === WebSocket.OPEN) socket.close();
    });
  }

  // A page in a background tab keeps its socket: the server pings, the traffic is a trickle, and
  // dropping it would mean a reconnect and a re-replay every time somebody switches tabs. The
  // tracker polls and therefore has to care about visibility; this does not.
  open();
})();
