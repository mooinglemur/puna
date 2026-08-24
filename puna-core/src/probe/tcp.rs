//! The fallback: can this room's port be opened, and nothing else.
//!
//! **Not a transitional stage.** pahoa has shipped the whole admin surface, so this exists for one
//! case — a room pinned to an image older than that — and the trait exists so that case is
//! expressible rather than because both were expected.
//!
//! It is still a meaningful signal: pahoa binds its listener only *after* the save is restored, so
//! an open port means the room is actually up rather than merely scheduled.
//!
//! What it deliberately does not do is invent numbers. A connect that succeeds yields a
//! [`RoomStatus`] that is entirely `None` — "it is up, and I cannot tell you anything else" — rather
//! than zeros, which would render as a room with no players and no activity. A caller reads
//! [`ProbeCapabilities`] and hides those columns instead.

use tokio::net::TcpStream;

use super::{ProbeCapabilities, ProbeError, RoomProbe, RoomStatus};
use crate::model::command::{CommandOutput, RoomCommand};
use crate::room::{RoomEndpoint, RoomError, Route};

pub struct TcpProbe;

#[async_trait::async_trait]
impl RoomProbe for TcpProbe {
    async fn status(
        &self,
        endpoint: &RoomEndpoint,
        _admin_token: &str,
    ) -> Result<RoomStatus, ProbeError> {
        // The Service name in-cluster, the advertised name outside it. No TLS: opening the socket is
        // the whole question, and completing a handshake would prove nothing more while needing the
        // certificate this probe exists to work without.
        let target = match &endpoint.route {
            Route::Service { namespace } => format!(
                "mw-{}.{}.svc:{}",
                endpoint.room, namespace, endpoint.base_port
            ),
            Route::Public => format!("{}:{}", endpoint.advertise_host, endpoint.base_port),
        };

        let connect = TcpStream::connect(&target);
        match tokio::time::timeout(endpoint.timeout, connect).await {
            Ok(Ok(_stream)) => Ok(RoomStatus::default()),
            Ok(Err(source)) => Err(RoomError::Resolve {
                name: target,
                source,
            }
            .into()),
            Err(_elapsed) => Err(RoomError::Resolve {
                name: target,
                source: std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the room did not accept a connection in time",
                ),
            }
            .into()),
        }
    }

    async fn request_shutdown(
        &self,
        _endpoint: &RoomEndpoint,
        _admin_token: &str,
        _reason: &str,
    ) -> Result<(), ProbeError> {
        // The caller degrades to deleting the Deployment, which is what `Step::Stop` does anyway:
        // the pod gets SIGTERM and 45 seconds, which is what pahoa's final save needs. The admin
        // route saves a few seconds, not a save.
        Err(ProbeError::Unsupported {
            what: "ask a room to shut down gracefully",
        })
    }

    async fn execute(
        &self,
        _endpoint: &RoomEndpoint,
        _admin_token: &str,
        _command: &RoomCommand,
    ) -> Result<CommandOutput, ProbeError> {
        // Unsupported rather than failed, so the console is HIDDEN rather than shown greyed out.
        // A control that is visible and refuses reads as a bug in Puna; an absent one reads as a
        // room Puna cannot drive, which is what it is.
        Err(ProbeError::Unsupported {
            what: "run commands against a room",
        })
    }

    async fn set_slot_password(
        &self,
        _endpoint: &RoomEndpoint,
        _admin_token: &str,
        _slot: i32,
        _password: Option<&str>,
    ) -> Result<(), ProbeError> {
        // The change still lands in the database and in the Secret; what is lost under this probe
        // is only the live push, so a rotation or a lock takes effect at the room's next start.
        Err(ProbeError::Unsupported {
            what: "set a slot password on a running room",
        })
    }

    async fn set_filter(
        &self,
        _endpoint: &RoomEndpoint,
        _admin_token: &str,
        _slot: Option<i32>,
        _rules: Option<&[crate::model::filter::Rule]>,
    ) -> Result<(), ProbeError> {
        // Same bargain as the password above: the intent is stored either way, and what this probe
        // cannot do is push it at a room that is already running. A room pinned to an image without
        // the filter resource is the case this exists for, and there the answer is honest rather
        // than a call that would 404.
        Err(ProbeError::Unsupported {
            what: "set a traffic filter on a running room",
        })
    }

    async fn metrics(
        &self,
        _endpoint: &RoomEndpoint,
        _admin_token: &str,
    ) -> Result<String, ProbeError> {
        Err(ProbeError::Unsupported {
            what: "read a room's own metrics",
        })
    }

    fn capabilities(&self) -> ProbeCapabilities {
        ProbeCapabilities {
            status: false,
            commands: false,
            graceful_shutdown: false,
            metrics: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn endpoint(port: u16) -> RoomEndpoint {
        RoomEndpoint {
            room: crate::ids::RoomId::new(),
            base_port: port,
            advertise_host: "127.0.0.1".into(),
            route: Route::Public,
            timeout: Duration::from_millis(500),
        }
    }

    /// An open port is "up, and I cannot tell you more" — **not** a room with zero players. Zeros
    /// here would render as a real reading of an idle room, which is the one thing a probe that
    /// knows nothing must not claim.
    #[tokio::test]
    async fn a_reachable_room_reports_nothing_rather_than_zeros() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let status = TcpProbe
            .status(&endpoint(port), "unused")
            .await
            .expect("the port is open");

        assert_eq!(status, RoomStatus::default());
        assert_eq!(status.net.clients_connected, None, "not Some(0)");
        assert_eq!(status.activity.idle_seconds, None);
        assert!(status.slots.is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_room_is_an_error_not_an_empty_status() {
        // Bound and dropped, so the port is almost certainly closed and definitely not ours.
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            listener.local_addr().expect("addr").port()
        };

        let result = TcpProbe.status(&endpoint(port), "unused").await;
        assert!(
            result.is_err(),
            "a closed port must not read as a live room"
        );
    }

    /// Commands are refused as *unsupported* too, and for the same reason: the console is hidden
    /// entirely under this probe rather than shown greyed out. A visible control that refuses reads
    /// as a bug in Puna; an absent one reads as what it is.
    #[tokio::test]
    async fn commands_are_unsupported_rather_than_broken() {
        let e = TcpProbe
            .execute(
                &endpoint(1),
                "unused",
                &crate::model::command::RoomCommand::Status,
            )
            .await
            .expect_err("must refuse");

        assert!(matches!(e, ProbeError::Unsupported { .. }));
        assert!(!e.is_transient());
    }

    /// Graceful shutdown is refused as *unsupported* rather than as a failure, so a caller hides
    /// the control and falls back rather than reporting an error to somebody.
    #[tokio::test]
    async fn shutdown_is_unsupported_rather_than_broken() {
        let e = TcpProbe
            .request_shutdown(&endpoint(1), "unused", "because")
            .await
            .expect_err("must refuse");

        assert!(matches!(e, ProbeError::Unsupported { .. }));
        assert!(!e.is_transient(), "retrying an unsupported call is a loop");
    }
}
