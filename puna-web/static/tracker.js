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
      // Only on the multiworld page, and built from the id already in this URL rather than from
      // anything the server sent: a slot's own tracker id is deliberately never in the JSON.
      href: (r) => (slotQuery ? null : `/tracker/${idFromApi()}/0/${r.slot}`),
    },

    locations: {
      rows: (d) => d.locations,
      cells: (r) => [r.name, r.checked ? "✔" : ""],
      rowClass: (r) => (r.checked ? "done" : null),
    },

    items: {
      rows: (d) => d.items,
      cells: (r) => [
        String(r.order),
        { text: r.item, tag: r.classification === "filler" ? null : r.classification },
        r.from_name,
        r.location,
      ],
    },

    hints: {
      rows: (d) => d.hints,
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

  // `null` is NEVER, and never is not 1970 -- rendering an epoch date is the classic way to make an
  // untouched slot look like an abandoned one. The server sends an age it computed, so a skewed
  // client clock cannot produce a negative one; this adds the time since that response arrived, so
  // the column keeps ticking between polls without a fetch.
  function age(msAgo) {
    if (msAgo === null || msAgo === undefined) return { text: "never", class: "hint" };
    const minutes = Math.floor((msAgo + (Date.now() - lastResponseAt)) / 60000);
    if (minutes < 1) return "just now";
    if (minutes < 60) return `${minutes}m ago`;
    if (minutes < 2880) return `${Math.floor(minutes / 60)}h ago`;
    return `${Math.floor(minutes / 1440)}d ago`;
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
      this.empty = section.querySelector(".empty");
      this.search = section.querySelector(".table-search");
      this.headers = Array.from(section.querySelectorAll("th[data-key]"));
      this.details = section.querySelector("details");
      this.rows = [];

      const state = readState();
      this.query = state.get(`${this.view}.q`) || "";
      this.sort = parseSort(state.get(`${this.view}.sort`));
      if (this.search) this.search.value = this.query;
      if (this.details && state.get(`${this.view}.open`) === "1") this.details.open = true;

      this.bind();
      this.markHeaders();
    }

    bind() {
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
      setOrDelete(params, `${this.view}.sort`, this.sort ? `${this.sort.key}:${this.sort.dir}` : "");
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
      let rows = this.rows;

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
        rows = rows.slice().sort((a, b) => compare(a[key], b[key], type) * (dir === "asc" ? 1 : -1));
      }

      this.tbody.replaceChildren(...rows.map((row) => this.rowElement(row)));
      if (this.empty) this.empty.hidden = rows.length > 0;
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
    if (value.class) td.classList.add(value.class);
    if (value.tag) {
      const tag = document.createElement("span");
      tag.className = "tag";
      tag.textContent = value.tag;
      td.append(" ", tag);
    }
  }

  function cellText(cell) {
    const value = typeof cell === "string" ? { text: cell } : cell;
    return `${value.text || ""} ${value.tag || ""}`;
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
