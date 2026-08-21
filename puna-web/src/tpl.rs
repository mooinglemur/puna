//! The context every template carries.
//!
//! `base.html` reads `base.*` and nothing else, so a page struct only has to hold its own data
//! plus one of these. Built from a [`Session`] rather than assembled by hand at each call site,
//! because a page that forgot to populate `is_admin` would silently hide the admin nav from an
//! administrator -- a bug that looks like a permissions problem.

use std::sync::LazyLock;

use crate::auth::Session;

/// Set by `build.rs` from a hash of `static/`, for cache-busting asset URLs.
pub const STATIC_VERSION: &str = env!("STATIC_VERSION");

/// What this deployment calls itself, from `PUNA_SITE_NAME`.
///
/// **Defaulted rather than required**, unlike the orchestrator's deployment-specific values. Those
/// name a shared resource -- an address, a port range, a label somebody else's policy matches -- so
/// a default there is one deployment's answer silently adopted by another. This one names nothing
/// but itself: the worst a missing value can do is show the software's own name, which is true.
///
/// Read once from the process environment rather than threaded through Rocket's state, because
/// `TplContext::new` is called from every page and has only a session to work with. An environment
/// variable cannot change under a running process, so there is nothing for a `State` to buy.
static SITE_NAME: LazyLock<String> = LazyLock::new(|| {
    std::env::var("PUNA_SITE_NAME")
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "puna".to_string())
});

#[derive(Debug, Clone)]
pub struct TplContext {
    pub is_logged_in: bool,
    pub is_admin: bool,
    pub username: String,
    /// The name in the corner, in the tab, and on the landing page. Not the software's name in the
    /// footer -- that one identifies the build and stays `puna`.
    pub site_name: &'static str,
    pub version: &'static str,
    pub static_version: &'static str,
    /// Whose eyes this page is being seen through, when an administrator is viewing as somebody
    /// else. `None` in every ordinary request.
    ///
    /// On [`TplContext`] rather than on the one page that offers the control, because the banner
    /// has to be on **every** page: the whole state is that the site looks like somebody else's,
    /// and a reminder that appears only where you started would be missing exactly where it is
    /// needed. `username` beside it is already the person being viewed, not the viewer.
    pub view_as: Option<String>,
}

impl TplContext {
    pub fn new(session: &Session) -> Self {
        Self {
            is_logged_in: session.is_logged_in,
            is_admin: session.is_admin,
            username: session.username.clone().unwrap_or_default(),
            site_name: site_name(),
            version: puna_core::VERSION,
            static_version: STATIC_VERSION,
            view_as: session.view_as.as_ref().map(|v| v.admin_username.clone()),
        }
    }
}

/// This deployment's name. `'static` because [`SITE_NAME`] is, which is what lets every page hold
/// it as a `&str` rather than cloning a `String` per render.
pub fn site_name() -> &'static str {
    SITE_NAME.as_str()
}
