//! Asking a room how it is, and asking it to stop.
//!
//! pahoa's admin API is the only thing that can answer "how many clients, how long idle, what are
//! this room's rules **now**" — and the last of those matters most, because a room's save is
//! authoritative for its gameplay options. After the first save, what Puna passed on the command
//! line describes how a room *started*, not how it *is*, and an organizer may have moved an option
//! with `!admin /option` since. So anything rendering a gameplay option must read it from here.
//!
//! ## `None` means "cannot tell", never zero
//!
//! Every field is optional and the distinction is load-bearing in both directions. `save` is `null`
//! for a room started without `--save-dir`; `activity` is `null` until a client has spoken. A zero
//! in either place would read as "saved nothing, ever" and "last spoke at the epoch", which are
//! different and alarming claims. The same rule governs [`TcpProbe`]: it can tell you a room is
//! reachable and nothing else, so it answers with a status that is entirely `None`.
//!
//! ## `clients_connected` counts SOCKETS, not players
//!
//! One player commonly holds three — game client, text client, tracker. Rendering it as a player
//! count is wrong, and **an idle reaper must read `activity.idle_seconds` rather than this number**,
//! or a room full of abandoned tracker tabs never reaps.
//!
//! ## The trait exists so the fallback is expressible
//!
//! `HttpsProbe` is the default and pahoa has shipped the whole surface, so `TcpProbe` is not a
//! transitional stage — it is what a room pinned to an older image gets. Under it the console is
//! **hidden entirely** rather than shown greyed out, and a graceful stop degrades to deleting the
//! Deployment, which is what `Step::Stop` already does.

pub mod http;
pub mod tcp;

use chrono::{DateTime, Utc};

use crate::model::command::{CommandOutput, RoomCommand};
use crate::room::{RoomEndpoint, RoomError};

pub use http::HttpsProbe;
pub use tcp::TcpProbe;

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error(transparent)]
    Room(#[from] RoomError),

    /// This probe cannot answer that at all — the `TcpProbe` case. Distinct from a failure, because
    /// a caller should hide a control rather than report an error.
    #[error("this probe cannot {what}")]
    Unsupported { what: &'static str },

    /// The room answered with something this build cannot read.
    #[error("the room's answer could not be understood: {0}")]
    Malformed(String),
}

impl ProbeError {
    /// Worth trying again on the next tick. Delegates to the transport's classification, which is
    /// where the `404`-means-no-Secret and `429`-means-back-off rules live.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Room(e) => e.is_transient(),
            Self::Unsupported { .. } | Self::Malformed(_) => false,
        }
    }

    /// How long to wait, when the room said to wait.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            Self::Room(RoomError::RateLimited { retry_after }) => *retry_after,
            _ => None,
        }
    }
}

/// What a probe can do, so a caller can hide what it cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeCapabilities {
    /// Numbers beyond reachability.
    pub status: bool,
    /// The typed command set.
    pub commands: bool,
    /// Quiesce-and-save, rather than deleting the Deployment and relying on SIGTERM.
    pub graceful_shutdown: bool,
}

/// The room's persistence, or `None` for a room that keeps nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SaveStatus {
    pub last_save_at: Option<DateTime<Utc>>,
    pub last_save_bytes: Option<i64>,
    pub last_save_micros: Option<i64>,
    pub save_interval_seconds: Option<i64>,
    /// State has changed since the last save was *started*.
    pub dirty: Option<bool>,
}

/// Counters from `pahoa-net::metrics`, which this endpoint is the reason for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetStatus {
    /// **Sockets, not players.** See the module docs.
    pub clients_connected: Option<i64>,
    pub mailbox_depth: Option<i64>,
    pub mailbox_peak: Option<i64>,
    /// Cumulative. Whatever re-exports this must treat it as a counter — see M11's note about
    /// `inc_by(new - old)` — or `rate()` breaks.
    pub lag_disconnects: Option<i64>,
    pub outbound_queued_bytes: Option<i64>,
    pub outbound_peak_bytes: Option<i64>,
    pub outbound_budget_bytes: Option<i64>,
    /// What turns §7's `slots * 3 * 96KiB` memory heuristic into a measurement.
    pub resident_bytes: Option<i64>,
}

/// `None` throughout until a client has spoken. Never zero for "never".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityStatus {
    pub last_client_message_at: Option<DateTime<Utc>>,
    pub idle_seconds: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotStatus {
    pub slot: i32,
    pub name: String,
    pub game: String,
    /// Open connections for this slot, commonly more than one for one player.
    pub connections: Option<i64>,
    pub checks: Option<i64>,
    pub total_checks: Option<i64>,
    /// Already a word from pahoa, not a number.
    pub status: Option<String>,
}

impl SlotStatus {
    /// Whether anybody is on this slot right now. Derived rather than read, so it cannot disagree
    /// with the count it is derived from.
    pub fn connected(&self) -> bool {
        self.connections.is_some_and(|n| n > 0)
    }
}

