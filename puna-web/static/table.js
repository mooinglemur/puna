// Filter and sort for a table the SERVER rendered.
//
// The tracker's tables get this from `tracker.js`, which sorts a JSON array and re-renders the
// rows. That machinery does not transfer: it owns its data, and this owns nothing -- the rows are
// markup that arrived complete, and everything here is a reordering of elements that already exist.
// Same conventions, though, deliberately: `th[data-key]` headers, a `.table-search` box, and the
// `data-sort` attribute the stylesheet draws arrows from. One look, whichever file drew it.
//
// Attach by marking the table `data-sortable`. Sorting reads `data-value` on a cell when present
// and its text otherwise, so a column that displays "3 days ago" can sort on the instant behind it
// without this file knowing what a date is.
//
// Progressive enhancement, and it has to be: the rows are already there and already readable. With
// scripting off you get a table in the order the server chose, which is the order it chose for a
// reason.
(function () {
  "use strict";

  // Numeric when every value on both sides parses, textual otherwise. Decided per comparison rather
  // than per column so a column of numbers with one "—" in it still sorts as numbers.
  function compare(a, b) {
    var x = parseFloat(a),
      y = parseFloat(b);
    if (!isNaN(x) && !isNaN(y) && /^[\d.+-]/.test(a) && /^[\d.+-]/.test(b)) return x - y;
    return a.localeCompare(b, undefined, { numeric: true, sensitivity: "base" });
  }

  function valueOf(row, index) {
    var cell = row.cells[index];
    if (!cell) return "";
    // An explicit `data-value` beats the rendered text, for columns whose display is not their
    // order. Everything else sorts on what it shows, which is what somebody reading it expects.
    return (cell.dataset.value !== undefined ? cell.dataset.value : cell.textContent).trim();
  }

  function attach(table) {
    var body = table.tBodies[0];
    if (!body) return;
    var headers = Array.from(table.querySelectorAll("th[data-key]"));
    // The server's order, kept so a third click on a header can return to it. Sorting is a lens,
    // and a lens you cannot take off is a worse one.
    var original = Array.from(body.rows);
    var sort = null;

    function apply() {
      var rows = original.slice();
      if (sort) {
        var index = sort.index;
        rows.sort(function (a, b) {
          return compare(valueOf(a, index), valueOf(b, index)) * (sort.dir === "asc" ? 1 : -1);
        });
      }
      rows.forEach(function (row) {
        body.appendChild(row);
      });

      headers.forEach(function (th, i) {
        var active = sort && sort.index === i;
        th.dataset.sort = active ? sort.dir : "";
        th.setAttribute(
          "aria-sort",
          active ? (sort.dir === "asc" ? "ascending" : "descending") : "none",
        );
      });
    }

    headers.forEach(function (th, index) {
      // Real controls to a keyboard. The stylesheet already draws the affordances; without these
      // the arrows would appear over something only a mouse could operate.
      th.tabIndex = 0;
      th.setAttribute("role", "button");

      function toggle() {
        if (!sort || sort.index !== index) sort = { index: index, dir: "asc" };
        else if (sort.dir === "asc") sort = { index: index, dir: "desc" };
        else sort = null; // back to the server's order
        apply();
      }

      th.addEventListener("click", toggle);
      th.addEventListener("keydown", function (event) {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          toggle();
        }
      });
    });

    var search = document.querySelector('[data-filters="' + table.id + '"]');
    var onlyMine = document.querySelector('[data-only="mine"][data-table="' + table.id + '"]');

    // **One place decides whether a row is visible.** Two independent controls each setting
    // `hidden` would fight: clearing the search box would reveal rows the toggle is hiding, and
    // whichever ran last would win. A row shows when it passes BOTH.
    function refilter() {
      var needle = search ? search.value.trim().toLowerCase() : "";
      var mineOnly = !!(onlyMine && onlyMine.checked);
      original.forEach(function (row) {
        // Matched against the RENDERED text, so what you can see is what you can search -- and a
        // value the table does not show cannot match invisibly.
        var matches = needle === "" || row.textContent.toLowerCase().includes(needle);
        var mine = !mineOnly || row.dataset.mine !== undefined;
        row.hidden = !(matches && mine);
      });
    }

    if (search) search.addEventListener("input", refilter);
    if (onlyMine) onlyMine.addEventListener("change", refilter);
  }

  // Wire every sortable table under `root` that is not wired already.
  //
  // Exposed because not every table is in the document at load: `/admin/rooms` fetches its
  // stopped-and-closed table when the section is opened, and a table that arrived late would
  // otherwise render its sort arrows over headers that do nothing -- an affordance that lies,
  // which is worse than no affordance. `data-wired` is the idempotence: scanning twice would
  // attach two click handlers to each header and make every sort a double-toggle.
  function scan(root) {
    (root || document).querySelectorAll("table[data-sortable]").forEach(function (table) {
      if (table.dataset.wired) return;
      table.dataset.wired = "1";
      attach(table);
    });
  }

  window.PunaTables = { scan: scan };
  scan(document);
})();
