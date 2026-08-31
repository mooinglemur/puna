//! Reaching a room over pahoa's own TLS.
//!
//! Two tiers dial rooms and they must dial them the same way: the **tracker** fetches documents, and
//! the **orchestrator** probes and commands. This is the transport both use, and it lives here
//! rather than in either of them because the interesting part is a security property that must not
//! be implemented twice.
//!
//! ## The certificate names the room, and the address does not
//!
//! In-cluster, Puna connects to `mw-<room>.<namespace>.svc` — but the room's certificate carries
//! exactly one name, `rooms.example.com`, because every room shares that hostname and differs only by
//! port (D10). So the connection **resolves the Service name to an address and then verifies against
//! the advertised host**, via reqwest's `resolve` override. Dialing the Service name directly would
//! fail verification; disabling verification to make it work would throw away the reason the
//! certificate exists.
//!
//! The alternative — hairpinning through the public VIP — works and is kept as a switch
//! ([`Route::Public`]) for running a tier outside a cluster, but it sends in-cluster traffic out to
//! the load balancer and back.
//!
//! ## What this module deliberately is not
//!
//! It builds URLs from a **room row and a caller-chosen path constant**, never from anything in an
//! HTTP request. The tracker's two-path allowlist is enforced one level up, in its own `Document`
//! type, and stays there: this is a transport, and giving it an opinion about paths would put the
//! allowlist in the wrong place.

use std::time::Duration;

use crate::ids::RoomId;

/// How Puna finds a room's address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `mw-<room>.<namespace>.svc`, resolved to an address, with TLS still verified against the
    /// advertised hostname. The default: no hairpin through the public VIP.
    Service { namespace: String },
    /// The public `host:port`, resolved by ordinary DNS. A debugging switch for running a tier
    /// outside a cluster.
    Public,
}

