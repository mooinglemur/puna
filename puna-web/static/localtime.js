// Absolute timestamps, in the reader's own timezone, behind the shorthand ages.
//
// Every duration on this site is rendered short and relative -- "40m", "6d 2h", "3h ago" -- because
// that is what answers the question being asked while scanning a table. What it cannot answer is
// *which* moment, and that is the question somebody has once they have found the row they care
// about: correlating with a log line, a Discord message, or another operator's account of events.
//
// So the shorthand stays and the exact instant goes in a `title`.
//
// ## Why the browser and not the server
//
// The server has the instant and does not have the reader. `2026-08-22 10:53:40 -0700` is only
// useful in the timezone the reader is actually in, and a page cached or shared between two people
// in different zones must render differently for each. That is a client concern by construction.
//
// ## The convention
//
// Any element carrying `data-at="<epoch milliseconds>"` gets a `title`. The server emits it beside
// the shorthand it already renders; `tracker.js` sets it on the cells it builds itself, calling
// `absolute()` directly rather than going through the sweep.
//
// Elements are stamped ONCE. The instant does not change -- only the shorthand does, as time passes
// -- so there is nothing to keep current.
(function () {
  "use strict";

  function pad(n) {
    return String(n).padStart(2, "0");
  }

  // `+0200` / `-0700`. The fallback, and the one thing that always works.
  function numericOffset(date) {
    // `getTimezoneOffset` is minutes to ADD to local to reach UTC, so it is the opposite sign from
    // the one printed in a timestamp: UTC+2 reports -120.
    var minutes = -date.getTimezoneOffset();
    var sign = minutes < 0 ? "-" : "+";
    minutes = Math.abs(minutes);
    return sign + pad(Math.floor(minutes / 60)) + pad(minutes % 60);
  }

  // Whatever the reader's own locale and zone call it: `MST`, `CEST`, `BST`, `AEST`, `UTC` where
  // there is an abbreviation, and `UTC+2` or `GMT+1` where the locale has none.
  //
  // **Both forms are surfaced as they come.** An earlier version rejected anything containing a
  // digit and substituted `+0200`, on the theory that `UTC+2` is an offset wearing a name. It is --
  // and it is the platform's own answer for that reader, which is more use to them than a
  // four-digit form their locale never shows them. Whether a zone has a short name is a property of
  // where they are, not something worth normalizing away.
  //
  // `numericOffset` remains for the case where there is no answer at all: an environment without
  // `Intl`, or a format with no zone part.
  function zoneName(date) {
    try {
      var parts = new Intl.DateTimeFormat(undefined, {
        timeZoneName: "short",
      }).formatToParts(date);
      for (var i = 0; i < parts.length; i++) {
        if (parts[i].type === "timeZoneName" && parts[i].value) {
          return parts[i].value;
        }
      }
    } catch (e) {
      // Intl is unavailable or the locale has no such format. The offset says the same thing.
    }
    return numericOffset(date);
  }

  // `2026-08-22 10:53:40 MST`
  //
  // Deliberately not `toLocaleString`: this is a timestamp somebody pastes beside a log line, so it
  // wants a fixed, sortable, unambiguous shape rather than one that reorders the fields by locale.
  // Only the ZONE is localized, because that is the part the reader cannot infer.
  function absolute(ms) {
    var d = new Date(ms);
    if (isNaN(d.getTime())) return "";
    return (
      d.getFullYear() +
      "-" +
      pad(d.getMonth() + 1) +
      "-" +
      pad(d.getDate()) +
      " " +
      pad(d.getHours()) +
      ":" +
      pad(d.getMinutes()) +
      ":" +
      pad(d.getSeconds()) +
      " " +
      zoneName(d)
    );
  }

  // Stamp every `[data-at]` under `root` that is not stamped already.
  //
  // Exposed because not every such element is in the document at load: `/admin/rooms` fetches its
  // stopped-and-closed table when the section is opened, and `room.js` replaces the whole lifecycle
  // panel on every state change. `data-at-done` is the idempotence -- re-stamping is harmless but
  // walking the same rows on every swap is not free on a long table.
  function stamp(root) {
    (root || document).querySelectorAll("[data-at]").forEach(function (el) {
      if (el.dataset.atDone) return;
      var when = absolute(Number(el.dataset.at));
      if (!when) return;
      // **Not overwritten if something already explains this cell.** A few carry a `title` saying
      // what the column means, and that sentence is worth more than a timestamp.
      if (!el.title) el.title = when;
      el.dataset.atDone = "1";
    });
  }

  window.PunaTime = { absolute: absolute, stamp: stamp };
  stamp(document);
})();
