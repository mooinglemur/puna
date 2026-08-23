// The tracker's tables, rendered in the browser.
//
// Puna digests the room's documents server-side (`/api/puna/tracker/<id>/<view>`) and this fetches
// the result. The reason is bandwidth first -- a room's live document is measured at 2.7 MB for 185
// slots and almost none of it is what a table shows -- and capability second: the multiworld view
// is never sent another slot's raw data, because no endpoint would serve it.
//
// What the browser buys with that: a search box over every table and sortable columns, neither of
// which the reference implementation can offer, because it renders static HTML on the server.
//
// No framework, no build step. This file is served as it is written.

(() => {
  "use strict";

  const root = document.getElementById("tracker");
  if (!root) return;

  const api = root.dataset.api;
  // Present on a slot's page. It rides in the query string because the per-slot views resolve their
  // scope from the tracker *id*, and the reference-compatible URL carries the room's id, not the
  // slot's.
  const slotQuery = root.dataset.slot ? `?slot=${encodeURIComponent(root.dataset.slot)}` : "";

  // **Which tracker this is**, and it namespaces every remembered preference. The hints table is on
  // both pages and its toggle is a different question on each -- "what am I still waiting for"
  // against "what is outstanding anywhere" -- so sharing one key would make choosing on one page
  // silently change the other.
  const pageType = root.dataset.slot ? "slot" : "room";

  // Declared up here because `age()` reads it and is called from the first render. The polling loop
  // that maintains it is at the bottom of this file.
  let intervalMs = 60000;
  let lastPollAt = 0;
  let lastResponseAt = Date.now();

  // --- how each view's rows become cells --------------------------------------------------------
  //
  // One entry per table. `cells` returns an array of either a string or a {text, class, href}, in
  // the same order as the server-rendered <th>s -- which is what keeps the header and the body from
  // drifting apart without a template engine to tie them together.

  const dash = { text: "—", class: "hint" };

  const VIEWS = {
    slots: {
      rows: (d) => d.slots,
      cells: (r) => [
        String(r.slot),
        r.claimed ? r.name : { text: r.name, tag: "unclaimed" },
        r.spectator ? { text: r.game, tag: "spectator" } : r.game,
        r.spectator
          ? dash
          : `${r.checks_done} / ${r.checks_total}${percent(r)}`,
        r.spectator ? dash : r.status,
        r.spectator ? dash : String(r.hints),
        age(r.last_activity_ms_ago),
      ],
      // **Checks sort by COMPLETION, not by count**, which is what the column means: 400/2000 is
      // behind 12/12 however the raw numbers compare, and sorting on the count puts the biggest
      // world first and calls it the furthest along. The header keeps `data-key="checks_done"`
      // because that is the column's identity — and because a remembered sort or a shared link
      // already carries that key, so renaming it would silently leave old links on the old
      // behavior rather than failing visibly.
      //
      // A slot with nothing to check has no answer, so it is `null` and sorts last in BOTH
      // directions — the same rule `last seen` follows for a slot that has never acted.
      sortValues: {
        checks_done: (r) => (r.checks_total ? r.checks_done / r.checks_total : null),
      },
      // The footer, computed from the rows CURRENTLY DISPLAYED rather than from the server's
      // `totals`. With no filter the two agree exactly; with one, this describes the table it sits
      // beneath — which is the property that cannot be wrong. A summary contradicting the rows
      // above it is worse than no summary, and "how far along is everyone playing this game" is a
      // question worth being able to ask.
      //
      // Spectators are out of the goal denominator: they cannot goal, so counting them would make
      // a finished multiworld read as permanently short.
      summary: (rows) => {
        const done = sum(rows, "checks_done");
        const total = sum(rows, "checks_total");
        const players = rows.filter((r) => !r.spectator);
        const goaled = players.filter((r) => r.status === "goal").length;
        return {
          checks: `${done} / ${total}${percent({ checks_done: done, checks_total: total })}`,
          goals: `${goaled} / ${players.length} goaled`,
          hints: String(sum(rows, "hints")),
          // Through `age` like every other cell in this column, so it carries the same shorthand,
          // the same absolute-time tooltip, and the same "never" when nobody has acted.
          seen: age(mostRecent(rows)),
        };
      },
      // Only on the multiworld page, and built from the id already in this URL rather than from
      // anything the server sent: a slot's own tracker id is deliberately never in the JSON.
      href: (r) => (slotQuery ? null : `/tracker/${idFromApi()}/0/${r.slot}`),
    },

    locations: {
      rows: (d) => d.locations,
      // Hide what is done, leaving what is left. A predicate on the view rather than a branch in
      // `Table`, for the same reason `collapse` is one: what "done" means belongs to the view.
      exclude: (r) => r.checked,
      cells: (r) => [r.name, r.checked ? "✔" : ""],
      rowClass: (r) => (r.checked ? "done" : null),
    },

    items: {
      rows: (d) => d.items,
      // **One row per item name, keeping the most recent.** Declared here rather than built into
      // `Table` because it is a property of what this view holds: an item list is the only one
      // where the same thing legitimately appears many times, and where "how many" is a fact the
      // reader wants rather than noise.
      collapse: { key: "item", recency: "order" },
      cells: (r) => [
        String(r.order),
        {
          text: r.item,
          // Between the name and the class chip, quiet: it qualifies the name rather than
          // categorizing it. Absent entirely at one, because "(x1)" is noise on every other row.
          note: r.count > 1 ? `(x${r.count})` : null,
          tag: r.classification === "filler" ? null : r.classification,
        },
        r.from_name,
        r.location,
      ],
    },

    hints: {
      rows: (d) => d.hints,
      exclude: (r) => r.found,
      cells: (r) => [
        r.receiving_name,
        { text: r.item, tag: r.classification === "filler" ? null : r.classification },
        r.finding_name,
        r.location,
        r.entrance || dash,
        r.status,
        r.found ? "✔" : "",
      ],
      rowClass: (r) => (r.found ? "done" : null),
    },
  };

  function percent(r) {
    if (!r.checks_total) return "";
    const pct = Math.min(100, Math.round((r.checks_done * 100) / r.checks_total));
    return ` (${pct}%)`;
  }

  // The freshest activity among `rows`, as an age — so the **smallest** number, not the largest.
  //
  // **`null` is never, and is excluded rather than compared.** Treating a slot that has never acted
  // as `0` would make it the most recent thing in the multiworld and pin the total at "just now"
  // forever, which is the same 1970 mistake in the other direction. All-null answers `null`, which
  // `age` renders as "never".
  //
  // `reduce` rather than `Math.min(...ages)`: a 2000-slot room would spread 2000 arguments onto the
  // stack for no reason.
  function mostRecent(rows) {
    const ages = rows
      .map((row) => row.last_activity_ms_ago)
      .filter((ms) => ms !== null && ms !== undefined);
    return ages.length ? ages.reduce((a, b) => (b < a ? b : a)) : null;
  }

  // `|| 0` rather than assuming the field is there: a spectator carries no meaningful check or hint
  // count, and a missing one must not turn the whole total into `NaN`.
  function sum(rows, key) {
    return rows.reduce((running, row) => running + (row[key] || 0), 0);
  }

  // `null` is NEVER, and never is not 1970 -- rendering an epoch date is the classic way to make an
  // untouched slot look like an abandoned one. The server sends an age it computed, so a skewed
  // client clock cannot produce a negative one; this adds the time since that response arrived, so
  // the column keeps ticking between polls without a fetch.
  function age(msAgo) {
    if (msAgo === null || msAgo === undefined) return { text: "never", class: "hint" };
    const minutes = Math.floor((msAgo + (Date.now() - lastResponseAt)) / 60000);
    const text =
      minutes < 1
        ? "just now"
        : minutes < 60
          ? `${minutes}m ago`
          : minutes < 2880
            ? `${Math.floor(minutes / 60)}h ago`
            : `${Math.floor(minutes / 1440)}d ago`;

    // **The exact moment, behind the shorthand.** `lastResponseAt` is when this document arrived
    // and `msAgo` is how old the event was THEN, so their difference is the instant itself -- and
    // it does not drift as the page sits open, unlike the age above it.
    //
    // Computed here rather than swept afterwards because these cells are rebuilt on every render;
    // a sweep would have to re-walk the table each time and would race the next one.
    const title = window.PunaTime
      ? window.PunaTime.absolute(lastResponseAt - msAgo)
      : undefined;
    return title ? { text, title } : text;
  }

  function idFromApi() {
    const parts = api.split("/");
    return parts[parts.length - 1];
  }

  // --- state, persisted in the fragment ----------------------------------------------------------
  //
  // The fragment rather than storage so a sorted, filtered view is a URL somebody can share or
  // reload into. Nothing here is secret: it is a column name and whatever was typed in a box.

  function readState() {
    return new URLSearchParams(location.hash.slice(1));
  }

  function writeState(params) {
    const next = params.toString();
    // `replaceState`, not `location.hash =`: typing in a filter box should not fill the back button
    // with one entry per keystroke.
    history.replaceState(null, "", next ? `#${next}` : location.pathname + location.search);
  }

  // --- one table -------------------------------------------------------------------------------

  class Table {
    constructor(section) {
      this.section = section;
      this.view = section.dataset.view;
      this.config = VIEWS[this.view];
      this.tbody = section.querySelector("tbody");
      // Absent on a slot's page, where a summary of one row would be noise. `renderSummary` guards
      // on that rather than the template and the script each knowing which pages have one.
      this.tfoot = section.querySelector("tfoot");
      this.empty = section.querySelector(".empty");
      this.search = section.querySelector(".table-search");
      this.toggle = section.querySelector("[data-toggle]");
      // Read AFTER `toggles.js` has restored it -- both files are `defer`, so they run in document
      // order and the box is already in its remembered state by the time this asks.
      this.toggled = !!(this.toggle && this.toggle.checked);
      // Namespaced by page type, so the hints table's sort on a slot page is not the multiworld's.
      this.sortKey = `tracker.${pageType}.${this.view}.sort`;
      this.headers = Array.from(section.querySelectorAll("th[data-key]"));
      this.details = section.querySelector("details");
      this.rows = [];

      const state = readState();
      this.query = state.get(`${this.view}.q`) || "";
      // **The fragment wins where it has an opinion**, because that is what a shared link carries --
      // somebody sending "look at this sorted by checks" must not have it overridden by whatever
      // the recipient last chose. Absent one, the remembered sort applies, which is what makes a
      // fresh visit pick up where the reader left off rather than at the server's order.
      this.sort = parseSort(
        state.get(`${this.view}.sort`) ||
          (window.PunaToggles ? window.PunaToggles.recall(this.sortKey) : "")
      );
      if (this.search) this.search.value = this.query;
      if (this.details && state.get(`${this.view}.open`) === "1") this.details.open = true;

      this.bind();
      this.markHeaders();
    }

    bind() {
      if (this.toggle) {
        // `toggles.js` owns persisting it; this only reacts. Two listeners on one input rather than
        // a callback threaded through, so neither file has to know the other's shape.
        this.toggle.addEventListener("change", () => {
          this.toggled = this.toggle.checked;
          this.render();
        });
      }

      if (this.search) {
        this.search.addEventListener("input", () => {
          this.query = this.search.value;
          this.persist();
          this.render();
        });
      }

      for (const th of this.headers) {
        th.tabIndex = 0;
        th.setAttribute("role", "button");
        const activate = () => this.toggleSort(th.dataset.key);
        th.addEventListener("click", activate);
        th.addEventListener("keydown", (e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            activate();
          }
        });
      }

      if (this.details) {
        this.details.addEventListener("toggle", () => {
          this.persist();
          // Not fetched until opened: this is the one table whose size scales with the whole
          // multiworld rather than with one slot.
          if (this.details.open && !this.rows.length) this.refresh();
        });
      }
    }

    toggleSort(key) {
      if (this.sort && this.sort.key === key) {
        this.sort = this.sort.dir === "asc" ? { key, dir: "desc" } : null;
      } else {
        this.sort = { key, dir: "asc" };
      }
      this.persist();
      this.markHeaders();
      this.render();
    }

    markHeaders() {
      for (const th of this.headers) {
        const active = this.sort && this.sort.key === th.dataset.key;
        th.dataset.sort = active ? this.sort.dir : "";
        th.setAttribute(
          "aria-sort",
          active ? (this.sort.dir === "asc" ? "ascending" : "descending") : "none"
        );
      }
    }

    persist() {
      const params = readState();
      setOrDelete(params, `${this.view}.q`, this.query);
      const sort = this.sort ? `${this.sort.key}:${this.sort.dir}` : "";
      setOrDelete(params, `${this.view}.sort`, sort);
      // Both: the fragment so the view can be shared, the store so it survives arriving without one.
      // Only the SORT is remembered -- a search box that refilled itself on every visit would hide
      // rows for a reason the reader has long forgotten typing, and unlike a sort that is not
      // visible at a glance.
      if (window.PunaToggles) window.PunaToggles.remember(this.sortKey, sort);
      if (this.details) setOrDelete(params, `${this.view}.open`, this.details.open ? "1" : "");
      writeState(params);
    }

    // Skipped entirely while a collapsed <details> hides it.
    wanted() {
      return !this.details || this.details.open;
    }

    async refresh() {
      if (!this.wanted()) return null;
      const response = await fetch(`${api}/${this.view}${slotQuery}`, {
        headers: { Accept: "application/json" },
      });
      // A per-slot view on a multiworld page answers 404 by design; so does a tracker whose policy
      // was changed under an open tab. Neither is worth blanking a table that is already useful.
      if (!response.ok) return null;

      const document_ = await response.json();
      this.rows = this.config.rows(document_) || [];
      this.render();
      return document_;
    }

    render() {
      const needle = this.query.trim().toLowerCase();
      // **Before filtering, deliberately.** Collapse then filter answers "the most recent of each
      // item, among those matching"; filter then collapse would answer "the most recent MATCHING
      // instance", which for a search that excludes the newest one shows an older row as though it
      // were current. Same rows, different meaning, and the wrong one is not visibly wrong.
      let rows = this.rows;
      if (this.toggled) {
        // A view declares one or the other, never both today -- but applying collapse first would
        // be the right order if one ever did: fold duplicates, then drop what is finished.
        if (this.config.collapse) rows = collapse(rows, this.config.collapse);
        if (this.config.exclude) rows = rows.filter((row) => !this.config.exclude(row));
      }

      if (needle) {
        // Matched against the RENDERED cells, not the raw fields, so what you can see is what you
        // can search -- "progression", "never", "12 / 216" all work, and a field the table does not
        // show cannot match something invisible.
        rows = rows.filter((row) =>
          this.config
            .cells(row)
            .map(cellText)
            .join(" ")
            .toLowerCase()
            .includes(needle)
        );
      }

      if (this.sort) {
        const { key, dir } = this.sort;
        const type = this.typeOf(key);
        // A column may sort by something other than the field it displays -- see `sortValues`.
        const valueOf =
          (this.config.sortValues && this.config.sortValues[key]) || ((row) => row[key]);
        rows = rows
          .slice()
          .sort((a, b) => compare(valueOf(a), valueOf(b), type) * (dir === "asc" ? 1 : -1));
      }

      this.tbody.replaceChildren(...rows.map((row) => this.rowElement(row)));
      if (this.empty) this.empty.hidden = rows.length > 0;
      this.renderSummary(rows);
    }

    // Fill the footer from whatever is on screen.
    renderSummary(rows) {
      if (!this.tfoot || !this.config.summary) return;
      // Hidden while there is nothing to summarize, so a table waiting on its first fetch shows no
      // row rather than a line of zeros -- which reads as a finished multiworld holding nothing.
      this.tfoot.hidden = rows.length === 0;
      if (!rows.length) return;

      const summary = this.config.summary(rows);
      Object.keys(summary).forEach((key) => {
        const cell = this.tfoot.querySelector(`.${key}`);
        if (!cell) return;
        // **Reset before rebuilding**, because this cell is reused on every render while
        // `appendCell` only ever adds: a `hint` class or a `title` left from a render when "last
        // seen" read "never" would stay on a real value afterwards, muted and carrying the wrong
        // tooltip. The class IS the key by construction, so restoring it is the whole reset.
        cell.className = key;
        cell.removeAttribute("title");
        cell.replaceChildren();
        // The same path a body cell takes, so a summary can carry a title or the `hint` class
        // exactly as a row does rather than needing a second cell renderer.
        appendCell(cell, summary[key], null);
      });
    }

    typeOf(key) {
      const th = this.headers.find((h) => h.dataset.key === key);
      return (th && th.dataset.type) || "text";
    }

    rowElement(row) {
      const tr = document.createElement("tr");
      const extra = this.config.rowClass && this.config.rowClass(row);
      if (extra) tr.className = extra;

      const href = this.config.href && this.config.href(row);
      this.config.cells(row).forEach((cell, index) => {
        const td = document.createElement("td");
        appendCell(td, cell, index === 1 && href ? href : null);
        tr.appendChild(td);
      });
      return tr;
    }
  }

  function appendCell(td, cell, href) {
    const value = typeof cell === "string" ? { text: cell } : cell;
    // `textContent`, never innerHTML: every string here is a player name, a game name or an item
    // name out of an uploaded seed, which is untrusted text.
    const target = href ? document.createElement("a") : td;
    if (href) {
      target.href = href;
      td.appendChild(target);
    }
    target.textContent = value.text;
    // On the CELL rather than the link inside it, so the whole cell is a hover target.
    if (value.title) td.title = value.title;
    if (value.class) td.classList.add(value.class);
    // Between the text and the tag, which is the order the object declares them in.
    if (value.note) {
      const note = document.createElement("span");
      note.className = "hint";
      note.textContent = value.note;
      td.append(" ", note);
    }
    if (value.tag) {
      const tag = document.createElement("span");
      tag.className = "tag";
      tag.textContent = value.tag;
      td.append(" ", tag);
    }
  }

  // One row per `key`, keeping the row with the highest `recency` and counting how many there were.
  //
  // The count rides on a COPY rather than being written onto the row, because `this.rows` is the
  // fetched data and is re-collapsed on every render -- mutating it would accumulate counts across
  // renders and survive the toggle being turned back off.
  function collapse(rows, config) {
    if (!config) return rows;
    const groups = new Map();

    for (const row of rows) {
      const key = row[config.key];
      const seen = groups.get(key);
      if (!seen) {
        groups.set(key, { row, count: 1 });
        continue;
      }
      seen.count += 1;
      if (row[config.recency] > seen.row[config.recency]) seen.row = row;
    }

    return Array.from(groups.values(), ({ row, count }) =>
      count > 1 ? Object.assign({}, row, { count }) : row
    );
  }

  function cellText(cell) {
    const value = typeof cell === "string" ? { text: cell } : cell;
    // Every rendered piece, in render order -- filtering matches what is on screen, so a count the
    // reader can see has to be searchable and one they cannot must not be.
    return `${value.text || ""} ${value.note || ""} ${value.tag || ""}`;
  }

  function compare(a, b, type) {
    // Nulls last in both directions: an untouched slot belongs at the end of "least recently seen"
    // and at the end of "most recently seen" alike, because it has no answer either way.
    if (a === null || a === undefined) return b === null || b === undefined ? 0 : 1;
    if (b === null || b === undefined) return -1;
    if (type === "number") return a - b;
    if (type === "boolean") return (a ? 1 : 0) - (b ? 1 : 0);
    return String(a).localeCompare(String(b), undefined, { numeric: true, sensitivity: "base" });
  }

  function parseSort(value) {
    if (!value) return null;
    const [key, dir] = value.split(":");
    return key ? { key, dir: dir === "desc" ? "desc" : "asc" } : null;
  }

  function setOrDelete(params, key, value) {
    if (value) params.set(key, value);
    else params.delete(key);
  }

  // --- polling ----------------------------------------------------------------------------------
  //
  // Two rules, and the second is the one that makes the first bearable:
  //
  //   * poll only while the tab is in the FOREGROUND and the interval has elapsed;
  //   * if the tab is foregrounded after the interval would have expired, refresh IMMEDIATELY.
  //
  // A background tab therefore costs nothing at all -- which matters for a page people leave open
  // for days -- and coming back to one never shows stale numbers while a timer runs down.
  //
  // The interval comes from the server (`next_poll_ms`), derived from the document's own cache
  // window: asking faster than that cannot produce new data, and only the server knows what it is.

  // **The controls are revealed here, exactly as `table.js` does it for the server-rendered
  // tables.** The class means "filtering and sorting are live on this page", not "table.js loaded"
  // -- and this file drives its own, so it has to say so too.
  //
  // Saying otherwise is what hid the items filter and its toggle: the stylesheet gates
  // `.table-controls` on the class, `table.js` was the only thing setting it, and the tracker does
  // not load `table.js`.
  document.documentElement.classList.add("js-tables");

  const tables = Array.from(root.querySelectorAll(".table-block")).map((s) => new Table(s));
  const freshness = document.getElementById("freshness");

  async function refreshAll() {
    lastPollAt = Date.now();
    const results = await Promise.all(tables.map((t) => t.refresh().catch(() => null)));
    lastResponseAt = Date.now();

    const document_ = results.find((r) => r);
    if (document_) {
      if (document_.next_poll_ms) intervalMs = document_.next_poll_ms;
      showFreshness(document_);
    }
  }

  function showFreshness(d) {
    if (!freshness) return;
    if (!d.stale) {
      freshness.hidden = true;
      return;
    }
    // The torn-down room, which for an async is most of its life. Deliberately NO start button: a
    // tracker's audience is not necessarily authorized to provision a pod.
    const when = new Date(d.as_of);
    freshness.textContent =
      `As of ${isNaN(when) ? d.as_of : when.toLocaleString()} — this room is not currently ` +
      `running, so this is the last state it reported.`;
    freshness.hidden = false;
  }

  function tick() {
    if (document.visibilityState !== "visible") return;
    if (Date.now() - lastPollAt < intervalMs) return;
    refreshAll();
  }

  refreshAll();
  // One cheap timer rather than a rescheduled timeout: it does nothing unless both conditions hold,
  // and it means the two rules above are expressed in one place instead of two.
  setInterval(tick, 1000);
  document.addEventListener("visibilitychange", tick);
})();