/// One room, as something that can be dialed.
#[derive(Debug, Clone)]
pub struct RoomEndpoint {
    pub room: RoomId,
    /// The even half of the reserved pair. Both ports serve the same HTTP surface; the filtered one
    /// differs only in its WebSocket feed, so there is never a reason to probe it.
    pub base_port: u16,
    /// The name on the room certificate, and therefore the name TLS is verified against.
    pub advertise_host: String,
    pub route: Route,
    pub timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum RoomError {
    #[error("could not resolve {name}: {source}")]
    Resolve {
        name: String,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Transport(#[from] reqwest::Error),

    /// **Rate limited, and this needs its own case.** pahoa limits authentication failures to 10 a
    /// minute per room and the lockout applies to the *correct* token too, deliberately, so it
    /// cannot be used as an oracle. A caller that retries a failing call in a tight loop therefore
    /// locks itself out — so this carries `Retry-After` and must not be folded into an ordinary
    /// transport error or into the Kubernetes-client backoff, which measures something else.
    #[error("the room is rate limiting; retry after {retry_after:?}")]
    RateLimited { retry_after: Option<Duration> },

    /// The room answered, and said no.
    ///
    /// **`404` is the diagnostic worth knowing**: it means no admin token is configured on that
    /// room, i.e. the Secret did not arrive. pahoa answers `404` rather than `401` for exactly this
    /// reason, so it reads as "the Secret is missing" rather than "the token is wrong".
    #[error("the room answered {status}")]
    Status { status: u16 },

    /// The room answered, said no, **and said why** — its own words, carried through.
    ///
    /// A `400` from pahoa is a Puna bug by construction (§6: the body is serialized from a typed
    /// enum, so the room failing to parse it means the two have drifted), and pahoa states the
    /// reason in `{"error": …}`. Discarding it cost real time: a chat filter written `from_slot`
    /// `PrintJSON` — a pairing pahoa refuses because it can never match — reached an operator as
    /// *"could not apply the filter: the room answered 400"*, over a page still showing the filter
    /// as the room's. The room had said "a print_json cannot travel from_slot" all along.
    #[error("the room answered {status}: {detail}")]
    Refused { status: u16, detail: String },
}

impl RoomError {
    /// Whether this is worth trying again on the next tick.
    ///
    /// `404` is not: a room with no token will not grow one without a Secret being rewritten, and
    /// hammering it turns a configuration fault into load. Rate limiting is not either — it has its
    /// own wait, which the caller must honor rather than re-attempt.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Transport(_) | Self::Resolve { .. } => true,
            Self::RateLimited { .. } => false,
            // One rule for both, because carrying the room's explanation must not change whether
            // the call is retried: a `400` with a reason is the same non-transient Puna bug a
            // `400` without one is.
            Self::Status { status } | Self::Refused { status, .. } => *status >= 500,
        }
    }
}

/// A room's error, with its own explanation attached when it gave one.
///
/// `classify` deliberately takes `&Response` and cannot read a body, so this is the async half:
/// only reached on a failure, so a healthy call pays nothing. pahoa answers `{"error": …}`; anything
/// else — an empty body, a proxy's HTML — leaves the error exactly as it was rather than pasting
/// something unhelpful into an operator's face.
pub async fn explain(error: RoomError, response: reqwest::Response) -> RoomError {
    let RoomError::Status { status } = error else {
        return error;
    };
    let detail = response
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|body| {
            body.get("error")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
    match detail {
        Some(detail) if !detail.trim().is_empty() => RoomError::Refused { status, detail },
        _ => RoomError::Status { status },
    }
}

impl RoomEndpoint {
    /// `https://<advertise_host>:<port><path>` — built from the row, never from a request.
    pub fn url(&self, path: &str) -> String {
        format!("https://{}:{}{}", self.advertise_host, self.base_port, path)
    }

    /// A client that will reach this room and verify its certificate.
    ///
    /// Built per call rather than shared, which is deliberate and cheap: the `resolve` override is
    /// per-client and per-room, so one shared client cannot serve two rooms. Callers make at most a
    /// handful of calls per room per tick.
    pub async fn client(&self) -> Result<reqwest::Client, RoomError> {
        let mut builder = reqwest::Client::builder()
            .timeout(self.timeout)
            .user_agent(concat!("puna/", env!("CARGO_PKG_VERSION")));

        if let Route::Service { namespace } = &self.route {
            // Point the *hostname on the certificate* at the room's Service address. SNI and
            // verification still use `advertise_host`, because that is the only name the room
            // certificate carries.
            let name = format!("mw-{}.{}.svc:{}", self.room, namespace, self.base_port);
            let addr = tokio::net::lookup_host(&name)
                .await
                .map_err(|source| RoomError::Resolve {
                    name: name.clone(),
                    source,
                })?
                .next()
                .ok_or_else(|| RoomError::Resolve {
                    name: name.clone(),
                    source: std::io::Error::other("no addresses"),
                })?;
            builder = builder.resolve(&self.advertise_host, addr);
        }

        Ok(builder.build()?)
    }
}

/// Turn a non-success response into the error that describes it.
///
/// Separated from the request so every caller classifies a status the same way — in particular so
/// nobody forgets that `429` is not an ordinary failure.
pub fn classify(response: &reqwest::Response) -> Option<RoomError> {
    let status = response.status();
    if status.is_success() {
        return None;
    }

    if status.as_u16() == 429 {
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            // Only the delta-seconds form. pahoa sends that; an HTTP-date is legal in the spec and
            // parsing one would mean trusting the room's clock against ours, which is precisely the
            // comparison to avoid. Unparseable becomes `None`, and the caller falls back to its own
            // wait rather than to zero.
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        return Some(RoomError::RateLimited { retry_after });
    }

    Some(RoomError::Status {
        status: status.as_u16(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> RoomEndpoint {
        RoomEndpoint {
            room: RoomId::new(),
            base_port: 41234,
            advertise_host: "mw.example".into(),
            route: Route::Public,
            timeout: Duration::from_secs(5),
        }
    }

    /// The URL is the advertised host and the room's own port. Never the Service name — that is
    /// where the connection *goes*, not what the certificate says.
    #[test]
    fn a_url_names_the_certificate_host_not_the_service() {
        let ep = endpoint();
        assert_eq!(
            ep.url("/admin/v1/status"),
            "https://mw.example:41234/admin/v1/status"
        );
        assert!(!ep.url("/healthz").contains(".svc"));
    }

    /// Retry classification, which is the thing a caller acts on.
    #[test]
    fn only_transport_and_server_errors_are_worth_another_tick() {
        assert!(
            RoomError::Resolve {
                name: "x".into(),
                source: std::io::Error::other("no"),
            }
            .is_transient()
        );
        assert!(RoomError::Status { status: 503 }.is_transient());

        // A room with no admin token will not grow one without a Secret being rewritten.
        assert!(!RoomError::Status { status: 404 }.is_transient());
        assert!(!RoomError::Status { status: 401 }.is_transient());

        // **Not transient**, because it has its own wait. Treating it as "try next tick" is how a
        // reconciler locks itself out of its own rooms for the rest of the window.
        assert!(!RoomError::RateLimited { retry_after: None }.is_transient());
    }
}
