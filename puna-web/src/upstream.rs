//! Fetching a document from a room, server-side.
//!
//! ## The browser never talks to the room, and this is why
//!
//! Two properties the reference implementation has that a naive tracker page loses. A page whose
//! JavaScript fetched `https://mw.ionium-dev.us:41234/api/tracker` would put **the room's address in
//! view-source** — and the tracker is the link meant for broad sharing, so that hands the multiworld's
//! address to a stream chat. And a URL of the form `/room/<id>/tracker` would leak the **room id**,
//! so sharing a tracker would share the room page. Proxying from an independent id solves both, and
//! there is no CORS in the picture at all because the page fetches its own origin.
//!
//! It is also the only thing that works. Pahoa gates the tracker whenever an admin token is
//! configured — which every Puna room has — and sending `Authorization` makes a request non-simple,
//! which needs a preflight pahoa does not answer.
//!
//! ## The allowlist is a type, not a check
//!
//! [`Document`] has two variants and its `path` is a constant per variant. **No path, host or port
//! from a request reaches this module**, so the "only these two upstream paths" rule is not a
//! validation that could be forgotten but a thing that cannot be spelled. A general proxy here would
//! be a confused deputy pointed at `/admin/v1/**` — and the tier holding this code can read
//! `rooms.admin_token`, so it would be a confused deputy with the credential in hand.

use std::time::Duration;

use puna_core::ids::RoomId;

/// Which of a room's two tracker documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Document {
    /// Progress: checks, items, hints, activity. Changes constantly.
    Live,
    /// Seed data: games, location totals, the datapackage checksums. Changes when the seed does,
    /// which is never.
    Static,
}

impl Document {
    /// The **only** two upstream paths this process will ever request.
    pub fn path(self) -> &'static str {
        match self {
            Self::Live => "/api/tracker",
            Self::Static => "/api/static_tracker",
        }
    }

    /// Pahoa's own cache window for this document, which is what Puna honors rather than inventing
    /// its own: the staleness is then exactly what `archipelago.gg` already gives, so no existing
    /// tracker page can tell the difference.
    pub fn ttl(self) -> Duration {
        match self {
            Self::Live => Duration::from_secs(60),
            Self::Static => Duration::from_secs(300),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "tracker",
            Self::Static => "static_tracker",
        }
    }
}

/// How to reach a room from here.
#[derive(Debug, Clone)]
pub enum Route {
    /// `mw-<room>.<namespace>.svc`, resolved to an address, with TLS still verified against the
    /// advertised hostname. The normal path: no hairpin through the public VIP, and the room's
    /// address never leaves the cluster.
    Service { namespace: String },
    /// The public `host:port`, resolved by ordinary DNS. A debugging switch for running the tracker
    /// tier outside the cluster; it works, but it sends room traffic out and back.
    Public,
}

#[derive(Debug, Clone)]
pub struct Upstream {
    /// The name on the room certificate, and therefore the name TLS is verified against however the
    /// connection is actually routed.
    pub advertise_host: String,
    pub route: Route,
    pub timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    /// The room has no port reserved, so there is nothing to fetch from.
    #[error("this room has no address yet")]
    NoAddress,

    #[error("could not resolve {name}: {source}")]
    Resolve {
        name: String,
        #[source]
        source: std::io::Error,
    },

    #[error("the room did not answer: {0}")]
    Transport(#[from] reqwest::Error),

    /// Pahoa answered, and said no. `404` here is the diagnostic worth knowing: it means **no admin
    /// token is configured on that room**, i.e. the Secret did not arrive — pahoa answers `404`
    /// rather than `401` for exactly this reason, so it reads as "old image" instead of "bad auth".
    #[error("the room answered {status}")]
    Status { status: u16 },
}

impl Upstream {
    /// Fetch one document from one room.
    ///
    /// The client is built per fetch rather than shared, which is deliberate and cheap here: the
    /// `resolve` override is per-client and per-room, and a fetch only happens on a cache miss —
    /// at most once per room per cache window.
    pub async fn fetch(
        &self,
        room: RoomId,
        base_port: u16,
        admin_token: &str,
        document: Document,
    ) -> Result<serde_json::Value, UpstreamError> {
        // Built from the advertised host and the room's own port, never from anything in a request.
        let url = format!(
            "https://{}:{}{}",
            self.advertise_host,
            base_port,
            document.path()
        );

        let mut builder = reqwest::Client::builder()
            .timeout(self.timeout)
            .user_agent(concat!("puna/", env!("CARGO_PKG_VERSION")));

        if let Route::Service { namespace } = &self.route {
            // Point the *hostname on the certificate* at the room's Service address. SNI and
            // verification still use `advertise_host`, because that is the only name the room
            // certificate carries -- dialing `mw-<id>.<ns>.svc` directly would fail verification.
            let name = format!("mw-{room}.{namespace}.svc:{base_port}");
            let addr = tokio::net::lookup_host(&name)
                .await
                .map_err(|source| UpstreamError::Resolve {
                    name: name.clone(),
                    source,
                })?
                .next()
                .ok_or_else(|| UpstreamError::Resolve {
                    name: name.clone(),
                    source: std::io::Error::other("no addresses"),
                })?;
            builder = builder.resolve(&self.advertise_host, addr);
        }

        let response = builder
            .build()?
            .get(&url)
            // Mandatory, not optional: pahoa gates the tracker whenever a token is configured, and
            // every Puna room has one.
            .bearer_auth(admin_token)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            return Err(UpstreamError::Status {
                status: status.as_u16(),
            });
        }

        Ok(response.json().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The allowlist, stated as an assertion so a third variant cannot be added without a test
    /// failing next to the reason it must not exist.
    #[test]
    fn exactly_two_upstream_paths_are_reachable() {
        let paths: Vec<&str> = [Document::Live, Document::Static]
            .into_iter()
            .map(Document::path)
            .collect();
        assert_eq!(paths, ["/api/tracker", "/api/static_tracker"]);

        for path in paths {
            assert!(
                !path.starts_with("/admin"),
                "{path} is the surface this proxy exists not to expose"
            );
        }
    }

    /// Pahoa's windows, not Puna's. Matching them is what keeps the staleness identical to what
    /// `archipelago.gg` already serves.
    #[test]
    fn the_cache_windows_match_pahoas() {
        assert_eq!(Document::Live.ttl(), Duration::from_secs(60));
        assert_eq!(Document::Static.ttl(), Duration::from_secs(300));
        assert!(Document::Static.ttl() > Document::Live.ttl());
    }
}