/// One room's answer to "how are you".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoomStatus {
    pub seed_name: Option<String>,
    pub pahoa_version: Option<String>,
    pub api_version: Option<i64>,
    pub started_at: Option<DateTime<Utc>>,
    /// `None` for a room that persists nothing, which pahoa reports as an explicit `null`.
    pub save: Option<SaveStatus>,
    pub net: NetStatus,
    pub activity: ActivityStatus,
    /// **The room's effective rules, and the only honest source for them.** Kept as opaque JSON on
    /// purpose: Puna stores no gameplay options of its own, so giving this a Rust shape would be
    /// Puna claiming to know a schema it does not own and would have to track through pahoa's
    /// releases. Render it; never compare it against what Puna passed.
    pub options: Option<serde_json::Value>,
    pub slots: Vec<SlotStatus>,
}

#[async_trait::async_trait]
pub trait RoomProbe: Send + Sync {
    /// Reachability at minimum; everything else where the probe can tell.
    ///
    /// The `admin_token` is a parameter rather than a field on [`RoomEndpoint`] deliberately:
    /// `RoomEndpoint` derives `Debug` and is logged, and a credential inside a `Debug` type is one
    /// `tracing::debug!` away from a container log.
    async fn status(
        &self,
        endpoint: &RoomEndpoint,
        admin_token: &str,
    ) -> Result<RoomStatus, ProbeError>;

    /// Ask the room to quiesce, save, release its `flock` and exit.
    ///
    /// **`202` means accepted, not finished**, and the caller must then watch for the Deployment to
    /// go away. It cannot mean finished: quiescing closes every connection including the one that
    /// asked, so a room that answered only when it was done could not answer at all.
    async fn request_shutdown(
        &self,
        endpoint: &RoomEndpoint,
        admin_token: &str,
        reason: &str,
    ) -> Result<(), ProbeError>;

    /// Run one typed command against a room.
    ///
    /// Returns the room's **answer**, including a refusal: `ok: false` is a `CommandOutput`, not an
    /// `Err`. Only a request the room could not understand, could not be sent, or was rate limited
    /// becomes an error — see [`crate::model::command::Disposition`] for why that line matters.
    async fn execute(
        &self,
        endpoint: &RoomEndpoint,
        admin_token: &str,
        command: &RoomCommand,
    ) -> Result<CommandOutput, ProbeError>;

    fn capabilities(&self) -> ProbeCapabilities;
}

/// Which probe an environment uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProbeKind {
    #[default]
    Https,
    Tcp,
}

impl ProbeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Tcp => "tcp",
        }
    }

    pub fn build(self) -> Box<dyn RoomProbe> {
        match self {
            Self::Https => Box::new(HttpsProbe),
            Self::Tcp => Box::new(TcpProbe),
        }
    }
}

impl std::str::FromStr for ProbeKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "https" => Ok(Self::Https),
            "tcp" => Ok(Self::Tcp),
            other => Err(format!(
                "unknown room probe {other:?}; expected https or tcp"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_kinds_round_trip() {
        for kind in [ProbeKind::Https, ProbeKind::Tcp] {
            assert_eq!(kind.as_str().parse::<ProbeKind>().unwrap(), kind);
        }
        assert!("grpc".parse::<ProbeKind>().is_err());
    }

    /// The default is HTTPS because pahoa has shipped the whole surface. `TcpProbe` exists for a
    /// room pinned to an older image, not as a transitional stage.
    #[test]
    fn https_is_the_default_and_carries_every_capability() {
        assert_eq!(ProbeKind::default(), ProbeKind::Https);

        let https = HttpsProbe.capabilities();
        assert!(https.status && https.commands && https.graceful_shutdown);

        let tcp = TcpProbe.capabilities();
        assert!(
            !tcp.status && !tcp.commands && !tcp.graceful_shutdown,
            "the TCP fallback must claim nothing it cannot do; a caller hides controls on this"
        );
    }

    /// `connected` is derived from the count so the two cannot disagree — and an idle *slot* with a
    /// tracker tab open is still connected, which is why a reaper must not read this either.
    #[test]
    fn connected_is_derived_from_the_socket_count() {
        let mut slot = SlotStatus::default();
        assert!(!slot.connected(), "unknown is not connected");

        slot.connections = Some(0);
        assert!(!slot.connected());

        slot.connections = Some(3);
        assert!(slot.connected(), "one player commonly holds three sockets");
    }

    #[test]
    fn rate_limiting_carries_its_wait_and_is_not_a_retry() {
        let e = ProbeError::Room(RoomError::RateLimited {
            retry_after: Some(std::time::Duration::from_secs(42)),
        });
        assert!(!e.is_transient());
        assert_eq!(e.retry_after(), Some(std::time::Duration::from_secs(42)));

        // Everything else has no wait to honor, so a caller falls back to its own.
        assert_eq!(
            ProbeError::Room(RoomError::Status { status: 503 }).retry_after(),
            None
        );
    }
}
