//! The real probe: pahoa's admin API over its own TLS.
//!
//! Every field is read defensively — `as_i64`, `as_str`, missing-is-`None` — rather than through a
//! `Deserialize` derive. That is deliberate. A status document is a **diagnostic**, and a strict
//! parse turns "pahoa added a field" or "one counter is null" into a room Puna can no longer see at
//! all. Reading it loosely means a newer room degrades to fewer numbers instead of to none, which is
//! the direction that keeps an operator informed during exactly the incident that added the field.

use chrono::{DateTime, Utc};

use super::{
    ActivityStatus, NetStatus, ProbeCapabilities, ProbeError, RoomProbe, RoomStatus, SaveStatus,
    SlotStatus,
};
use crate::model::command::{CommandOutput, RoomCommand};
use crate::room::{RoomEndpoint, classify};

/// `GET /admin/v1/status` and `POST /admin/v1/shutdown`.
pub struct HttpsProbe;

const STATUS: &str = "/admin/v1/status";
const SHUTDOWN: &str = "/admin/v1/shutdown";
const COMMAND: &str = "/admin/v1/command";
/// Formatted with the slot number; pahoa spells this one per slot rather than taking it in a body.
const SLOT_PASSWORD: &str = "/admin/v1/slots/{slot}/password";

#[async_trait::async_trait]
impl RoomProbe for HttpsProbe {
    async fn status(
        &self,
        endpoint: &RoomEndpoint,
        admin_token: &str,
    ) -> Result<RoomStatus, ProbeError> {
        let response = endpoint
            .client()
            .await?
            .get(endpoint.url(STATUS))
            .bearer_auth(admin_token)
            .send()
            .await
            .map_err(crate::room::RoomError::from)?;

        if let Some(e) = classify(&response) {
            return Err(e.into());
        }

        let document: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ProbeError::Malformed(e.to_string()))?;

        Ok(parse(&document))
    }

    async fn request_shutdown(
        &self,
        endpoint: &RoomEndpoint,
        admin_token: &str,
        reason: &str,
    ) -> Result<(), ProbeError> {
        let response = endpoint
            .client()
            .await?
            .post(endpoint.url(SHUTDOWN))
            .bearer_auth(admin_token)
            .json(&serde_json::json!({ "reason": reason }))
            .send()
            .await
            .map_err(crate::room::RoomError::from)?;

        if let Some(e) = classify(&response) {
            return Err(e.into());
        }

        // **Accepted, not finished.** `202` is the expected answer and the caller now watches for
        // the Deployment to disappear; waiting on this response for completion would wait forever,
        // because quiescing closes the connection that asked.
        Ok(())
    }

    async fn execute(
        &self,
        endpoint: &RoomEndpoint,
        admin_token: &str,
        command: &RoomCommand,
    ) -> Result<CommandOutput, ProbeError> {
        let response = endpoint
            .client()
            .await?
            .post(endpoint.url(COMMAND))
            .bearer_auth(admin_token)
            // Serialized from the typed enum, so the body cannot be a shape pahoa has to reject --
            // which is why a `400` here is a Puna bug rather than a caller's.
            .json(command)
            .send()
            .await
            .map_err(crate::room::RoomError::from)?;

        if let Some(e) = classify(&response) {
            return Err(e.into());
        }

        // **A refusal arrives here, not in the error path.** pahoa answers `200` with `ok: false`
        // for "no such slot", "nobody to kick", "countdown out of range" -- the room understood and
        // said no. Mapping that to an error would invite a retry, and retrying a refusal loops.
        response
            .json()
            .await
            .map_err(|e| ProbeError::Malformed(e.to_string()))
    }

    async fn set_slot_password(
        &self,
        endpoint: &RoomEndpoint,
        admin_token: &str,
        slot: i32,
        password: Option<&str>,
    ) -> Result<(), ProbeError> {
        let path = SLOT_PASSWORD.replace("{slot}", &slot.to_string());
        let response = endpoint
            .client()
            .await?
            .post(endpoint.url(&path))
            .bearer_auth(admin_token)
            // `null` is a lock, not an omission: pahoa reads an absent key the same way, and
            // sending the key explicitly is what makes the intent readable in a packet capture.
            .json(&serde_json::json!({ "password": password }))
            .send()
            .await
            .map_err(crate::room::RoomError::from)?;

        // **A `404` here has two causes and pahoa's message names only one of them.** Its handler
        // answers `there is no slot <n> in this seed` both when the slot genuinely does not exist
        // and when the room is not in per-slot mode at all -- the actor collapses the second into
        // the first (`None => known = false`). So a rotation that arrives after somebody switched
        // the mode reports a missing slot, and sends whoever reads it to look at the seed.
        //
        // The caller checks the mode before queueing, which is what keeps this rare; it is recorded
        // because the message would otherwise be actively misleading in the one case it appears.
        if let Some(e) = classify(&response) {
            return Err(e.into());
        }
        Ok(())
    }

    fn capabilities(&self) -> ProbeCapabilities {
        ProbeCapabilities {
            status: true,
            commands: true,
            graceful_shutdown: true,
        }
    }
}

