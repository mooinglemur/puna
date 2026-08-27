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

  var earlier = document.getElementById("journal-earlier");
  var progress = document.getElementById("journal-progress");

  var socket = null;
  var cursor = null;
  var stuckToBottom = true;
  // The offset the oldest line on the page begins at. `null` until the first replay lands, `0` once
  // the walk has reached the beginning of the file and there is nothing earlier to ask for.
  var oldest = null;
  var backfilling = false;
  // How many earlier records the walk has pulled in, for the progress note. A whole-feed load on a
  // busy room is dozens of round trips over tens of seconds, and a note that says only "loading"
  // for all of them is indistinguishable from one that has stopped — which is precisely the
  // confusion the silent-stop bug above produced, and the reason a bare spinner would not do.
  var backfilled = 0;
  // Set once the reader asks for the whole feed. It turns the DOM trim off: the trim exists so a
  // page left open overnight does not accumulate a node per check, and it would otherwise eat the
  // top of exactly what the reader just asked to see.
  var keepEverything = false;
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

  // **Who the room says it was.** Every record carrying a person carries `player` off the
  // authenticated connection, and the slot number as a fallback for a record written before a name
  // was known. This is the only thing that ever fills the identity cell — never `source`, which is
  // the sending client's own claim.
  function who(row, event) {
    cell(row, event.player || "slot " + event.slot, "who");
  }

  // **What the sending client said its name was, when that is not the slot's.**
  //
  // `source` is copied straight out of the bounce payload and nothing in the protocol validates it,
  // so it must never stand in for the authenticated identity. It is not noise either: one slot can
  // be a whole group of people, which is exactly what Archipelago's Minecraft world does — several
  // accounts play through a single server that holds the slot, and `source` is the only thing in
  // the record that says which of them died. Withholding it would drop the one fact the room
  // cannot otherwise report.
  //
  // So it renders BESIDE the room's answer rather than instead of it, and only when the two
  // disagree — the case the reader is being told about. Identical values say nothing, and on a busy
  // feed a parenthetical after every link would train the eye to skip exactly the one that matters.
  //
  // `title` says where the value came from, because a name in parentheses reads as authority and
  // this one has none: a client that wants to name somebody else can.
  function claimed(row, event) {
    var source = event.source;
    if (typeof source !== "string") return;
    source = source.trim();
    // Untrusted, unbounded text on a one-line row. `textContent` makes it inert; the cap keeps one
    // client from pushing the rest of the record off the end of the line.
    if (source.length > 48) source = source.slice(0, 48) + "…";
    if (!source || source === event.player) return;
    var span = cell(row, " (" + source + ")", "claimed");
    span.title = "Reported by the sending client, not verified by the room";
  }

  // How many other slots a link reached. Suppressed at zero rather than rendered as "0 slots",
  // which reads as a failure where it usually means a solo room or a convention nobody else runs.
  function recipients(row, event) {
    if (!event.recipients) return;
    cell(
      row,
      " → " + event.recipients + (event.recipients === 1 ? " slot" : " slots"),
      "hint"
    );
  }

  // A connection's tags, which are what tell three connections on one slot apart. No leading
  // space: the call sites differ in what precedes them, and building the separator in here is how
  // `tags_changed` came out with a double space between its verb and its first list.
  function tags(list) {
    if (!Array.isArray(list) || !list.length) return "(no tags)";
    return "[" + list.join(", ") + "]";
  }

  // The build behind a `started`/`stopped` pair. `build_rev` ending in `+` means the tree was
  // dirty, which on a room off a CI image means something was built outside the pipeline.
  function build(event) {
    if (!event.version) return "";
    return " — pahoa " + event.version + (event.build_rev ? " (" + event.build_rev + ")" : "");
  }

  // An admin command's arguments. Rendered as JSON on purpose: the shape is per verb and open, so
  // any prettier rendering would be a table to keep in step with sixteen handlers in another
  // repository — and would quietly render the next verb as nothing.
  function detail(value) {
    if (!value || typeof value !== "object") return "";
    var parts = Object.keys(value).map(function (key) {
      var v = value[key];
      return key + ": " + (typeof v === "object" ? JSON.stringify(v) : String(v));
    });
    return parts.length ? " — " + parts.join(", ") : "";
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

      // The three link conventions, and the one rule that matters is whose name is shown.
      //
      // **`player` is the room's answer; `source` is the sending client's claim.** They are
      // recorded separately precisely because they can disagree — nothing in the protocol stops a
      // client putting somebody else's name in the payload — so a page rendering `source` as "who
      // killed you" would be rendering an assertion an attacker picks. Every one of these reads
      // `player`, which comes off the authenticated connection the packet arrived on. `source` is
      // never displayed at all; `RingLink` does not even carry a usable one.
      case "deathlink":
        who(row, event);
        claimed(row, event);
        cell(row, " died", "verb");
        if (event.cause) {
          cell(row, " — ", "verb");
          cell(row, event.cause, "where");
        }
        recipients(row, event);
        break;

      case "traplink":
        who(row, event);
        claimed(row, event);
        cell(row, " sent ", "verb");
        cell(row, event.trap_name || "a trap", "item trap");
        recipients(row, event);
        break;

      // `amount` is a number and keeps its own type, so it is legitimately negative — a ring link
      // relays a loss as readily as a gain. **The sign is the whole event**, so it is rendered as
      // the word rather than as a signed number: "sent -25 rings" describes half of these
      // backwards, and a bare `-25` beside a name is exactly the sort of thing a reader rounds off
      // to "sent".
      case "ringlink":
        who(row, event);
        // RingLink has no usable `source` -- that convention puts a client instance id where the
        // others put a name -- so pahoa records null rather than something wrong. `claimed` is
        // called anyway: the guard belongs in one place, and a convention that starts sending a
        // real name should surface without a change here.
        claimed(row, event);
        if (typeof event.amount !== "number") {
          cell(row, " changed rings", "verb");
        } else {
          cell(row, event.amount < 0 ? " lost " : " gained ", "verb");
          cell(row, Math.abs(event.amount) + " rings", "item useful");
        }
        recipients(row, event);
        break;

      // **The incarnation markers.** A file spans every run of a room, so without these a jump in
      // the timestamps could be a quiet night or a crash and there is no way to tell. A `started`
      // with no `stopped` before it is an unclean stop — that absence IS the signal, so the pair is
      // worth drawing plainly rather than interpreting here.
      case "started":
        cell(row, "▶ room started", "kind");
        cell(row, build(event), "hint");
        break;

      case "stopped":
        cell(row, "■ room stopped", "kind");
        // pahoa's own word, unchanged: `SIGTERM` is an orchestrated drain, `admin request` is the
        // shutdown endpoint, `SIGINT` is a person at a terminal. It matches the room's log line
        // exactly, so the two can be read together without a translation table.
        cell(row, event.reason ? " (" + event.reason + ")" : "", "where");
        cell(row, build(event), "hint");
        break;

      // One record per CONNECTION, not per player: a slot running a game client, a text client and
      // a tracker produces three. So the tags are worth showing — they are what tells those three
      // apart, and they decide whether a connection may claim the goal or receives chat at all.
      case "connected":
        who(row, event);
        cell(row, " connected", "verb");
        cell(row, event.game ? " — " + event.game : "", "where");
        cell(row, " " + tags(event.tags), "hint");
        break;

      // `slot_empty` is the field worth building on: closing one of three clients is ordinary, the
      // slot going dark is the thing somebody asks about later. Deriving it would mean replaying
      // every join and part from the top of the file.
      case "disconnected":
        who(row, event);
        cell(row, " disconnected", "verb");
        cell(row, event.slot_empty ? " — slot is now empty" : "", "hint");
        break;

      case "tags_changed":
        who(row, event);
        cell(row, " tags ", "verb");
        cell(row, tags(event.from) + " → " + tags(event.to), "where");
        break;

      // Written BEFORE the checks it causes, so it sits above the release burst rather than buried
      // under three thousand lines of it. Worth rendering as an arrival rather than a status.
      case "goal":
        who(row, event);
        cell(row, " finished", "verb");
        cell(row, event.game ? " " + event.game : "", "where");
        break;

      // Every mutating admin verb, recorded at the dispatch point as it was ASKED FOR -- so a
      // refused command still appears, which is equally interesting to somebody reconstructing a
      // dispute. What came of it is in the reply the operator got, not here.
      case "admin":
        cell(row, "admin ", "kind");
        cell(row, event.command || "command", "item");
        cell(row, typeof event.slot === "number" ? " on slot " + event.slot : "", "who");
        cell(row, detail(event.detail), "hint");
        break;

      // `!getitem`. It exists because no `check` can account for it: the item moves with no location
      // behind it, so without this line the history would show an item nobody found.
      case "cheat":
        who(row, event);
        cell(row, " conjured ", "verb");
        cell(row, event.item_name || "item " + event.item, itemClass(event.flags));
        break;

      // **Both balances, not just the cost.** Hint price is a percentage of a slot's own location
      // count and can be changed mid-room, so a cost in isolation cannot be checked against
      // anything afterwards. Equal balances mean a free hint -- an item at an already-checked
      // location -- which is usually the thing being adjudicated.
      case "hints":
        who(row, event);
        var granted = Array.isArray(event.granted) ? event.granted : [];
        cell(row, granted.length === 1 ? " hinted " : " hinted " + granted.length + "× ", "verb");
        cell(row, granted.join("; ") || "nothing", "where");
        if (typeof event.points_before === "number") {
          cell(
            row,
            " (" + (event.cost || 0) + " points: " + event.points_before + " → " +
              event.points_after + ")",
            "hint"
          );
        }
        break;

      case "option_changed":
        cell(row, "option ", "kind");
        cell(row, event.option || "?", "item");
        cell(row, " → ", "verb");
        cell(row, String(event.value), "where");
        break;

      // The VALUE is never in this record, by pahoa's design -- only whether one now exists.
      case "slot_password_changed":
        cell(row, "slot " + event.slot + " password ", "kind");
        cell(row, event.set ? "set" : "cleared", "where");
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

  function append(events) {
    if (!events.length) return;
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
    log.appendChild(batch);

    // Off once the reader has asked for the whole feed: the trim exists to stop an overnight page
    // accumulating a node per check, and it would otherwise eat the top of what they just loaded.
    if (!keepEverything) {
      while (log.childElementCount > MAX_LINES) log.removeChild(log.firstElementChild);
    }
    // Only follow if the reader was already at the bottom. Yanking the view back down while
    // somebody is reading upward is the single most annoying thing a live feed can do.
    if (stuckToBottom) log.scrollTop = log.scrollHeight;
  }

  // Older records, on the front.
  //
  // **The reader's position is held by pixel offset, not by scroll top.** Prepending shifts
  // everything down by exactly the height of what was added, so a naive prepend teleports the view
  // and the reader loses the line they were on — which is the whole reason to backfill in place
  // rather than clear and reload. Measuring the scroll height on both sides and adding the
  // difference back keeps the same record under the same pixel.
  function prepend(events) {
    if (!events.length) return;

    var batch = document.createDocumentFragment();
    var previousDay = null;
    events.forEach(function (event) {
      var when = at(event.at);
      if (when) {
        var key = dayKey(when);
        if (key !== previousDay) {
          batch.appendChild(daybreak(when));
          previousDay = key;
        }
      }
      batch.appendChild(line(event));
    });

    var before = log.scrollHeight;
    log.insertBefore(batch, log.firstChild);
    log.scrollTop += log.scrollHeight - before;

    // The page below this batch already has a heading for its own first day, and the batch has just
    // ended on some day of its own. If they agree, that heading is now a repeat.
    var headings = log.querySelectorAll(".daybreak");
    for (var i = headings.length - 1; i > 0; i--) {
      if (headings[i].textContent === headings[i - 1].textContent) {
        headings[i].remove();
      }
    }
  }

  function say(text, className) {
    status.textContent = text;
    status.className = className || "notice";
  }

  // **The fact that a feed is filtered belongs in the status line, once, not in the feed.**
  //
  // The server reports a `withheld` count per frame, and rendering it as a row put a timestamp-less
  // line into a stream where every other line is an event at an instant — so it read as something
  // having happened, scattered through the history once per batch, saying "1 record" each time.
  // That is metadata about a delivery, and a delivery is a network artifact the reader should never
  // see the seams of.
  //
  // It is still said, because a reader is owed the knowledge that they are looking at part of a
  // history rather than all of it — and it is said as **what this feed is** rather than as what is
  // missing from it. "(items only)" is the same phrase the room's own setting offers, so a reader
  // who goes looking for why sees the words they were shown; a count would describe whatever
  // happened to be fetched rather than anything about the room.
  var live = false;
  var filtered = false;

  function sayLive() {
    say(
      filtered
        ? "Live — following this room's feed (items only)."
        : "Live — following this room's feed.",
      "notice"
    );
  }

  function noteFiltering(count) {
    if (!count || filtered) return;
    filtered = true;
    if (live) sayLive();
  }

  // Ask for the page of records immediately before what is on screen.
  //
  // One request in flight at a time. The alternative — firing every page at once — would put a
  // thousand backwards seeks on a 250 MB file at a server that is also following it, to fill a DOM
  // the reader cannot scroll through anyway.
  function askForEarlier() {
    if (backfilling || oldest === null || oldest <= 0) return;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    backfilling = true;
    keepEverything = true;
    setEarlier(
      false,
      backfilled
        ? "Loading earlier records… " + backfilled + " so far."
        : "Loading earlier records…"
    );
    socket.send(JSON.stringify({ before: oldest }));
  }

  function setEarlier(enabled, note) {
    if (!earlier) return;
    earlier.hidden = !enabled && !note;
    earlier.disabled = !enabled;
    if (progress) progress.textContent = note || "";
  }

  if (earlier) {
    earlier.addEventListener("click", function () {
      askForEarlier();
    });
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

      // A backfill page goes on the front and never touches the follow cursor.
      if (frame.kind === "earlier") {
        prepend(frame.events || []);
        noteFiltering(frame.withheld);
        oldest = typeof frame.start === "number" ? frame.start : 0;
        backfilled += (frame.events || []).length;
        // **Cleared before the next ask, not in the arm that ends the walk.** This request is
        // finished — its page is on the screen — so `askForEarlier`'s in-flight guard is about the
        // *next* one. Leaving the flag set until the walk ended made that guard reject every
        // continuation, so the whole-feed button loaded one page and stopped: button disabled,
        // note frozen mid-sentence, nothing thrown, 5,000 records of 160,000 on the page. A silent
        // stop is the worst shape this could fail in, because it looks exactly like a short file.
        backfilling = false;
        if (oldest > 0) {
          // Keep walking. One page in flight at a time, so a slow disk backs the walk up rather
          // than queueing a thousand requests at a server reading a 250 MB file.
          askForEarlier();
        } else {
          setEarlier(false, "Showing the whole feed — " + backfilled + " earlier records.");
        }
        return;
      }

      append(frame.events || []);
      noteFiltering(frame.withheld);
      if (frame.kind === "replay") {
        // The backoff resets on a connection that got as far as a replay, not on one that merely
        // opened: a server that accepts and drops immediately should not be redialled twice a
        // second.
        retry = RETRY_MIN;
        live = true;
        sayLive();
        log.scrollTop = log.scrollHeight;

        // **Re-anchored on every replay, including after a reconnect.** A socket that dropped and
        // came back replays the tail, so the page's oldest line is whatever that replay began with
        // — carrying the previous `start` across would ask for a region the page no longer joins on
        // to, leaving a hole in the middle of the feed.
        oldest = typeof frame.start === "number" ? frame.start : null;
        backfilling = false;
        if (keepEverything && oldest > 0) {
          askForEarlier();
        } else {
          setEarlier(oldest === null || oldest > 0, "");
        }
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
