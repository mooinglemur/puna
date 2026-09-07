// The room's history, live.
//
// Opens a WebSocket to Puna, asks for the last hundred records, renders them, and then follows.
// Everything it draws arrives on that socket; there is no polling and no second endpoint.
//
// WHY THE SCHEME IS DERIVED RATHER THAN WRITTEN
// TLS is terminated at the gateway, so the page is `https` in the cluster and `http` in front of a
// local `cargo run`. and the socket has to match, or it is blocked as mixed content in one
// environment and refused as a bad scheme in the other. Hardcoding either would work exactly where
// it was written and nowhere else.
(function () {
  "use strict";

  var log = document.getElementById("journal");
  var status = document.getElementById("journal-status");
  var message = document.getElementById("journal-message");
  var link = document.getElementById("journal-link");
  if (!log || !status) return;

  // The FEED's id, which is not the room's and is not derivable from it. Everything this script
  // addresses is under `/journal/<id>`, so nothing it builds can name the room.
  var feed = status.dataset.feed;
  if (!feed) return;

  // --- HOW MUCH OF THE FEED THE READER HOLDS ------------------------------------------------------
  //
  // **One number, deciding three things this file used to decide separately**: how many records the
  // page asks for on connect, how far the backwards walk goes, and how many rows stay in the
  // document. Those were a constant, a button and a second constant, and every seam between them
  // showed. The button turned the trim off for good, so the only way back to a bounded page was to
  // reload it; it kept its label after there was nothing left to load, since nothing about it
  // expressed a state; and a reader who wanted a deeper window than the one this file had picked
  // had no way to say so at all.
  //
  // So the reader picks the window and the rest follows from it. `Infinity` is the whole feed,
  // which is the old button expressed as one of the sizes rather than as a mode.
  var WINDOW_SIZES = [Infinity, 10000, 5000, 2500, 1000, 500];

  // What somebody who has never chosen gets, and the one number here that every viewer pays for on
  // every load: the window is also the connect burst.
  //
  // A 500-line replay measures ~124 KiB raw, and `rocket_ws` sits on a tungstenite with no
  // permessage-deflate, so that is what crosses the wire. 1,000 doubles that and is still a page
  // load rather than an event, while landing near the 2,000 rows this page used to accumulate
  // before it trimmed. It also gives the filter box something to filter: a narrow search over a
  // hundred lines usually finds nothing and reads as broken.
  var DEFAULT_WINDOW = 1000;

  // The most the server will put in one frame, `journal::MAX_REPLAY_LINES`.
  //
  // **It clamps regardless, so this is not the enforcement.** It is how the page knows a window
  // larger than this arrives in pieces, and therefore that it has to keep asking after the first
  // frame lands rather than believing it has everything it wanted.
  var REPLAY_MAX = 5000;

  // Where the choice is remembered, in the one store `toggles.js` owns. Namespaced by hand like
  // every other key there, since they share a store across every page on this site.
  //
  // **`Infinity` is never written to it**, which is Troy's rule and a good one: loading a room's
  // whole history is a deliberate act with a real cost at both ends, and a preference that quietly
  // did it on every page load would be one nobody remembers setting. Choosing it leaves whatever
  // numeric window was remembered before untouched, so the next load comes back to that rather than
  // to the default.
  var WINDOW_KEY = "journal.lines";

  // How many rows stay in the document, and how much history the page tries to hold.
  //
  // A busy room produces thousands of lines a minute. A mass release is one per location, and a
  // page left open overnight would otherwise hold a DOM node for every check since it was opened.
  // The trim is from the top, because this feed reads downward and the oldest line is the one
  // nobody is looking at. **Day headings are rows and count toward it**: this is a statement about
  // the document, which is what the reader asked to bound, rather than about the history.
  var cap = DEFAULT_WINDOW;

  // Reconnect backoff, in ms. Doubling, jittered, capped.
  //
  // Jittered for the reason the load tool's is: a room that drops one viewer usually drops all of
  // them: a redeploy, a reap, a gateway restart, and a fixed delay would bring every open page
  // back in the same instant.
  var RETRY_MIN = 500;
  var RETRY_MAX = 30000;
  var retry = RETRY_MIN;

  var windowRow = document.getElementById("journal-window");
  var progress = document.getElementById("journal-progress");

  // --- THE VIEW FILTERS ---------------------------------------------------------------------------
  // A search box and three checkboxes over what is already on the page. Nothing here refetches,
  // nothing here is authorization, and every row a filter hides was sent to this reader and is one
  // keystroke or one click away.
  //
  // **The hiding is the stylesheet's, and the two halves get there differently.** A checkbox is a
  // property of the RECORD, so each row is painted with marker classes as it is built and the box
  // toggles one class on the feed: a class change on one element, and a record landing in ten
  // minutes is hidden or shown by the same rule with nothing having to remember what is ticked.
  //
  // A substring is a property of the ROW'S TEXT and CSS has no selector for it, so that one is
  // decided per row: at build time for a row arriving, and over the whole feed when the needle
  // changes. Both ends put the same class on, so the rule and the count stay one rule.
  var filters = document.getElementById("journal-filters");
  var filterNote = document.getElementById("journal-filter-note");
  var filterSearch = document.getElementById("journal-filter-search");

  // The current search, trimmed and lower-cased once rather than per row per keystroke.
  var needle = "";

  // The rows a non-empty search is hiding, spelled the way `puna.css` spells it. See `FILTERS`,
  // whose `hides` entries this joins for the count.
  var SEARCH_HIDES = ".entry:not(.unfilterable).unmatched";

  // **Both come from the server, and neither is retyped here.** `gameplay` is `PUBLIC_KINDS` out of
  // `routes/journal.rs`, the same list that decides what a public viewer is SENT: a second copy in
  // this file would drift, and it would drift in the quiet direction, hiding a record type pahoa
  // added from a page whose whole promise is that it omits no history. `slots` is this viewer's own,
  // empty for anybody holding none, in which case the control was never rendered.
  function words(name) {
    var raw = (filters && filters.dataset[name]) || "";
    return raw.split(/\s+/).filter(Boolean);
  }

  var GAMEPLAY_KINDS = words("gameplayKinds");
  var MY_SLOTS = words("mySlots").map(Number);

  // Records no filter may hide, whatever is ticked.
  //
  // `gap` is pahoa's own "records were dropped here" marker and is the only evidence a history is
  // incomplete: `routes/journal.rs` refuses to filter it at any policy, and a view filter that
  // hid it would present a partial history as a whole one through the front door instead of the
  // back. The day headings are the other, by the same argument in miniature: they are not records,
  // and a feed that silently lost its calendar while filtered would be one where the clock appears
  // to run backwards.
  var UNFILTERABLE = ["gap"];

  // The three link conventions, which the personal filter never hides.
  //
  // A `deathlink`/`traplink` record carries the SENDER's slot and a recipient COUNT,
  // never a recipient list, so "did this one reach me" is not answerable from the record at all.
  // Shown rather than answered wrongly: the alternative is a filter that quietly drops the deaths
  // somebody opened the feed to explain.
  var LINK_KINDS = ["deathlink", "traplink"];

  var socket = null;
  // The follow position, in bytes into the room's history file. Advanced by every frame the server
  // sends, and sent back on a RECONNECT so the feed resumes exactly where it stopped rather than
  // replaying a tail the page already shows.
  var cursor = null;
  // Whether the connection now opening asked to resume, and the offset it asked from. Kept because
  // the server's answer is only interpretable against the question: a `start` equal to what was
  // asked is a clean join, and anything else means it served a tail instead.
  var resumed = false;
  var cursorAsked = null;
  // The pending redial, so it can be cancelled when the tab goes away. `null` means none is armed.
  var reconnectTimer = null;
  // --- THE DEAD-LINK WATCHDOG ---------------------------------------------------------------------
  // **A WebSocket does not tell you it has stopped working.** Blocking the site with iptables drops
  // packets rather than resetting the connection, so TCP retransmits into the void with exponential
  // backoff. up to about fifteen minutes on Linux before it gives up, and until then the socket is
  // open, `close` never fires and `readyState` is still `OPEN`. Measured: five minutes on a green
  // dot, then a silent recovery when the block lifted, which was the same connection catching up
  // rather than a reconnect.
  //
  // The protocol's own ping cannot help, and this is the part worth knowing: **the browser
  // WebSocket API exposes no ping or pong to JavaScript**. The browser answers the server's pings
  // by itself and tells the page nothing. So liveness has to be an ordinary message the page can
  // see, and a timer that gives up when one stops arriving.
  var aliveTimer = null;
  // Bumped on every dial and on every abandonment. Each socket's handlers capture the value they
  // were opened under and do nothing once it has moved, so a `close` or a stray frame arriving
  // late from a socket the watchdog gave up on cannot reach in and reset the live one's state.
  var epoch = 0;
  // Filled from the opening frame's `heartbeat_ms`, so there is one authority for the cadence.
  // The multiplier absorbs a missed beat and a slow network; a background tab throttles timers, but
  // throttling makes them fire LATE rather than early, which is the safe direction for a watchdog.
  var HEARTBEAT_MISSES = 2.5;
  var aliveAfter = 0;
  var stuckToBottom = true;
  // Whether a backfill page is in flight. One at a time, always: see `fill`.
  var backfilling = false;
  // How many earlier records the walk has pulled in, for the progress note. A whole-feed load on a
  // busy room is dozens of round trips over tens of seconds, and a note that says only "loading"
  // for all of them is indistinguishable from one that has stopped, which is precisely the
  // confusion the silent-stop bug above produced, and the reason a bare spinner would not do.
  var backfilled = 0;
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

  // --- WHAT A ROW IS, FOR THE FILTERS -------------------------------------------------------------
  // Painted once as the row is built, never recomputed. Everything below answers a question about
  // the RECORD; which of those answers currently hides a row is the stylesheet's business.

  function isMine(slot) {
    return typeof slot === "number" && MY_SLOTS.indexOf(slot) !== -1;
  }

  // **Any record naming one of your slots**, not only the item traffic.
  //
  // Deliberately generous, and stated as the rule rather than arrived at by listing types: a record
  // is about you if the room says your slot is in it. The narrower reading would need an allowlist
  // of types to keep in step with pahoa's sixteen, and its failures are the bad kind: a filter
  // called "my slots" that hides your own goal, or the release that emptied your world, is one a
  // reader has to learn the exceptions to before they can trust it.
  //
  // `finder` and `receiver` are the check's two ends; `slot` is what everything else names its
  // subject with.
  function isPersonal(event) {
    if (!MY_SLOTS.length) return false;
    if (LINK_KINDS.indexOf(event.type) !== -1) return true;
    return isMine(event.finder) || isMine(event.receiver) || isMine(event.slot);
  }

  // **Archipelago's own classification, not a judgment made here.** `flags` 0 is filler: neither
  // progression (1), useful (2) nor a trap (4), the same bits `itemClass` colors by.
  //
  // Only a record that CARRIES an item can be filler. Everything else is unclassified rather than
  // filler, and this filter removes what is classified as filler rather than everything that is not
  // classified as something else, which is the difference between hiding a thousand junk items and
  // hiding the chat.
  function isFiller(event) {
    if (event.type !== "check" && event.type !== "cheat") return false;
    return !event.flags;
  }

  // What the reader typed, reduced to what a row is compared against.
  //
  // **Its own function so the normalization is somewhere a test can reach.** Trimming and
  // lower-casing here rather than per row is the cheap half of the reason; the deciding half is
  // that case-insensitivity is a property somebody would assert, and with this inline in the input
  // handler the only thing that could exercise it is a browser. A test written against the
  // comparison alone would pass whatever the handler did, which is a check on nothing.
  //
  // Once per keystroke rather than once per row per keystroke, which on a feed somebody has loaded
  // in full is the difference between one `toLowerCase` and a hundred and sixty thousand.
  function setNeedle(raw) {
    needle = String(raw || "")
      .trim()
      .toLowerCase();
  }

  // Whether one row survives the current search.
  //
  // **`unfilterable` is exempt here as it is everywhere else**, which is also what makes the
  // `searchText` below safe to read without a guard: every row that reaches the comparison came out
  // of `line`, and the day headings, which do not, never reach it.
  //
  // Matched against the RENDERED text rather than the record's fields, the same rule `table.js`
  // states: what you can see is what you can search, and a field the feed does not show cannot
  // match invisibly. So "sword" finds the item, "lemur" finds the player, and a timestamp finds
  // the minute.
  function applySearch(row) {
    if (row.classList.contains("unfilterable")) return;
    row.classList.toggle("unmatched", needle !== "" && row.searchText.indexOf(needle) === -1);
  }

  // Every row again, for a needle that changed.
  //
  // O(rows) per keystroke, which is why `line` caches each row's text: `textContent` walks and
  // concatenates a row's spans every time it is read, and this page can be holding the whole feed
  // if somebody pressed the button. The cached string turns a keystroke into a run of
  // `indexOf`.
  function refreshSearch() {
    for (var i = 0; i < log.children.length; i++) applySearch(log.children[i]);
  }

  function marks(event) {
    var out = "";
    if (UNFILTERABLE.indexOf(event.type) !== -1) out += " unfilterable";
    if (GAMEPLAY_KINDS.indexOf(event.type) !== -1) out += " gameplay";
    if (isPersonal(event)) out += " personal";
    if (isFiller(event)) out += " filler";
    return out;
  }

  // A person's name, marked when the slot behind it is one of the viewer's.
  //
  // **The reason this is one function rather than a class added at each call site**: a slot's name
  // appears as a subject in nine record types and as a *recipient* in one, and the recipient is the
  // half a player is scanning for. Marking it wherever a name is rendered means a record type added
  // later gets it by using this, and a name rendered some other way is visibly the odd one out.
  //
  // **Nothing at all for an anonymous or non-participant viewer**, which needs no branch here:
  // `MY_SLOTS` is empty for them, so `isMine` is false for every slot and every cell comes out with
  // the class the feed has always had. The attribute the list comes from is rendered only for
  // somebody holding a slot in this room.
  //
  // Marked on the CELL rather than the row, deliberately. The row already carries `personal` for the
  // filter, which is a question about the whole record; this is about one name inside it, and on a
  // check between two other players the answer differs between the two names on the same line.
  function name(row, text, slot) {
    return cell(row, text, isMine(slot) ? "who mine" : "who");
  }

  // **Who the room says it was.** Every record carrying a person carries `player` off the
  // authenticated connection, and the slot number as a fallback for a record written before a name
  // was known. This is the only thing that ever fills the identity cell, never `source`, which is
  // the sending client's own claim.
  function who(row, event) {
    name(row, event.player || "slot " + event.slot, event.slot);
  }

  // **What the sending client said its name was, when that is not the slot's.**
  //
  // `source` is copied straight out of the bounce payload and nothing in the protocol validates it,
  // so it must never stand in for the authenticated identity. It is not noise either: one slot can
  // be a whole group of people, which is exactly what Archipelago's Minecraft world does. Several
  // accounts play through a single server that holds the slot, and `source` is the only thing in
  // the record that says which of them died. Withholding it would drop the one fact the room
  // cannot otherwise report.
  //
  // So it renders BESIDE the room's answer rather than instead of it, and only when the two
  // disagree, the case the reader is being told about. Identical values say nothing, and on a busy
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

  // The convention's name, first on the line after the timestamp.
  //
  // **A DeathLink and a TrapLink are not things the reader did or was told; they are things that
  // happened TO them**, arriving from a game they may not be playing, and the question a feed gets
  // opened to answer is "why did I get a trap I never earned". Leading with the convention answers
  // it before the sentence starts, and it is what makes the two scannable in a column where every
  // other line begins with somebody's name.
  //
  // The trailing space rides INSIDE the cell rather than between cells, the same reason the
  // timestamp's does: `white-space: pre` keeps it, and markup whitespace is what askama strips.
  //
  // A helper for two call sites, because the format is the thing that would drift: brackets, a
  // space, and the convention spelled the way Archipelago spells it.
  function convention(row, name) {
    cell(row, "[" + name + "] ", "convention");
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

  // **What a connection is DOING, in the reference's own words.**
  //
  // `_non_game_messages` in `MultiServer.py`, transcribed rather than paraphrased: a tag decides
  // the verb, and every Archipelago client has always announced a join as "X playing Balatro has
  // joined" or "X tracking has joined". Order is the reference's, because a connection can carry
  // more than one of these and the first match wins there.
  //
  // Worth inheriting rather than rendering the raw tags, because it is the difference between a
  // reader knowing a tracker just attached and a reader parsing `["AP","Tracker"]` to work it out.
  var CLIENT_VERBS = [
    ["HintGame", "hinting"],
    ["Tracker", "tracking"],
    ["TextOnly", "viewing"],
  ];

  function clientVerb(list) {
    if (Array.isArray(list)) {
      for (var i = 0; i < CLIENT_VERBS.length; i++) {
        if (list.indexOf(CLIENT_VERBS[i][0]) !== -1) return CLIENT_VERBS[i][1];
      }
    }
    return "playing";
  }

  // The build behind a `started`/`stopped` pair. `build_rev` ending in `+` means the tree was
  // dirty, which on a room off a CI image means something was built outside the pipeline.
  function build(event) {
    if (!event.version) return "";
    return " - pahoa " + event.version + (event.build_rev ? " (" + event.build_rev + ")" : "");
  }

  // An admin command's arguments. Rendered as JSON on purpose: the shape is per verb and open, so
  // any prettier rendering would be a table to keep in step with sixteen handlers in another
  // repository, and would quietly render the next verb as nothing.
  function detail(value) {
    if (!value || typeof value !== "object") return "";
    var parts = Object.keys(value).map(function (key) {
      var v = value[key];
      return key + ": " + (typeof v === "object" ? JSON.stringify(v) : String(v));
    });
    return parts.length ? " - " + parts.join(", ") : "";
  }

  // What set off a bulk release or collect, in words rather than as a bare tag.
  //
  // Absent for anybody at the feed tier, which is the common case and must read as a complete
  // sentence rather than as a line with something missing off the end. Present for an organizer,
  // where the four values are pahoa's: goal, admin, player, group.
  //
  // An unrecognized value falls through to nothing rather than being printed raw: a future pahoa
  // trigger is a word this build cannot phrase, and "(quorum)" in the middle of a sentence reads
  // as a bug rather than as news.
  function trigger(value) {
    switch (value) {
      // **Phrased to fit BOTH records**, which the first version did not. "an organizer cleared it"
      // and "they gave up on it" describe releasing a world; on a collect they are not merely
      // clumsy but false, since a collect is the opposite motion: pulling your own items in. And
      // `trigger` is withheld from the feed tier, so the only people who would ever have read those
      // lines are the ones who would know they were wrong.
      case "goal":
        return " - on reaching their goal";
      case "admin":
        return " - at an organizer's request";
      case "player":
        return " - at their own request";
      // A group slot sweeping up because its last member finished. Deliberately not "player":
      // pahoa gave it its own value rather than attributing a group's sweep to whoever happened
      // to finish last.
      case "group":
        return " - when the group finished";
      default:
        return "";
    }
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

  // `arriving` is the stylesheet's cue to open the row and fade its text in, and it is passed
  // through rather than decided here: only a record that landed on a live feed is new to the
  // reader. See the `arriving` rules in puna.css for what animating a replay would look like.
  function line(event, arriving, begins) {
    var row = document.createElement("div");
    row.className =
      "entry " + (event.type || "unknown") + (arriving ? " arriving" : "") + marks(event);
    // Where this record begins in the room's history file, off the frame's `starts`. It is what
    // lets the page walk backwards from wherever the trim has left it; see `pageStart`.
    if (typeof begins === "number") row.dataset.start = String(begins);

    var when = at(event.at);
    var stamp = cell(row, "", "when");
    if (when) {
      // The trailing space is inside the cell rather than between cells: `white-space: pre` keeps
      // it, and the alternative. markup whitespace, is exactly what askama would strip.
      stamp.textContent = "[" + when.toTimeString().slice(0, 8) + "] ";
      // The absolute instant, in the reader's own zone, through the one thing that decides how an
      // instant is spelled here. A bare `toLocaleString` would render a different order per reader
      // and no zone at all. See localtime.js.
      if (window.PunaTime) stamp.title = window.PunaTime.absolute(when.getTime());
    } else {
      stamp.textContent = "[--:--:--] ";
    }

    switch (event.type) {
      // **Two sentences, and the reference implementation writes both.**
      //
      // `json_format_send_event` in `MultiServer.py` branches on whether the finder is also the
      // receiver: "X found their Y (location)" when it is, "X sent Y to Z (location)" when it is
      // not. Every Archipelago client has always rendered it that way, so a feed that said "Lemur
      // sent Sword to Lemur" would be describing the most ordinary event in a multiworld (a player
      // finding something of their own) in words no player has ever seen it in.
      //
      // Compared on the slot NUMBERS rather than the names: the numbers are what the room means by
      // identity, and they are present on records whose names are not.
      case "check":
        name(row, event.finder_name || "slot " + event.finder, event.finder);
        if (event.finder === event.receiver) {
          cell(row, " found their ", "verb");
          cell(row, event.item_name || "item " + event.item, itemClass(event.flags));
        } else {
          cell(row, " sent ", "verb");
          cell(row, event.item_name || "item " + event.item, itemClass(event.flags));
          cell(row, " to ", "verb");
          name(row, event.receiver_name || "slot " + event.receiver, event.receiver);
        }
        cell(row, " (", "verb");
        cell(row, event.location_name || "location " + event.location, "where");
        cell(row, ")", "verb");
        break;

      // **The slot number is deliberately absent.** pahoa journals chat "as the room broadcast
      // it", which already begins with the speaker's name. so prefixing the slot rendered
      // `slot 1: MooingYacht1: meow`, saying the same thing twice and in the less useful order.
      case "chat":
        cell(row, event.text || "", "chat-text");
        break;

      // The two link conventions this build renders, and the one rule that matters is whose name
      // is shown.
      //
      // **RingLink was the third and its renderer is gone**, removed 2026-09-07 once pahoa stopped
      // journaling it: it fires on a counter changing, so a room running a game that uses it would
      // fill a history with records nobody opens a feed to read. No prod journal had ever carried
      // one, which is what made deleting the renderer cheaper than keeping it. It is out of
      // `PUBLIC_KINDS` with it, so a record that turned up anyway is treated as any kind this build
      // does not know: withheld from a feed viewer and counted, raw for an organizer. That is the
      // fail-closed default rather than a special case.
      //
      // It remains a real bounce tag on the wire, and `model::filter`'s `BOUNCE_TAGS` still offers
      // it: what a room may FILTER is a different question from what its history records.
      //
      // **`player` is the room's answer; `source` is the sending client's claim.** They are
      // recorded separately precisely because they can disagree. Nothing in the protocol stops a
      // client putting somebody else's name in the payload, so a page rendering `source` as "who
      // killed you" would be rendering an assertion an attacker picks. Every one of these reads
      // `player`, which comes off the authenticated connection the packet arrived on. `source` is
      // never displayed at all.
      case "deathlink":
        convention(row, "DeathLink");
        who(row, event);
        claimed(row, event);
        cell(row, " died", "verb");
        if (event.cause) {
          cell(row, " - ", "verb");
          cell(row, event.cause, "where");
        }
        recipients(row, event);
        break;

      case "traplink":
        convention(row, "TrapLink");
        who(row, event);
        claimed(row, event);
        cell(row, " sent ", "verb");
        cell(row, event.trap_name || "a trap", "item trap");
        recipients(row, event);
        break;


      // **The incarnation markers.** A file spans every run of a room, so without these a jump in
      // the timestamps could be a quiet night or a crash and there is no way to tell. A `started`
      // with no `stopped` before it is an unclean stop. That absence IS the signal, so the pair is
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
      // a tracker produces three. **The reference's sentence, minus the team**: `on_client_joined`
      // announces "X (Team #1) playing Balatro has joined. Client(0.6.8), {'AP'}."  The verb comes
      // from the tags, so a tracker attaching reads as tracking rather than as an array to parse.
      //
      // `(Team #1)` is dropped deliberately: one team exists and nothing can generate a second, so
      // it is a constant on every line. See `model::slot`'s note on why Puna keys on slot alone.
      case "connected":
        who(row, event);
        cell(row, " " + clientVerb(event.tags), "verb");
        cell(row, event.game ? " " + event.game : "", "where");
        cell(row, " has joined", "verb");
        cell(row, event.version ? " - client " + event.version : "", "hint");
        cell(row, " " + tags(event.tags), "hint");
        break;

      // `slot_empty` is the field worth building on: closing one of three clients is ordinary, the
      // slot going dark is the thing somebody asks about later. Deriving it would mean replaying
      // every join and part from the top of the file.
      // The reference's counterpart, `on_client_left`: "has left the game" for a game client, and
      // "has stopped tracking the game" for one of the others. `slot_empty` is Puna's own addition.
      // The reference has no equivalent, and it is the half somebody actually asks about later,
      // since closing one of three clients is ordinary and the slot going dark is not.
      case "disconnected":
        who(row, event);
        var verb = clientVerb(event.tags);
        cell(
          row,
          verb === "playing" ? " has left the game" : " has stopped " + verb + " the game",
          "verb"
        );
        cell(row, event.slot_empty ? " - slot is now empty" : "", "hint");
        break;

      case "tags_changed":
        who(row, event);
        cell(row, " tags ", "verb");
        cell(row, tags(event.from) + " → " + tags(event.to), "where");
        break;

      // Written BEFORE the checks it causes, so it sits above the release burst rather than buried
      // under three thousand lines of it. Worth rendering as an arrival rather than a status.
      // `on_goal_achieved`'s wording, again without the team. Not "finished": the reference has
      // said "has completed their goal" since forever, and it is the line a player screenshots.
      case "goal":
        who(row, event);
        cell(row, " has completed their goal", "verb");
        cell(row, event.game ? " - " + event.game : "", "where");
        break;

      // A world emptied in one go, written by the room BEFORE the checks it produces, so this line
      // sits above its own flood and explains it.
      //
      // `trigger` is rendered only when it is there. An organizer gets it; a viewer at the feed
      // tier does not, because who decided to clear a world is the room's operation rather than
      // its play - see PUBLIC_BULK_FIELDS in routes/journal.rs. So this arm must read correctly
      // with the field absent, which is the ordinary case for most people looking at it.
      case "release":
      case "collect":
        who(row, event);
        // **Both phrased as "verb from where", so the count cell completes the sentence.**
        //
        cell(row, event.type === "release"
          ? " released from their world"
          : " collected from other worlds", "verb");
        cell(row, typeof event.items === "number" ? " - " + event.items + " item" + (event.items === 1 ? "" : "s") : "", "item");
        cell(row, trigger(event.trigger), "hint");
        break;

      // Every mutating admin verb, recorded at the dispatch point as it was ASKED FOR, so a
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
      // anything afterwards. Equal balances mean a free hint, an item at an already-checked
      // location, which is usually the thing being adjudicated.
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

      // The VALUE is never in this record, by pahoa's design, only whether one now exists.
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
    // **Read once, here, where the row is finished and before it is in the document.**
    //
    // An expando rather than a `data-` attribute deliberately: this is a search index, not markup,
    // and a `dataset` write would serialize a copy of every line into the DOM for a reader to
    // stumble over in devtools and for nothing to read but this file.
    //
    // The saving is real rather than theoretical. `textContent` walks and concatenates a row's
    // spans on every read, and a search over a feed somebody has loaded in full would do that for
    // every row on every keystroke.
    row.searchText = row.textContent.toLowerCase();
    applySearch(row);
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
  // its own. A key derived from `toISOString` would be a UTC day wearing a local label.
  function dayKey(date) {
    return date.getFullYear() + "-" + date.getMonth() + "-" + date.getDate();
  }

  function daybreak(date, begins) {
    var row = document.createElement("div");
    // **The offset of the record it introduces, because a heading holds no bytes of its own.** It
    // is equally true of the heading, and carrying it is what lets a trim that stops on one leave
    // the page still able to say where it begins.
    if (typeof begins === "number") row.dataset.start = String(begins);
    // `unfilterable` for the reason the constant gives: a heading is not a record, and a filtered
    // feed that lost its calendar reads as one where the clock runs backwards. A heading left
    // standing over rows that are all hidden is the deliberate cost of that, and it is a true
    // statement: the feed spans that day and nothing on it matched.
    row.className = "entry daybreak unfilterable";
    cell(row, window.PunaTime ? window.PunaTime.day(date.getTime()) : "", "day");
    return row;
  }

  function nearBottom() {
    return log.scrollHeight - log.scrollTop - log.clientHeight < 40;
  }

  // How many records may arrive together and still open one at a time.
  //
  // The animation exists so a record landing on a quiet feed is something the eye catches. A release
  // lands hundreds at once, where nothing is individually perceivable, the effect degrades into the
  // whole block sliding, and the cost is a few hundred simultaneous height animations invalidating
  // the log's layout every frame, during the busiest thing a room ever does. Past a handful it is a
  // burst rather than an event, and the rows simply appear.
  var ARRIVE_MAX = 10;

  // How long to hold the bottom after a live row lands.
  //
  // **Deliberately longer than the animation rather than equal to it**, so this is a ceiling on how
  // long the feed follows itself and not a second copy of a duration the stylesheet owns. The
  // asymmetry is what makes that safe: overshooting costs nothing, since the follow stops the moment
  // the reader scrolls away, while undershooting leaves the newest line below the fold.
  var FOLLOW_MS = 250;

  var followUntil = 0;
  var following = false;
  // The scroll position this page last WROTE, read back after writing it because the browser clamps
  // what it is given. `-1` means it has written none. See `readerMoved`.
  var pinnedAt = -1;

  // Put the view at the bottom and remember where that was.
  //
  // One function for every place that pins, so `pinnedAt` cannot go stale behind a raw assignment:
  // a pin the page forgot it made is indistinguishable, one frame later, from the reader having
  // scrolled there.
  function pinBottom() {
    log.scrollTop = log.scrollHeight;
    pinnedAt = log.scrollTop;
  }

  // The view is exactly where this page last put it, which is to say nobody has moved it since.
  //
  // Every pin is written at a bottom, so this is also "the reader is at the bottom" stated in a way
  // that survives the bottom moving, which is the whole difficulty: growth, a trim and a clamp all
  // move the end while leaving the reader where they were.
  function atOwnPin() {
    return pinnedAt >= 0 && Math.abs(log.scrollTop - pinnedAt) <= 2;
  }

  // **Has the READER moved the view, as opposed to the page's own rows moving under it?**
  //
  // It takes BOTH signals, and each on its own has now been wrong here in a different direction.
  //
  // Distance alone was the shipped bug. The follow gave up when the view was no longer near the
  // bottom, which is exactly what a batch of opening rows makes true: one row growing is 22 pixels
  // and stays inside the 40-pixel tolerance, so a quiet feed looked perfect, while a release lands
  // a few hundred at once, they grow by hundreds of pixels between two frames, and the follow read
  // its own animation as a reader scrolling away. The feed then sat where it had been left, and
  // every later batch measured `nearBottom()` as false and never pinned again.
  //
  // Position alone is wrong the other way, which a simulation of that release caught before this
  // shipped twice: **the browser moves `scrollTop` too.** A row opening from no height makes the
  // content shorter for a frame, and the view is clamped to the new end, so the position the page
  // last wrote is gone through nobody's doing.
  //
  // Together they are exact. A clamp leaves the view AT the end, so the distance answers it; growth
  // leaves `scrollTop` untouched, so the position answers that. Only a reader changes the position
  // to somewhere that is not the end. Two pixels of slack for fractional scroll offsets on a scaled
  // display, which are the page's own value rounded rather than anybody's intent.
  function readerMoved() {
    return pinnedAt >= 0 && !atOwnPin() && !nearBottom();
  }

  // Keep the bottom pinned while an arriving row is still growing.
  //
  // A live row opens from no height, so the position that was the bottom when it was inserted stops
  // being the bottom on the very next frame: the browser clamps `scrollTop` when content shrinks
  // under it and does not put it back when the content grows again. Pinning once is therefore not
  // enough, and without this the line that just arrived would sit below the fold until the one after
  // it pushed it into view. Following for the length of the animation is also what produces the
  // effect the animation is for: the feed slides up as the row opens.
  function followBottom() {
    followUntil = Date.now() + FOLLOW_MS;
    if (following) return;
    following = true;
    requestAnimationFrame(function step() {
      // **The reader always wins.** Following is a courtesy and fighting a wheel event is the one
      // thing a live feed must never do, so a scroll of their own ends it immediately rather than at
      // the deadline.
      if (readerMoved()) {
        following = false;
        // The view is theirs now. Forgetting the pin is what stops anything later reading a stale
        // one as permission to scroll them back.
        pinnedAt = -1;
        return;
      }
      pinBottom();
      if (Date.now() < followUntil) {
        requestAnimationFrame(step);
      } else {
        following = false;
      }
    });
  }

  // **Anything above the feed changing height knocks the view off the bottom, and this page does it
  // to itself.** The controls above the feed are rendered hidden and revealed by this script, and
  // revealing one takes its height out of the feed, which is the flex item holding the page's
  // slack. Reported on the deployed page and reproducible every time: the feed opened one line
  // short of the bottom. The progress note appearing and wrapping does the same thing, and so do a
  // window resize, a phone rotating, and whatever gets added above the feed next.
  //
  // The ordering fix in the replay branch handles the known case. This handles the class: the feed's
  // own height changing re-pins, unless the reader has taken the view somewhere themselves. Guarded
  // rather than assumed, because it is not in Safari before 13.1 and a missing observer costs the
  // safety net rather than the page.
  if (window.ResizeObserver) {
    // Constructed off the same reference the guard tested, which is also this file's idiom for
    // anything it does not own (see `window.PunaTime`).
    new window.ResizeObserver(function () {
      if (atOwnPin() || nearBottom()) pinBottom();
    }).observe(log);
  }

  // --- WHERE THE PAGE BEGINS --------------------------------------------------------------------
  //
  // The backwards walk asks for the records immediately *before* a byte offset, so it needs the
  // offset the page's oldest line begins at. **Every row carries its own**, off the `starts` array
  // the server sends beside the events, so the answer is read back off the document rather than
  // remembered.
  //
  // That is the whole reason the wire carries one per record rather than one per frame. A frame's
  // own boundary describes the page for about a second: the trim drops rows off the top as new ones
  // arrive, and a remembered offset then names a line that is no longer there. Walking back from it
  // prepends records that stop short of what is on screen, which is a hole in the middle of the feed
  // with nothing marking it, on a page whose whole promise is that it omits no history. An earlier
  // build of this file answered "I cannot say" in that case and rebuilt the window from the end,
  // which was honest and cost the reader their place on every widening. One offset per record costs
  // about seven bytes each and removes the question.
  //
  // A day heading is not a record and holds no bytes, so it takes the offset of the record it
  // introduces: equally true, and it means a trim that stops on a heading has still left the page
  // able to say where it begins.
  function pageStart() {
    var first = log.firstElementChild;
    if (!first || first.dataset.start === undefined) return null;
    return Number(first.dataset.start);
  }

  // Records on the page, which is what every note counts. Day headings are rows and are not
  // records: the trim counts them because they hold a DOM node, and nothing a reader is told ever
  // should.
  function records() {
    return log.querySelectorAll(".entry:not(.daybreak)").length;
  }

  // Drop rows off the top until the document is inside the cap, leaving the reader's line under the
  // reader's eye.
  //
  // Removing content above the view shifts everything up by exactly its height, so a naive trim
  // teleports a reader who has scrolled back: the same problem `prepend` solves in the other
  // direction, and the same fix. It is also right for a reader sitting at the bottom with no
  // special case, since the bottom moves up by the amount the position does.
  function trimToCap() {
    if (cap === Infinity || log.childElementCount <= cap) return;
    var before = log.scrollHeight;
    while (log.childElementCount > cap) log.removeChild(log.firstElementChild);
    log.scrollTop -= before - log.scrollHeight;
  }

  // `live` is true only for an `append` frame: those are the records that arrived while the reader
  // was watching, and they are the only ones worth animating.
  function append(events, live, starts) {
    if (!events.length) return;
    // **Mid-animation counts as being at the bottom.** A batch landing while the last one is still
    // opening measures a distance the page's own rows are creating, so `nearBottom()` on its own
    // would read a busy feed as a reader who had scrolled away and stop following after the first
    // frame of it. A follow still running that the reader has not interrupted is this page saying it
    // is still at the bottom.
    stuckToBottom = nearBottom() || (following && !readerMoved());

    var batch = document.createDocumentFragment();
    events.forEach(function (event, i) {
      var begins = starts && starts[i];
      // A heading whenever the calendar day moves, including before the first line. The feed shows
      // times of day, so without it a reader has no idea which day they are looking at, and a
      // journal that spans a week looks like one where the clock runs backwards.
      var when = at(event.at);
      if (when) {
        var key = dayKey(when);
        if (key !== lastDay) {
          batch.appendChild(daybreak(when, begins));
          lastDay = key;
        }
      }
      batch.appendChild(line(event, live, begins));
    });
    log.appendChild(batch);

    trimToCap();
    // After the trim, because the note counts what is on the page and the trim is what decides that.
    // Before the pin, because the note sits above the feed and the feed is the flex item holding
    // this page's slack: a note that grows or shrinks after the pin moves the bottom the pin just
    // wrote. See `.feed-page`.
    refreshFilterNote();
    // Only follow if the reader was already at the bottom. Yanking the view back down while
    // somebody is reading upward is the single most annoying thing a live feed can do.
    if (stuckToBottom) {
      // After the trim, never before it: removing lines from the top moves the bottom, and a
      // remembered position from before that is a position the reader never occupied.
      pinBottom();
      // And keep following for as long as the row is opening. Replay and backfill do not animate,
      // so they have nothing to follow.
      if (live) followBottom();
    }
  }

  // Older records, on the front.
  //
  // **The reader's position is held by pixel offset, not by scroll top.** Prepending shifts
  // everything down by exactly the height of what was added, so a naive prepend teleports the view
  // and the reader loses the line they were on, which is the whole reason to backfill in place
  // rather than clear and reload. Measuring the scroll height on both sides and adding the
  // difference back keeps the same record under the same pixel.
  function prepend(events, starts, start) {
    if (!events.length) {
      // **An empty page is still an answer, and dropping it is a walk that never ends.** It says
      // there is nothing before what is on screen, so the page's own head is where the file begins.
      // Without this the head would go on claiming the offset the server has just reported holds
      // nothing, and `fill` would ask for the same region forever, once per frame, at a server
      // reading a 250 MB file.
      if (log.firstElementChild && typeof start === "number") {
        log.firstElementChild.dataset.start = String(start);
      }
      return;
    }

    var batch = document.createDocumentFragment();
    var previousDay = null;
    events.forEach(function (event, i) {
      var begins = starts && starts[i];
      var when = at(event.at);
      if (when) {
        var key = dayKey(when);
        if (key !== previousDay) {
          batch.appendChild(daybreak(when, begins));
          previousDay = key;
        }
      }
      batch.appendChild(line(event, false, begins));
    });

    var before = log.scrollHeight;
    log.insertBefore(batch, log.firstChild);
    log.scrollTop += log.scrollHeight - before;

    // The page below this batch already has a heading for its own first day, and the batch has just
    // ended on some day of its own. If they agree, that heading is now a repeat.
    var headings = log.querySelectorAll(".daybreak");
    for (var i = headings.length - 1; i > 0; i--) {
      // Removing it loses nothing now: every row carries its own offset, so the heading was not
      // holding the only copy of anything.
      if (headings[i].textContent === headings[i - 1].textContent) headings[i].remove();
    }

    // **`lastDay` is deliberately untouched.** It describes the page's TAIL, and a backfill goes on
    // the FRONT of a page whose bottom has not moved, so the day down there is still whatever it
    // was. Setting it from this batch would drop the heading at the next genuine day change.
    //
    // There is no longer a case where this batch IS the tail: that was the rebuild, which per-record
    // offsets removed. A widened feed growing a second daybreak the moment the next line arrived was
    // the bug that came of the two disagreeing, and the way it is gone for good is that the page is
    // never emptied and refilled at all.
    refreshFilterNote();
  }

  // --- THE THREE VIEW FILTERS ---------------------------------------------------------------------
  //
  // Each is a class on the feed and a stylesheet rule, so ticking a box costs one class change on
  // one element and no pass over the rows at all. The property that buys is not speed but
  // correctness for records that have not arrived yet: a row landing in ten minutes carries its own
  // markers and is hidden or shown by the same rule, with nothing having to remember what is ticked.
  //
  // **`hides` is the selector `puna.css` uses, spelled here too**, because the count below has to
  // describe the view actually on screen rather than a second opinion about it. Two spellings of one
  // rule is a thing this project has been bitten by, so `tests/templates.rs` holds the two files to
  // these exact strings.
  //
  // A missing input is not an error: the gameplay box is rendered only for a viewer the socket is
  // not already filtering, and the personal one only for somebody holding a slot in this room.
  var FILTERS = [
    {
      cls: "only-gameplay",
      input: "journal-filter-gameplay",
      hides: ".entry:not(.unfilterable):not(.gameplay)"
    },
    {
      cls: "only-personal",
      input: "journal-filter-personal",
      hides: ".entry:not(.unfilterable):not(.personal)"
    },
    {
      cls: "hide-filler",
      input: "journal-filter-filler",
      hides: ".entry:not(.unfilterable).filler"
    }
  ];

  // How many records a filter is currently keeping off the screen, said above the feed.
  //
  // **Without this a filter and a dead socket look identical.** A personal feed on a room whose
  // other twenty players are busy is a blank frame, and so is a connection that quietly stopped;
  // the reader has no way to tell which they are looking at, and the honest answer is cheap.
  //
  // "Loaded" is the load-bearing word. The page holds whatever window the reader has asked for, and
  // the trim counts rows rather than visible ones, so a filtered feed on a busy room is genuinely a
  // narrow view of that window rather than of the room's history. The `title` in the markup says so
  // at length; this says which numbers it is talking about.
  function refreshFilterNote() {
    if (!filterNote) return;
    var hides = FILTERS.filter(function (f) {
      return log.classList.contains(f.cls);
    }).map(function (f) {
      return f.hides;
    });
    // **The search counts as a filter here, which is the whole point of the number.** With a narrow
    // needle on a busy room the total climbs every second while the shown count sits still, and
    // that pair is the only thing on screen saying the feed is alive rather than stopped.
    if (needle !== "") hides.push(SEARCH_HIDES);
    if (!hides.length) {
      filterNote.textContent = "";
      return;
    }
    // Headings are not records and are never hidden, so neither number counts them.
    var total = log.querySelectorAll(".entry:not(.daybreak)").length;
    // One comma-joined query rather than one per filter: a row matching two of them must be counted
    // once, which `querySelectorAll` does for free and a sum of three lengths does not.
    var hidden = log.querySelectorAll(hides.join(",")).length;
    var shown = total - hidden;
    if (!hidden) {
      filterNote.textContent = "Showing all " + total + " loaded lines.";
    } else if (!shown) {
      filterNote.textContent =
        "Nothing loaded matches these filters (" + total + " lines hidden).";
    } else {
      filterNote.textContent = "Showing " + shown + " of " + total + " loaded lines.";
    }
  }

  // Read the boxes and put their classes on the feed, and say what is hidden.
  //
  // `filtering` carries none of the rules: it is what the stylesheet keys the alternating ground
  // off, because `:nth-child(even)` counts rows a filter has hidden and a filtered feed striped
  // that way comes out in runs of two and three, which reads as a rendering fault rather than as a
  // filter. **The search counts toward it too**, since a searched feed has the same gaps in it.
  // See the `.journal.filtering` rules.
  //
  // The search's own per-row class is not set here: it is set where a row is built and by
  // `refreshSearch` when the needle moves, because CSS cannot ask whether text contains a string.
  function applyFilters() {
    if (!filters) return;
    var any = needle !== "";
    FILTERS.forEach(function (f) {
      var box = document.getElementById(f.input);
      var on = !!(box && box.checked);
      log.classList.toggle(f.cls, on);
      if (on) any = true;
    });
    log.classList.toggle("filtering", any);
    refreshFilterNote();
  }

  if (filters) {
    // Revealed only now: without script these boxes would tick and do nothing, which is worse than
    // their absence. Same bargain `#journal-window` makes.
    filters.hidden = false;
    FILTERS.forEach(function (f) {
      var box = document.getElementById(f.input);
      if (!box) return;
      box.addEventListener("change", function () {
        // **Whether the reader is at the bottom is measured BEFORE the view changes.** Hiding rows
        // shortens the feed, which moves the bottom out from under them; asking afterwards would
        // read a reader who was following as one who had scrolled away, and leave them looking at
        // the middle of the history with no way to tell the feed had stopped moving.
        var wasFollowing = nearBottom();
        applyFilters();
        if (wasFollowing) pinBottom();
      });
    });

    if (filterSearch) {
      filterSearch.addEventListener("input", function () {
        // The same measure-first rule the boxes follow, and it matters more here: a needle is
        // narrowed a character at a time, so a reader watching the bottom of a live feed would be
        // dropped out of following on the first keystroke and have no idea why.
        var wasFollowing = nearBottom();
        setNeedle(filterSearch.value);
        refreshSearch();
        applyFilters();
        if (wasFollowing) pinBottom();
      });
      // **Whatever is already in the box, applied before anything is drawn.** The browser restores
      // a search box's value from session history on a reload, and restoring fires no `input`: the
      // exact bug `table.js` shipped on the room page, which came back as a populated box over an
      // unfiltered table. Read here so a needle that survived a reload is in force from the first
      // replayed row rather than from the reader's next keystroke.
      setNeedle(filterSearch.value);
    }

    // `toggles.js` has already restored each box from localStorage and dispatches no event of its
    // own, so this is what applies a remembered filter to the first paint. Without it the boxes
    // come up ticked over an unfiltered feed, which is the shape of every "my setting did not
    // persist" report.
    applyFilters();
  }

  function say(text, className) {
    // The MESSAGE span, not the whole paragraph: the dot is a sibling and `textContent` on the
    // parent would delete it. The first version of this wrote to `status` and the indicator
    // vanished on the first status change, which is to say, immediately and always.
    message.textContent = text;
    status.className = className || "notice";
  }

  // Green when the feed is attached, red when it is not. Purely decorative. The sentence beside it
  // carries the same state in words, and this element is `aria-hidden` for that reason.
  function setLink(up) {
    if (link) link.className = up ? "link-state up" : "link-state down";
  }

  // --- THE DOT AND THE SENTENCE ARE ONE FACT ------------------------------------------------------
  // Every connection-state change goes through these two, never through `say` alone. The watchdog
  // originally announced "Lost contact…" with a plain `say`, which left a GREEN dot beside it until
  // the close event eventually arrived, the indicator contradicting the words it was put there to
  // reinforce, which is worse than having neither.
  //
  // Routing both through one call makes that unspellable rather than merely fixed.
  function linkUp(text) {
    live = true;
    setLink(true);
    say(text, "notice");
  }

  function linkDown(text) {
    live = false;
    setLink(false);
    say(text, "warning");
  }

  // Called on EVERY frame, whatever it carries. Any traffic at all proves the link, so a busy room
  // never runs this timer down and a silent one is carried by the heartbeat alone.
  function heard() {
    if (aliveTimer !== null) clearTimeout(aliveTimer);
    if (!aliveAfter) return;
    aliveTimer = setTimeout(function () {
      aliveTimer = null;
      // **Abandon it, do not merely close it.** `close()` starts a CLOSING HANDSHAKE. It sends a
      // Close frame and waits for the peer's reply, and on the black hole that got us here that
      // reply never comes, so the browser waits out its own timeout before firing `close`. Handing
      // the redial to that event is how "Lost contact…" sat on screen for a long moment before
      // anything else happened.
      //
      // So the socket is disowned here: `epoch` moves, which makes every event still to come from
      // it inert, and the redial is scheduled directly. `close()` is still called so the browser
      // tears down what it can, and whatever it does afterwards is no longer this page's business.
      var dead = socket;
      epoch++;
      socket = null;
      if (dead) {
        try {
          dead.close();
        } catch (e) {
          // A socket that was already failing is allowed to fail again; it is being discarded.
        }
      }
      scheduleReconnect("Lost contact with the room's feed. Reconnecting…");
    }, aliveAfter);
  }

  function stopWatchdog() {
    if (aliveTimer !== null) {
      clearTimeout(aliveTimer);
      aliveTimer = null;
    }
  }

  // **The fact that a feed is filtered belongs in the status line, once, not in the feed.**
  //
  // The server reports a `withheld` count per frame, and rendering it as a row put a timestamp-less
  // line into a stream where every other line is an event at an instant, so it read as something
  // having happened, scattered through the history once per batch, saying "1 record" each time.
  // That is metadata about a delivery, and a delivery is a network artifact the reader should never
  // see the seams of.
  //
  // It is still said, because a reader is owed the knowledge that they are looking at part of a
  // history rather than all of it, and it is said as **what this feed is** rather than as what is
  // missing from it. "(items and links only)" has similar phrasing as the room's own setting,
  // so a reader who goes looking for why sees the words they were shown; a count would describe
  // whatever happened to be fetched rather than anything about the room.
  var live = false;
  var filtered = false;

  function sayLive() {
    linkUp(
      filtered
        ? "Live: following this room's feed (gameplay only)."
        : "Live: following this room's feed."
    );
  }

  function noteFiltering(count) {
    if (!count || filtered) return;
    filtered = true;
    if (live) sayLive();
  }

  // How many records to ask for on connect.
  //
  // The server clamps to `REPLAY_MAX` whatever it is told, so asking for the cap outright would
  // work; asking for what it will actually serve is what keeps the page's own arithmetic about how
  // much is still missing honest.
  function firstPage() {
    return cap === Infinity ? REPLAY_MAX : Math.min(cap, REPLAY_MAX);
  }

  // Ask for whatever the window is short of, walking backwards from the page's oldest line.
  //
  // **One request in flight at a time.** The alternative, firing every page at once, would put a
  // thousand backwards seeks on a 250 MB file at a server that is also following it, to fill a DOM
  // the reader cannot scroll through anyway. The walk continues itself from the frame handler and
  // stops here, by asking for nothing once the window is full: the cap is what ends it, rather than
  // a flag somebody has to remember to clear.
  //
  // It asks for what it is short of rather than for a full page, because a reader who widened from
  // 500 to 1,000 needs five hundred records and a full page is five thousand: nine tenths of it on
  // an uncompressed socket to be trimmed off the top on arrival.
  // Whether the page is holding less than the reader asked for. The walk's condition, and the note's:
  // it is what makes narrowing mid-walk a cancellation rather than a request that has to be tracked
  // and revoked.
  function short() {
    return cap === Infinity || log.childElementCount < cap;
  }

  function fill() {
    if (backfilling || !socket || socket.readyState !== WebSocket.OPEN) return;
    if (!short()) return;
    var want = cap === Infinity ? REPLAY_MAX : cap - log.childElementCount;
    // `null` is a page with nothing on it yet, in which case the replay is on its way and will
    // anchor the page itself; `0` is a page holding the beginning of the file, and there is nothing
    // earlier than that. Neither is a state to recover from: a page with rows on it always knows
    // where it begins, because every row says.
    var start = pageStart();
    if (start === null || start === 0) return;
    backfilling = true;
    socket.send(JSON.stringify({ before: start, lines: Math.min(want, REPLAY_MAX) }));
  }

  // What the walk is doing, in words, beside the control that started it.
  //
  // **A bare spinner would not do, and the reason is on the record.** A whole-feed load on a busy
  // room is dozens of round trips over tens of seconds, and a note that says only "loading" for all
  // of them is indistinguishable from one that has stopped: exactly the confusion a silent stop
  // after one page produced when this walk last went wrong.
  //
  // Nothing is said while the window is simply being held. The buttons already say how much that
  // is, and a line restating it under them would be noise on every page load.
  function setProgress() {
    if (!progress) return;
    // **`short()` and not `backfilling`, so a cancelled walk stops saying it is loading at once.**
    // A page already asked for is on its way and cannot be unsent, but nothing more is being
    // fetched for this reader, and a note going on counting through a narrowing they just asked for
    // is the page describing a request rather than what it is doing.
    if (backfilling && short()) {
      progress.textContent = backfilled
        ? "Loading earlier records… " + backfilled + " so far."
        : "Loading earlier records…";
      return;
    }
    progress.textContent =
      pageStart() === 0 ? "The whole feed is loaded: " + records() + " lines." : "";
  }

  // --- THE WINDOW CONTROL -------------------------------------------------------------------------

  // The remembered size, or the default.
  //
  // Anything the store holds that is not one of the offered sizes is ignored rather than honored:
  // it is a string out of a store any script on this origin can write, and these six are the only
  // values this page has been reasoned about at. `Infinity` is refused along with the rest, which
  // is what makes "never persisted" a property of the reader rather than only of the writer.
  function storedWindow() {
    var saved = window.PunaToggles && window.PunaToggles.recall(WINDOW_KEY);
    var size = Number(saved);
    return size !== Infinity && WINDOW_SIZES.indexOf(size) >= 0 ? size : DEFAULT_WINDOW;
  }

  function windowValue(button) {
    return button.dataset.lines === "all" ? Infinity : Number(button.dataset.lines);
  }

  // **The current size is the disabled button**, which is Troy's call and reads correctly: there is
  // nothing to press, because the page is already showing that much. The stylesheet draws it as
  // selected rather than as unavailable, since those are opposite meanings for one attribute.
  function markWindow() {
    if (!windowRow) return;
    windowRow.querySelectorAll("button[data-lines]").forEach(function (button) {
      var current = windowValue(button) === cap;
      button.disabled = current;
      // `disabled` takes a button out of the tab order, so on its own the current size is a control
      // a screen reader cannot reach and nothing else announces. This is what says which one it is.
      if (current) button.setAttribute("aria-current", "true");
      else button.removeAttribute("aria-current");
    });
  }

  function setWindow(size) {
    if (size === cap) return;
    var narrowing = size < cap;
    cap = size;
    markWindow();
    if (window.PunaToggles && size !== Infinity) {
      window.PunaToggles.remember(WINDOW_KEY, String(size));
    }
    if (narrowing) {
      // **Measured before the view changes**, for the reason the filter boxes measure it before
      // theirs: trimming moves the bottom out from under the reader, and asking afterwards reads
      // somebody who was following as somebody who had scrolled away.
      var wasFollowing = nearBottom() || (following && !readerMoved());
      trimToCap();
      if (wasFollowing) pinBottom();
      refreshFilterNote();
    } else {
      fill();
    }
    setProgress();
  }

  if (windowRow) {
    cap = storedWindow();
    markWindow();
    // Revealed only now, for the reason the filter row is: a control that can do nothing without
    // script is worse than an absent one. Before any frame has landed rather than after, so the
    // height it takes out of the feed is already gone by the time the first replay pins the bottom.
    windowRow.hidden = false;
    windowRow.querySelectorAll("button[data-lines]").forEach(function (button) {
      button.addEventListener("click", function () {
        setWindow(windowValue(button));
      });
    });
  }

  function open() {
    // Captured by every handler below. Once it has moved on, this socket is somebody else's
    // history. See the watchdog.
    var mine = ++epoch;
    var scheme = location.protocol === "https:" ? "wss:" : "ws:";
    // Held locally as well as on the module, so a handler acts on ITS OWN socket rather than on
    // whatever happens to be current when it runs.
    var sock = new WebSocket(
      scheme + "//" + location.host + "/journal/" + feed + "/feed"
    );
    socket = sock;

    sock.addEventListener("open", function () {
      if (mine !== epoch) return;
      // The server waits for this before replaying, so it decides where the page starts.
      //
      // **A reconnect resumes at the cursor; a first connect asks for a tail.** Sending the tail
      // both times is what duplicated the feed across a rollout: the page keeps its lines, the
      // server replayed the last hundred records, and `append` has no reason to think it has seen
      // them. Confirmed happening on a re-rollout, and it reads as a busy room rather than as a
      // fault, which is why it survived.
      //
      // `at` is accepted here too, for the day this page offers a time to scroll back to. It is
      // deliberately not what a resume uses: several records routinely share a timestamp and
      // `since` is inclusive, so it would re-send the ties.
      resumed = cursor !== null;
      cursorAsked = cursor;
      socket.send(
        JSON.stringify(
          resumed ? { from: { after: cursor } } : { from: { lines: firstPage() } }
        )
      );
    });

    sock.addEventListener("message", function (message) {
      if (mine !== epoch) return;
      // **Before the parse, and before any `kind` is looked at.** A frame arriving is the proof
      // the link is alive whatever it says, and a frame this build cannot read is still a frame
      // that crossed the wire. Restarting the watchdog only for messages we understood would let a
      // newer server's unfamiliar frame look exactly like silence.
      heard();

      var frame;
      try {
        frame = JSON.parse(message.data);
      } catch (e) {
        return;
      }
      // Carries nothing and is not meant to: its whole job is to arrive, so `heard` above has
      // something to hear on a room where nobody is playing.
      if (frame.kind === "heartbeat") return;
      if (frame.kind === "empty") {
        say("This room has no feed history yet. It is written while the room runs.", "notice");
        return;
      }
      if (typeof frame.cursor === "number") cursor = frame.cursor;
      // Where each of this frame's records begins, one per event and in the same order. Every kind
      // of frame carries them, so nothing here has to infer an offset from a cursor or from what
      // some previous frame said. See `pageStart`.
      var starts = Array.isArray(frame.starts) ? frame.starts : null;
      // The cadence comes from the server, on the opening frame. Until it arrives the watchdog is
      // disarmed rather than guessing. A guess shorter than the real interval would tear down a
      // healthy connection on a timer, which is worse than the gap it was meant to close.
      if (typeof frame.heartbeat_ms === "number" && frame.heartbeat_ms > 0) {
        aliveAfter = frame.heartbeat_ms * HEARTBEAT_MISSES;
        heard();
      }

      // A backfill page goes on the front and never touches the follow cursor.
      if (frame.kind === "earlier") {
        prepend(
          frame.events || [],
          starts,
          typeof frame.start === "number" ? frame.start : 0
        );
        // **The window may have moved while this page was in flight.** Narrowing cancels the walk,
        // but it cannot unsend the request already on the wire, so the answer arrives for a window
        // nobody wants any more: five thousand rows onto a page the reader has just cut to five
        // hundred. Trimmed here rather than left for the next live record, which on a quiet room is
        // never, and which would leave the control saying 500 over a page holding thousands.
        //
        // It also settles the ordinary overshoot: a page is asked for in records and lands with a
        // day heading or two besides, which are rows like any other.
        trimToCap();
        noteFiltering(frame.withheld);
        backfilled += (frame.events || []).length;
        // **Cleared before the next ask, not in the arm that ends the walk.** This request is
        // finished. Its page is on the screen, so `fill`'s in-flight guard is about the
        // *next* one. Leaving the flag set until the walk ended made that guard reject every
        // continuation, so the whole-feed button loaded one page and stopped: button disabled,
        // note frozen mid-sentence, nothing thrown, 5,000 records of 160,000 on the page. A silent
        // stop is the worst shape this could fail in, because it looks exactly like a short file.
        backfilling = false;
        // Keep walking while the window is short of what was asked for, and stop by asking for
        // nothing once it is full. One page in flight at a time, so a slow disk backs the walk up
        // rather than queueing a thousand requests at a server reading a 250 MB file.
        fill();
        setProgress();
        return;
      }

      // **A resume the server could not stitch.** It answers a fresh tail instead. The file was
      // reset under us, or the gap was larger than one frame may carry, and says so by reporting a
      // `start` other than the offset asked for. Appending that onto what is already here would put
      // a hole in the middle of the feed with nothing marking it, so the page starts over.
      var restarted = false;
      if (frame.kind === "replay" && resumed && frame.start !== cursorAsked) {
        log.replaceChildren();
        lastDay = null;
        backfilled = 0;
        restarted = true;
        resumed = false;
      }

      // `append` is the live frame and `replay` is the tail on connect; only the first is new to
      // whoever is watching, so only the first opens a row. A BURST does not either: see ARRIVE_MAX.
      var events = frame.events || [];
      append(events, frame.kind === "append" && events.length <= ARRIVE_MAX, starts);
      noteFiltering(frame.withheld);
      if (frame.kind === "replay") {
        // The backoff resets on a connection that got as far as a replay, not on one that merely
        // opened: a server that accepts and drops immediately should not be redialled twice a
        // second.
        retry = RETRY_MIN;

        // **Said here rather than where the page was cleared**, because the branch below announces
        // the connection unconditionally and would have overwritten it in the same tick. The note
        // was set and then replaced before a frame was ever painted, so nobody could have read it.
        // One place decides what this line says once the socket is up.
        if (restarted) {
          linkUp("Reconnected. Tail restarted.");
        } else {
          sayLive();
        }

        // **A resume leaves the page's anchors alone**, because it leaves the page alone: its
        // oldest line is still whatever it was, and `append` has already decided whether to follow
        // the bottom by whether the reader was sitting there. Re-anchoring here would point the
        // backfill walk at the middle of what is on screen, and scrolling would yank a reader who
        // came back to find their place.
        if (resumed) {
          resumed = false;
          return;
        }

        // **The page is re-anchored by the replay itself**, on every replay that REPLACES it: a
        // first connect, or a resume the server could not stitch, both of which start from a tail.
        // Its oldest line is whatever that replay began with, and `append` has already stamped it
        // with the `start` the frame carried, so nothing here has to carry an offset across.
        backfilling = false;
        // The window is capped at `REPLAY_MAX` on the wire, so anything larger than that arrives in
        // pieces: this is what asks for the rest, on a first connect exactly as on a cap change.
        fill();
        setProgress();

        // **Pinned LAST, after the progress note has been written.** The note sits above the feed,
        // which is the flex item holding this page's slack (see `.feed-page`), so a line appearing
        // there takes its own height out of the feed and moves the bottom the pin just wrote. That
        // is the shape of a reported bug: with the reveal ordered the other way the view ended up
        // one line short of the bottom on every first load, close enough to the 40-pixel tolerance
        // to be a coin toss about whether the page then considered itself to be following at all.
        //
        // Through `pinBottom` like every other pin, so the position the page believes it wrote is
        // the one it actually wrote. A raw assignment here would leave a stale one behind for the
        // first live row to misread.
        pinBottom();
      }
    });

    sock.addEventListener("close", function () {
      if (mine !== epoch) return;
      // Or it fires against a socket that is already gone and closes the next one.
      stopWatchdog();
      scheduleReconnect();
    });

    // `close` fires after `error`, so the reconnect is scheduled there and not twice.
    //
    // **Closes `mine`, never the module's `socket`.** Written against the shared variable, an error
    // arriving late from a socket the watchdog had already disowned would have closed whatever is
    // connected NOW, a healthy feed torn down by the failure of its predecessor, and a redial loop
    // if the pattern repeated. The epoch guard makes it moot and the local reference makes it
    // impossible; both, because this is the arm nobody watches.
    sock.addEventListener("error", function () {
      if (mine !== epoch) return;
      if (sock && sock.readyState === WebSocket.OPEN) sock.close();
    });
  }

  // Whether a socket is up or on its way up. Guards every path that might open a second one. The
  // visibility handler and the close handler can both decide to redial, and two sockets would both
  // replay and both follow.
  function attached() {
    return (
      socket &&
      (socket.readyState === WebSocket.OPEN ||
        socket.readyState === WebSocket.CONNECTING)
    );
  }

  // The **Page Visibility API**: `document.visibilityState`, which is `"visible"` or `"hidden"`,
  // with `visibilitychange` fired on every transition. It is the right question rather than
  // `window.onfocus`: a tab sitting in view beside another window is `visible` and unfocused, and
  // that reader is watching the feed.
  function showing() {
    return document.visibilityState === "visible";
  }

  // **A hidden tab does not redial.** Nobody is reading it, a room can be down for an hour, and a
  // laptop that slept with twenty of these open should not wake into twenty reconnect storms. The
  // dial resumes the moment the tab is looked at.
  //
  // An OPEN socket in a hidden tab is left alone, which is the older rule and still right: the
  // server pings, the traffic is a trickle, and dropping it would cost a reconnect every time
  // somebody switched tabs.
  function scheduleReconnect(reason) {
    if (reconnectTimer !== null || attached()) return;
    if (!showing()) {
      linkDown("Not connected. Will reconnect when you come back to this tab.");
      return;
    }
    linkDown(reason || "Reconnecting to the room's feed…");
    var wait = retry / 2 + Math.random() * (retry / 2);
    retry = Math.min(retry * 2, RETRY_MAX);
    reconnectTimer = setTimeout(function () {
      reconnectTimer = null;
      open();
    }, wait);
  }

  document.addEventListener("visibilitychange", function () {
    if (!showing()) {
      // Cancel a redial that has not fired. Without this a tab hidden mid-backoff still reconnects
      // once, which is the case the rule exists for: a browser waking a hundred background tabs.
      if (reconnectTimer !== null) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      if (!attached()) {
        linkDown("Not connected. Will reconnect when you come back to this tab.");
      }
      return;
    }
    if (attached()) return;
    // **Immediately, and from a clean backoff.** The wait that had built up was measuring a server
    // that would not answer; coming back to the tab is new information about this reader rather
    // than about the server, and making somebody stare at a red dot for the remains of a 30-second
    // timer is the thing they came back to avoid.
    retry = RETRY_MIN;
    open();
  });

  // Deliberately NOT painted red here. The stylesheet's bare `.link-state` is muted, which is the
  // honest third state: the page has not tried yet, and the server-rendered sentence beside it says
  // "Connecting…". Opening on red would report a failure that has not happened.
  open();
})();
