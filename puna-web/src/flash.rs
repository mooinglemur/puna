//! One-shot page notices, carried in a cookie rather than in the URL.
//!
//! Every one of these is the sentence a POST wants to say after it redirects, and a redirect
//! carries nothing else. The obvious place to put it is a query parameter -- which is where these
//! started, and which has three problems that only the third makes urgent:
//!
//!   * **It survives a refresh.** The message is about something that happened once, so a page
//!     still saying "Queued." ten minutes and two refreshes later is stating a fact about the past
//!     as though it were the present.
//!   * **It survives a bookmark and a paste.** The URL an operator copies out of the address bar
//!     carries a stale sentence into wherever they paste it.
//!   * **Anyone can write one.** `/admin/rooms?notice=<whatever>` renders whatever it is handed,
//!     in the site's own voice, on the site's own admin page. It mutates nothing -- so it is not
//!     CSRF -- but it does not have to mutate anything to be useful: a link that makes Puna appear
//!     to tell an administrator something is exactly the shape a phishing message wants.
//!
//! A cookie fixes all three at once, because the message is delivered *out of band from the link*.
//! Rocket's [`Flash`] sets it on the redirect and [`FlashMessage`] removes it as it is read, so it
//! shows once and is gone -- a refresh renders the same page with no notice, which is the behavior
//! the URL form only pretended to have.
//!
//! It is not a security control and should not be mistaken for one: the cookie is not encrypted,
//! and someone who can already run script on this origin can set it. What it removes is the
//! *link* -- the thing you can send to somebody else.

use rocket::request::FlashMessage;

/// A notice to render once, with the stylesheet class that carries its severity.
///
/// The class is decided here rather than in markup so the three kinds cannot drift between pages:
/// `.notice`, `.warning` and `.error` are visually distinct by border as well as hue, and a page
/// that picked its own class for a failure would report it in the color of a success.
pub struct Notice {
    pub class: &'static str,
    pub message: String,
}

impl Notice {
    /// Read the pending notice, if there is one. Reading it is what clears it.
    ///
    /// Takes the guard by value because that is the whole mechanism: `FlashMessage`'s
    /// `FromRequest` removes the cookie as it resolves, so the notice is consumed by the request
    /// that renders it whether or not this is called.
    pub fn take(flash: Option<FlashMessage<'_>>) -> Option<Self> {
        flash.map(|flash| Self {
            class: match flash.kind() {
                "warning" => "warning",
                "error" => "error",
                // Rocket's own kind for `Flash::success`, plus anything a future caller invents:
                // an unknown severity renders as the quiet one rather than as an alarm.
                _ => "notice",
            },
            message: flash.message().to_string(),
        })
    }
}
