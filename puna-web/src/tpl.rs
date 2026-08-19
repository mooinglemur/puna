//! The context every template carries.
//!
//! `base.html` reads `base.*` and nothing else, so a page struct only has to hold its own data
//! plus one of these. Built from a [`Session`] rather than assembled by hand at each call site,
//! because a page that forgot to populate `is_admin` would silently hide the admin nav from an
//! administrator -- a bug that looks like a permissions problem.

use crate::auth::Session;

/// Set by `build.rs` from a hash of `static/`, for cache-busting asset URLs.
pub const STATIC_VERSION: &str = env!("STATIC_VERSION");

#[derive(Debug, Clone)]
pub struct TplContext {
    pub is_logged_in: bool,
    pub is_admin: bool,
    pub username: String,
    pub version: &'static str,
    pub static_version: &'static str,
}

impl TplContext {
    pub fn new(session: &Session) -> Self {
        Self {
            is_logged_in: session.is_logged_in,
            is_admin: session.is_admin,
            username: session.username.clone().unwrap_or_default(),
            version: puna_core::VERSION,
            static_version: STATIC_VERSION,
        }
    }
}
