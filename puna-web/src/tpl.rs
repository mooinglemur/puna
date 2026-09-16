//! The context every template carries.
//!
//! `base.html` reads `base.*` and nothing else, so a page struct only has to hold its own data
//! plus one of these. Built from a [`Session`] rather than assembled by hand at each call site,
//! because a page that forgot to populate `is_admin` would silently hide the admin nav from an
//! administrator: a bug that looks like a permissions problem.

use std::sync::{LazyLock, RwLock};

use crate::auth::Session;

/// Set by `build.rs` from a hash of `static/`, for cache-busting asset URLs.
pub const STATIC_VERSION: &str = env!("STATIC_VERSION");

/// The commit this binary was built from, set by `build.rs`.
///
/// **The same string the image tag is made of**, because both come from `CI_COMMIT_SHORT_SHA`. So
/// the footer names something somebody can look up in the registry rather than a number that merely
/// resembles one. `dev` for a build with neither CI nor git.
pub const BUILD_REV: &str = env!("PUNA_BUILD_REV");

/// The pahoa image the orchestrator is configured to run, as the footer names it.
///
/// **Refreshed in the background rather than read per render**, which is the whole reason this is a
/// global. `TplContext::new` is sync, takes only a session, and is called from every page on both
/// tiers; a query there would put one round trip on every render of the highest-volume public
/// surface Puna has, for a string that changes when somebody repins an image.
///
/// **Read from `fleet` rather than from this tier's own environment.** `PUNA_PAHOA_IMAGE` is the
/// orchestrator's variable, and M17 settled that setting it here too is worse: two copies in git
/// that can drift, with the drift landing precisely on the thing the value exists to report. The
/// orchestrator publishes what it is actually configured with, and this reads that.
///
/// `None` until the first refresh answers, and `None` is rendered as nothing at all rather than as
/// a guess: a footer that names no pahoa is honest about a value it does not have yet.
static PAHOA_IMAGE: RwLock<Option<String>> = RwLock::new(None);

/// How long a repin takes to reach the footer. A repin is rare and the footer is provenance rather
/// than a control, so this is deliberately slack: the cost of a shorter interval is a query per
/// replica per interval, forever, and the cost of a longer one is a footer that lags a rollout.
pub const PAHOA_IMAGE_REFRESH: std::time::Duration = std::time::Duration::from_secs(600);

/// Publish what the orchestrator says it is configured with.
///
/// Takes `Option` rather than `&str` so a failed read leaves the previous answer standing: the
/// database being briefly unreachable is not evidence that the fleet has no pahoa image, and
/// blanking the footer on it would make a transient fault look like a configuration one.
///
/// **Reduced to the tag on the way in**, since the footer is the only reader and what it wants is
/// the revision. The stored value is a whole reference, and
/// `registry.git.mooinglemur.com/mooinglemur/pahoa:sha-65a80331` spends forty characters saying
/// where our registry lives before it gets to the eight that identify the build. `/admin/rooms`
/// still shows the reference in full, which is the page where the registry is part of the answer.
pub fn set_pahoa_image(image: Option<String>) {
    if let Some(image) = image
        && let Ok(mut held) = PAHOA_IMAGE.write()
    {
        *held = Some(image_tag(&image).to_string());
    }
}

/// The tag half of an image reference: everything after the last colon.
///
/// **The LAST colon, not the first**, which is the whole reason this is a function with a test
/// rather than a `split_once` at the call site: a registry may carry a port, so `split_once` on
/// `registry:5000/pahoa:sha-abc` answers `5000/pahoa:sha-abc` and looks right on every reference
/// that has no port, which is every one this deployment has.
///
/// A reference with no colon is returned whole: it names a repository with no tag, and the
/// alternative is a footer that says nothing where it could say something true.
fn image_tag(image: &str) -> &str {
    image.rsplit_once(':').map_or(image, |(_, tag)| tag)
}

fn pahoa_image() -> Option<String> {
    PAHOA_IMAGE.read().ok().and_then(|held| held.clone())
}

/// What this deployment calls itself, from `PUNA_SITE_NAME`.
///
/// **Defaulted rather than required**, unlike the orchestrator's deployment-specific values. Those
/// name a shared resource (an address, a port range, a label somebody else's policy matches), so
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
    /// footer: that one identifies the build and stays `puna`.
    pub site_name: &'static str,
    pub version: &'static str,
    /// The commit this build came from, appended to the version in the footer.
    pub build_rev: &'static str,
    /// The pahoa image the fleet is configured with, or `None` before the first refresh answers.
    /// Owned rather than borrowed because it comes from a lock this render does not hold.
    pub pahoa_image: Option<String>,
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
            build_rev: BUILD_REV,
            pahoa_image: pahoa_image(),
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

#[cfg(test)]
mod tests {
    use super::image_tag;

    /// **The tag is what follows the LAST colon.** Splitting on the first is the obvious spelling
    /// and is correct for every reference this deployment currently has, which is what makes it
    /// worth pinning: it breaks only once somebody puts a port on a registry, and then the footer
    /// reports most of a URL under a heading that says it is a revision.
    #[test]
    fn a_reference_reduces_to_its_tag() {
        assert_eq!(
            image_tag("registry.git.mooinglemur.com/mooinglemur/pahoa:sha-65a80331"),
            "sha-65a80331"
        );
        assert_eq!(
            image_tag("registry:5000/mooinglemur/pahoa:sha-abc"),
            "sha-abc"
        );

        // No tag at all: the repository names itself, and that is better than nothing.
        assert_eq!(image_tag("pahoa"), "pahoa");
    }
}
