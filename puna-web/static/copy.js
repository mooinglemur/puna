// Copy a value to the clipboard, with a floating confirmation.
//
// Its own file because it is not room-specific: the room page copies an address, a slot password
// and a claim link; `/admin/users` copies a Discord id. It lived inside `room.js`, which returns
// early when the page has no room panel, so loading that file elsewhere for the clipboard would
// have produced a copy button that silently did nothing, which is precisely the failure the
// feature detection below exists to prevent, arriving by a different road.
//
// Attach by marking a control `.copy` with `data-copy="<the value>"`. A value beginning with `/`
// is resolved against the origin, so a claim link copies as a whole URL while an address, which is
// `host:port` and not a path, is copied exactly as written.
(function () {
  "use strict";

  // Feature-detected before the controls are revealed, not after they are clicked.
  // `navigator.clipboard` requires a secure context, so on plain HTTP it is simply absent, and a
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
  // not simply be made `absolute` either: the global `table` rule sets `overflow-x: auto`, so the
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
    // tooltip to fit: a confirmation rendered off-screen is the same as none, and this is the one
    // case where somebody would go on to paste something they never copied.
    if (rect.top < 44) {
      confirmation.classList.add("below");
      confirmation.style.top = rect.bottom + 8 + "px";
    } else {
      confirmation.style.top = rect.top - 8 + "px";
    }

    // Positioned once rather than tracked. Over a second and a bit that is right for everything
    // except scrolling, where the anchor moves and the tooltip would not, so scrolling takes it
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

    // A value beginning with `/` is a path this page rendered, and what somebody wants on their
    // clipboard is the whole link: a claim URL is a thing you send to a person, not something
    // they retype. Anything else is copied verbatim: an address is `host:port` and must not be
    // mangled into a URL.
    var text = button.dataset.copy;
    if (text.charAt(0) === "/") text = new URL(text, location.origin).href;
    navigator.clipboard.writeText(text).then(
      function () {
        confirmCopy(button, "Copied to clipboard");
      },
      function () {
        // The API exists and refused: a permissions policy, or a click the browser did not count
        // as a user gesture. Say so rather than claiming success: the address is still right there
        // to select by hand, and this is the case where somebody would otherwise paste the wrong
        // thing into a game client and wonder why it will not connect.
        confirmCopy(button, "Could not copy", true);
      },
    );
  });
})();