/// Read what is there, ignore what is not.
///
/// Split from the request so the whole shape is testable from a JSON literal — which is the only
/// way to assert the two states that mean "no data" rather than zero.
pub fn parse(document: &serde_json::Value) -> RoomStatus {
    RoomStatus {
        seed_name: string(document, "seed_name"),
        pahoa_version: string(document, "pahoa_version"),
        api_version: document
            .get("api_version")
            .and_then(serde_json::Value::as_i64),
        started_at: timestamp(document, "started_at"),

        // **`null` is a room that persists nothing**, which pahoa reports explicitly rather than by
        // omitting the key. Absent and null collapse to the same `None` here, and both are honest:
        // neither says "a save that never happens".
        save: document
            .get("save")
            .filter(|v| !v.is_null())
            .map(|save| SaveStatus {
                last_save_at: timestamp(save, "last_save_at"),
                last_save_bytes: number(save, "last_save_bytes"),
                last_save_micros: number(save, "last_save_micros"),
                save_interval_seconds: number(save, "save_interval_seconds"),
                dirty: save.get("dirty").and_then(serde_json::Value::as_bool),
            }),

        net: document
            .get("net")
            .map_or_else(NetStatus::default, |net| NetStatus {
                clients_connected: number(net, "clients_connected"),
                mailbox_depth: number(net, "mailbox_depth"),
                mailbox_peak: number(net, "mailbox_peak"),
                lag_disconnects: number(net, "lag_disconnects"),
                outbound_queued_bytes: number(net, "outbound_queued_bytes"),
                outbound_peak_bytes: number(net, "outbound_peak_bytes"),
                outbound_budget_bytes: number(net, "outbound_budget_bytes"),
                resident_bytes: number(net, "resident_bytes"),
            }),

        activity: document
            .get("activity")
            .map_or_else(ActivityStatus::default, |activity| ActivityStatus {
                last_client_message_at: timestamp(activity, "last_client_message_at"),
                idle_seconds: number(activity, "idle_seconds"),
                last_check_at: timestamp(activity, "last_check_at"),
                check_idle_seconds: number(activity, "check_idle_seconds"),
            }),

        options: document.get("options").filter(|v| !v.is_null()).cloned(),

        slots: document
            .get("slots")
            .and_then(serde_json::Value::as_array)
            .map(|slots| {
                slots
                    .iter()
                    .filter_map(|slot| {
                        // A slot with no number is not a slot. Dropped rather than defaulted to 0,
                        // which is the server's own reserved slot and would be a lie.
                        let number_of = number(slot, "slot")?;
                        Some(SlotStatus {
                            slot: i32::try_from(number_of).ok()?,
                            name: string(slot, "name").unwrap_or_default(),
                            game: string(slot, "game").unwrap_or_default(),
                            connections: number(slot, "connections"),
                            checks: number(slot, "checks"),
                            total_checks: number(slot, "total_checks"),
                            status: string(slot, "status"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn string(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_string)
}

fn number(value: &serde_json::Value, key: &str) -> Option<i64> {
    value.get(key)?.as_i64()
}

/// RFC 3339, which is what this surface uses — unlike the tracker documents, which use RFC 1123
/// because the reference does. Two surfaces, two formats, and mixing them up yields `None` rather
/// than a wrong instant.
fn timestamp(value: &serde_json::Value, key: &str) -> Option<DateTime<Utc>> {
    let raw = value.get(key)?.as_str()?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every request this module builds must carry the token.**
    ///
    /// Found the accidental way: `execute` was written without `.bearer_auth`, and only an
    /// *unused variable* warning caught it. Had the token been used anywhere else in the function
    /// the compiler would have been silent, and the symptom would have been actively misleading —
    /// pahoa answers `404` to an unauthenticated admin call, which Puna reads as "the Secret did
    /// not arrive" and reports as a provisioning fault.
    ///
    /// A source lint rather than a wire test because there is no room to dial here, and the
    /// property is about what the code says rather than what one call did.
    #[test]
    fn every_request_authenticates() {
        let source = include_str!("http.rs");
        // One per request built against this endpoint. `url()` is the only way to name one.
        let requests = source.matches("endpoint.url(").count();
        let authenticated = source.matches(".bearer_auth(admin_token)").count();

        assert!(
            requests > 0,
            "the lint found no requests, so it proves nothing"
        );
        assert_eq!(
            requests, authenticated,
            "a request to a room was built without an admin token: {requests} request(s), \
             {authenticated} authenticated"
        );
    }

    /// A full document, transcribed from pahoa's own `status::document` rather than invented.
    fn document() -> serde_json::Value {
        serde_json::json!({
            "seed_name": "70327325896653383029",
            "pahoa_version": "0.4.0",
            "api_version": 1,
            "started_at": "2026-08-19T18:00:00Z",
            "save": {
                "last_save_at": "2026-08-19T18:30:00Z",
                "last_save_bytes": 4096,
                "last_save_micros": 1234,
                "save_interval_seconds": 30,
                "dirty": false
            },
            "net": {
                "clients_connected": 3,
                "mailbox_depth": 0,
                "mailbox_peak": 12,
                "lag_disconnects": 1,
                "outbound_queued_bytes": 0,
                "outbound_peak_bytes": 8192,
                "outbound_budget_bytes": 1048576,
                "resident_bytes": 73400320,
                "compressions": 44
            },
            "activity": {
                "last_client_message_at": "2026-08-19T18:29:00Z",
                "idle_seconds": 60
            },
            "options": {
                "hint_cost": 10,
                "release_mode": "auto-enabled",
                "item_cheat": true
            },
            "slots": [
                {"slot": 1, "name": "Troy", "game": "A Link to the Past",
                 "connected": true, "connections": 3, "checks": 49, "total_checks": 97,
                 "status": "playing"},
                {"slot": 2, "name": "Alice", "game": "Timespinner",
                 "connected": false, "connections": 0, "checks": 0, "total_checks": 100,
                 "status": "unknown"}
            ]
        })
    }

    #[test]
    fn a_full_document_parses() {
        let status = parse(&document());

        assert_eq!(status.seed_name.as_deref(), Some("70327325896653383029"));
        assert_eq!(status.api_version, Some(1));
        assert_eq!(status.net.clients_connected, Some(3));
        assert_eq!(status.net.resident_bytes, Some(73_400_320));
        assert_eq!(status.activity.idle_seconds, Some(60));
        assert_eq!(
            status.save.as_ref().and_then(|s| s.last_save_bytes),
            Some(4096)
        );
        assert_eq!(status.slots.len(), 2);
        assert_eq!(status.slots[0].status.as_deref(), Some("playing"));

        // Derived, so it cannot disagree with the count beside it.
        assert!(status.slots[0].connected());
        assert!(!status.slots[1].connected());

        // Options are carried through untouched: Puna renders them and owns none of them.
        assert_eq!(
            status.options.as_ref().unwrap()["release_mode"],
            "auto-enabled"
        );
    }

    /// **The two `null`s that are not zero.** A room with no `--save-dir` reports `save: null`, and a
    /// room nobody has spoken to reports a null activity block. Rendering either as a number would
    /// claim something false — "saved 0 bytes", "last spoke in 1970".
    #[test]
    fn absent_data_is_none_and_never_zero() {
        let mut document = document();
        document["save"] = serde_json::Value::Null;
        document["activity"] = serde_json::json!({
            "last_client_message_at": null,
            "idle_seconds": null
        });

        let status = parse(&document);
        assert_eq!(status.save, None, "a room that persists nothing");
        assert_eq!(status.activity.last_client_message_at, None);
        assert_eq!(status.activity.idle_seconds, None, "never, not zero");
    }

    /// A newer pahoa degrades to fewer numbers, not to a room Puna cannot see. This is why the
    /// parse is defensive rather than a `Deserialize` derive.
    #[test]
    fn an_unfamiliar_or_partial_document_still_yields_what_it_can() {
        let sparse = serde_json::json!({
            "seed_name": "abc",
            "something_added_later": {"deeply": "nested"},
            "net": {"clients_connected": 2},
            "slots": [
                {"slot": 1, "name": "Troy", "game": "ALTTP"},
                {"name": "no slot number at all"}
            ]
        });

        let status = parse(&sparse);
        assert_eq!(status.seed_name.as_deref(), Some("abc"));
        assert_eq!(status.net.clients_connected, Some(2));
        assert_eq!(status.net.resident_bytes, None, "absent is unknown");
        assert_eq!(status.save, None);
        assert_eq!(status.started_at, None);

        // A slot with no number is dropped rather than defaulted to 0 -- slot 0 is the server
        // itself, so a default would be an outright lie about who is connected.
        assert_eq!(status.slots.len(), 1);
        assert_eq!(status.slots[0].slot, 1);
        assert_eq!(status.slots[0].connections, None);

        // An empty document is a status with nothing in it, not a panic.
        assert_eq!(parse(&serde_json::json!({})), RoomStatus::default());
    }

    /// This surface is RFC 3339; the tracker documents are RFC 1123. A timestamp in the other
    /// format yields `None` rather than a plausible wrong instant.
    #[test]
    fn timestamps_are_rfc3339_here() {
        let value = serde_json::json!({
            "good": "2026-08-19T18:00:00Z",
            "tracker_style": "Mon, 17 Aug 2026 18:22:09 GMT",
            "nonsense": "yesterday"
        });

        assert!(timestamp(&value, "good").is_some());
        assert_eq!(timestamp(&value, "tracker_style"), None);
        assert_eq!(timestamp(&value, "nonsense"), None);
    }
}
