// Fill a `<details>` from the server the first time it is opened.
//
// For a section whose contents grow without bound and which most visits do not need: today that
// is `/admin/rooms`'s list of stopped and closed rooms, which every room anybody ever stopped joins
// and never leaves. Loading it with the page would make the page an operator opens during an
// incident slower every week, to answer a question they did not ask.
//
// Mark the element `data-loads="<url>"`. The response is a server-rendered fragment and replaces
// whatever `[data-placeholder]` holds; nothing here builds markup, so there is no second copy of
// the table to keep in step with the first.
//
// Progressive enhancement, and the `<noscript>` inside each section is the other half: with
// scripting off the section would be a heading over an empty box, so the markup carries a plain
// link to the same content as its own page.
(function () {
  "use strict";

  document.querySelectorAll("details[data-loads]").forEach(function (details) {
    var url = details.dataset.loads;
    var placeholder = details.querySelector("[data-placeholder]");
    if (!url || !placeholder) return;

    // Guards the fetch, not the open state: `toggle` fires on every open AND every close, and a
    // section somebody opens, closes and opens again must not ask three times.
    var started = false;

    details.addEventListener("toggle", function () {
      if (!details.open || started) return;
      started = true;

      fetch(url, { headers: { Accept: "text/html" } })
        .then(function (response) {
          if (!response.ok) throw new Error(response.status);
          return response.text();
        })
        .then(function (markup) {
          var holder = document.createElement("div");
          holder.innerHTML = markup;
          placeholder.replaceWith(holder);
          // The table arrived after `table.js` scanned, so it has sort arrows and no handlers
          // until this runs. An affordance that does nothing is worse than none.
          if (window.PunaTables) window.PunaTables.scan(holder);
        })
        .catch(function () {
          // Reset, so closing and reopening retries. A section stuck on "Loading..." forever is
          // indistinguishable from a slow query, and this is the one place a reader can act.
          started = false;
          placeholder.textContent = "Could not load these. Close this and open it again to retry.";
        });
    });
  });
})();
