//! What a room's pod should be, and the fingerprint that says whether it already is.
//!
//! ## The spec hash decides when a room gets bounced, so what it covers is a contract
//!
//! The reconciler compares the hash on a running Deployment against the one the row describes, and
//! a difference means delete-and-recreate — about ten seconds of downtime with clients reconnecting
//! on their own. So every field is a decision about whether a change is worth that, and two of them
//! are the reason this is not simply "hash the manifest":
//!
//!   * **`slot_auth` is covered**, though it moves nothing in the pod spec. The password mode
//!     reaches pahoa through the Secret with `envFrom`, so without folding it in, turning passwords
//!     on would change the Secret and never restart the room that reads it at startup — the room
//!     would stay open while the UI said locked.
//!   * **Per-slot password *values* are not covered.** They can be rotated on a live room over the
//!     admin API, and hashing them would bounce a room every time one player rotated a password.
//!
//! Both fall out of one rule: **the hash covers everything pahoa reads once at startup, and nothing
//! it can be told later.** The room-wide password is covered (pahoa reads `PAHOA_PASSWORD` at
//! startup and has no live setter, deliberately). The admin token is covered (same, and a rotated
//! token that has not reached the pod makes every console call fail with a `404` that reads as an
//! old image). The slot map's *keys* are covered, because a slot added to a per-slot room needs its
//! password in the environment before anyone can use it.
//!
//! **It is deliberately not a hash of the rendered manifest.** That would be deterministic and
//! wrong: a `k8s-openapi` upgrade that reordered one serialization would change every room's hash
//! and recreate every pod in the cluster, for nothing. The canonical string below is Puna's own, so
//! only Puna's own decisions move it.

use puna_core::hash::sha256_hex;
use puna_core::ids::RoomId;
use puna_core::model::room::SlotAuth;

use crate::cluster::RoomSpec;
use crate::spec::secret::SecretData;

/// The canonical form's version.
///
/// Bumping it recreates every room on the next tick, which is occasionally the right thing and never
/// an accident: a change to what the fingerprint *means* has to be distinguishable from a change to
/// what it is fingerprinting.
const CANONICAL_VERSION: &str = "puna/room-spec/1";

/// Everything a room's pod is, before it is fingerprinted.
///
/// Deliberately not a `RoomSpec` with a placeholder hash. A struct whose hash is a lie for the
/// duration of a function call is a struct that eventually escapes one — and `spec_hash` is compared
/// against a live Deployment, so a wrong value there is a room that either never converges or gets
/// recreated on every tick.
#[derive(Debug, Clone)]
pub struct Draft {
    pub room_id: RoomId,
    pub image: String,
    pub base_port: u16,
    pub wants_filtered: bool,
    /// Every slot in the multidata, **groups included** — this sizes the memory request, and pahoa
    /// derives its outbound budget from `slot_info.len()`, so the connectable count under-requests.
    pub slot_count: i32,
    pub save_interval_secs: i32,
    pub use_embedded_options: bool,
}

impl Draft {
    /// Fingerprint the draft and hand back the spec the cluster is asked for.
    ///
    /// `env` is the room's whole environment, as `spec::secret::build` produced it. Passing it in
    /// rather than the room row is what keeps the exclusion rule honest: the only thing this can
    /// exclude is a value it was given, so a new key added to the Secret is covered by default and
    /// leaving it out has to be written down.
    pub fn build(self, slot_auth: SlotAuth, env: &SecretData) -> RoomSpec {
        let spec_hash = sha256_hex(self.canonical(slot_auth, env).as_bytes());
        RoomSpec {
            room_id: self.room_id,
            spec_hash,
            image: self.image,
            base_port: self.base_port,
            wants_filtered: self.wants_filtered,
            slot_count: self.slot_count,
            save_interval_secs: self.save_interval_secs,
            use_embedded_options: self.use_embedded_options,
        }
    }

    /// The exact bytes that get hashed. One `key=value` per line, fixed order.
    ///
    /// The room's id is **not** in it: two rooms with identical settings should hash the same, and
    /// the hash's job is to answer "is this pod the pod this room's row describes", which is asked
    /// per room already.
    fn canonical(&self, slot_auth: SlotAuth, env: &SecretData) -> String {
        let mut out = String::with_capacity(512);
        out.push_str(CANONICAL_VERSION);
        out.push('\n');

        for line in [
            format!("image={}", self.image),
            format!("base_port={}", self.base_port),
            format!("filtered={}", self.wants_filtered),
            format!("slot_count={}", self.slot_count),
            format!("save_interval={}", self.save_interval_secs),
            format!("use_embedded_options={}", self.use_embedded_options),
            format!("slot_auth={}", slot_auth.as_sql()),
        ] {
            out.push_str(&line);
            out.push('\n');
        }

        // `SecretData` is a BTreeMap, so this walks in key order without sorting -- which is also
        // why it is a BTreeMap rather than a HashMap.
        for (key, value) in env {
            match key.as_str() {
                // The one exclusion, and the reason live rotation is live. The KEYS still count: a
                // slot added to a per-slot room cannot connect until its password is in the pod's
                // environment, so the map's shape has to be able to move the hash even though its
                // contents must not.
                "PAHOA_SLOT_PASSWORDS" => {
                    out.push_str("env=PAHOA_SLOT_PASSWORDS=slots:");
                    out.push_str(&slot_numbers(value).join(","));
                    out.push('\n');
                }
                _ => {
                    out.push_str("env=");
                    out.push_str(key);
                    out.push('=');
                    out.push_str(value);
                    out.push('\n');
                }
            }
        }

        out
    }
}

/// The slot numbers a `PAHOA_SLOT_PASSWORDS` map covers, in order, without its values.
///
/// A parse failure yields no numbers rather than an error: this is a fingerprint input, and the
/// Secret builder has already refused every shape that could get here malformed. Returning the raw
/// string instead would fold the passwords back into the hash, which is the one thing this function
/// exists to prevent.
fn slot_numbers(json: &str) -> Vec<String> {
    match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json) {
        Ok(map) => {
            let mut keys: Vec<String> = map.into_iter().map(|(k, _)| k).collect();
            // Numeric where possible, so "10" sorts after "9" -- the ordering only has to be
            // stable, but one that reads correctly is easier to eyeball in a diff.
            keys.sort_by_key(|k| (k.parse::<i64>().unwrap_or(i64::MAX), k.clone()));
            keys
        }
        Err(_) => Vec::new(),
    }
}

/// Pahoa's own sizing heuristic, transcribed from `config::outbound_budget_for`.
///
/// `max(64 MiB, slots × 3 × 96 KiB)`: three connections per slot, because one player commonly holds
/// a game client, a text client and a tracker, at 96 KiB of headroom each. A 2000-slot room lands at
/// 562.5 MiB, which is the number pahoa's own help text quotes.
pub fn outbound_budget_bytes(slot_count: i32) -> i64 {
    const PER_CONNECTION: i64 = 96 * 1024;
    const CONNECTIONS_PER_SLOT: i64 = 3;
    const FLOOR: i64 = 64 * 1024 * 1024;

    let slots = i64::from(slot_count.max(0));
    slots
        .saturating_mul(CONNECTIONS_PER_SLOT)
        .saturating_mul(PER_CONNECTION)
        .max(FLOOR)
}

/// Headroom over the outbound budget for everything else in the process: the save in memory, the
/// data package, the allocator's slack.
const MEMORY_REQUEST_OVERHEAD: i64 = 192 * 1024 * 1024;
/// The limit's own overhead, on top of half again the budget. A room that reaches its outbound cap
/// is a room under load, and being OOM-killed at exactly that moment loses play.
const MEMORY_LIMIT_OVERHEAD: i64 = 256 * 1024 * 1024;

/// What to request, in bytes. Replace with measurement once `/admin/v1/status` reports
/// `resident_bytes` — the endpoint exists for questions like this one.
pub fn memory_request_bytes(slot_count: i32) -> i64 {
    outbound_budget_bytes(slot_count) + MEMORY_REQUEST_OVERHEAD
}

pub fn memory_limit_bytes(slot_count: i32) -> i64 {
    outbound_budget_bytes(slot_count) * 3 / 2 + MEMORY_LIMIT_OVERHEAD
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Draft {
        Draft {
            room_id: RoomId::new(),
            image: "registry.example/pahoa:sha-abc123".into(),
            base_port: 40000,
            wants_filtered: true,
            slot_count: 96,
            save_interval_secs: 30,
            use_embedded_options: true,
        }
    }

    fn env(pairs: &[(&str, &str)]) -> SecretData {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn token_only() -> SecretData {
        env(&[("PAHOA_ADMIN_TOKEN", &"a".repeat(52))])
    }

    fn hash(draft: &Draft, slot_auth: SlotAuth, env: &SecretData) -> String {
        draft.clone().build(slot_auth, env).spec_hash
    }

    #[test]
    fn the_same_inputs_always_hash_the_same() {
        let draft = draft();
        assert_eq!(
            hash(&draft, SlotAuth::None, &token_only()),
            hash(&draft, SlotAuth::None, &token_only())
        );
        // Two rooms with identical settings agree, because the id is deliberately not an input.
        let mut other = draft.clone();
        other.room_id = RoomId::new();
        assert_eq!(
            hash(&draft, SlotAuth::None, &token_only()),
            hash(&other, SlotAuth::None, &token_only())
        );
    }

    /// Pins the canonical form. A change here is a change to what every existing room's annotation
    /// means, so it should cost a deliberate edit -- and `CANONICAL_VERSION` is the honest way to
    /// make one.
    #[test]
    fn the_canonical_form_is_pinned() {
        let draft = Draft {
            room_id: RoomId::new(),
            image: "pahoa:test".into(),
            base_port: 40000,
            wants_filtered: true,
            slot_count: 4,
            save_interval_secs: 30,
            use_embedded_options: true,
        };
        let env = env(&[("PAHOA_ADMIN_TOKEN", "token")]);
        assert_eq!(
            draft.canonical(SlotAuth::None, &env),
            "puna/room-spec/1\n\
             image=pahoa:test\n\
             base_port=40000\n\
             filtered=true\n\
             slot_count=4\n\
             save_interval=30\n\
             use_embedded_options=true\n\
             slot_auth=none\n\
             env=PAHOA_ADMIN_TOKEN=token\n"
        );
    }

    /// Every field of the pod spec has to move it, or a change to that field never reaches a
    /// running room.
    #[test]
    fn every_spec_field_moves_the_hash() {
        let base = hash(&draft(), SlotAuth::None, &token_only());

        /// A named change to one field, so the failure message says which field went unhashed.
        type Mutation = (&'static str, Box<dyn Fn(&mut Draft)>);

        let mutations: Vec<Mutation> = vec![
            (
                "image",
                Box::new(|d: &mut Draft| d.image = "pahoa:next".into()),
            ),
            ("base_port", Box::new(|d: &mut Draft| d.base_port = 40002)),
            (
                "wants_filtered",
                Box::new(|d: &mut Draft| d.wants_filtered = false),
            ),
            ("slot_count", Box::new(|d: &mut Draft| d.slot_count = 97)),
            (
                "save_interval",
                Box::new(|d: &mut Draft| d.save_interval_secs = 60),
            ),
            (
                "use_embedded_options",
                Box::new(|d: &mut Draft| d.use_embedded_options = false),
            ),
        ];

        for (field, mutate) in mutations {
            let mut draft = draft();
            mutate(&mut draft);
            assert_ne!(
                hash(&draft, SlotAuth::None, &token_only()),
                base,
                "changing {field} must recreate the pod"
            );
        }
    }

    /// The mode moves nothing in the manifest, which is exactly why it has to be in the hash: it
    /// arrives through the Secret, and pahoa reads it once at startup.
    #[test]
    fn the_password_mode_moves_the_hash_though_the_manifest_is_identical() {
        let draft = draft();
        let none = hash(&draft, SlotAuth::None, &token_only());

        let room_mode = hash(
            &draft,
            SlotAuth::Room,
            &env(&[
                ("PAHOA_ADMIN_TOKEN", "t"),
                ("PAHOA_PASSWORD", "open-sesame"),
            ]),
        );
        let per_slot = hash(
            &draft,
            SlotAuth::PerSlot,
            &env(&[
                ("PAHOA_ADMIN_TOKEN", "t"),
                ("PAHOA_SLOT_PASSWORDS", r#"{"1":"a","2":"b"}"#),
            ]),
        );

        assert_ne!(none, room_mode);
        assert_ne!(none, per_slot);
        assert_ne!(room_mode, per_slot);
    }

    /// The one exclusion, and the whole reason `POST /admin/v1/slots/<n>/password` is worth having:
    /// rotating one player's password must not bounce everyone else's room.
    #[test]
    fn rotating_a_slot_password_does_not_move_the_hash() {
        let draft = draft();
        let before = hash(
            &draft,
            SlotAuth::PerSlot,
            &env(&[
                ("PAHOA_ADMIN_TOKEN", "t"),
                ("PAHOA_SLOT_PASSWORDS", r#"{"1":"old","2":"b"}"#),
            ]),
        );
        let after = hash(
            &draft,
            SlotAuth::PerSlot,
            &env(&[
                ("PAHOA_ADMIN_TOKEN", "t"),
                ("PAHOA_SLOT_PASSWORDS", r#"{"1":"new","2":"b"}"#),
            ]),
        );
        assert_eq!(before, after);
    }

    /// ...but the map's shape does move it. A slot with no entry is refused under the fail-closed
    /// rule, so its password has to reach the pod, and only a restart does that.
    #[test]
    fn adding_a_slot_to_a_per_slot_room_moves_the_hash() {
        let draft = draft();
        let two = hash(
            &draft,
            SlotAuth::PerSlot,
            &env(&[
                ("PAHOA_ADMIN_TOKEN", "t"),
                ("PAHOA_SLOT_PASSWORDS", r#"{"1":"a","2":"b"}"#),
            ]),
        );
        let three = hash(
            &draft,
            SlotAuth::PerSlot,
            &env(&[
                ("PAHOA_ADMIN_TOKEN", "t"),
                ("PAHOA_SLOT_PASSWORDS", r#"{"1":"a","2":"b","3":"c"}"#),
            ]),
        );
        assert_ne!(two, three);

        // The map's own key order must not matter -- it is a JSON object, and only its membership
        // is a fact about the room.
        let reordered = hash(
            &draft,
            SlotAuth::PerSlot,
            &env(&[
                ("PAHOA_ADMIN_TOKEN", "t"),
                ("PAHOA_SLOT_PASSWORDS", r#"{"2":"b","1":"a"}"#),
            ]),
        );
        assert_eq!(two, reordered);
    }

    /// Startup-only values are covered, all of them.
    ///
    /// A rotated admin token that has not reached the pod makes every console call fail with a
    /// `404`, which reads as "this room is running an old image" -- the most confusing possible
    /// symptom for the most routine possible operation.
    #[test]
    fn every_startup_only_credential_moves_the_hash() {
        let draft = draft();

        let old_token = hash(
            &draft,
            SlotAuth::None,
            &env(&[("PAHOA_ADMIN_TOKEN", "old")]),
        );
        let new_token = hash(
            &draft,
            SlotAuth::None,
            &env(&[("PAHOA_ADMIN_TOKEN", "new")]),
        );
        assert_ne!(
            old_token, new_token,
            "rotating the admin token needs a restart"
        );

        let before = hash(
            &draft,
            SlotAuth::Room,
            &env(&[("PAHOA_ADMIN_TOKEN", "t"), ("PAHOA_PASSWORD", "old")]),
        );
        let after = hash(
            &draft,
            SlotAuth::Room,
            &env(&[("PAHOA_ADMIN_TOKEN", "t"), ("PAHOA_PASSWORD", "new")]),
        );
        assert_ne!(
            before, after,
            "pahoa has no live password setter, deliberately, so this is a restart"
        );

        // And a key nobody thought about is covered by default: the exclusion is a list of one.
        let with_server_password = hash(
            &draft,
            SlotAuth::None,
            &env(&[("PAHOA_ADMIN_TOKEN", "t"), ("PAHOA_SERVER_PASSWORD", "s")]),
        );
        let without = hash(&draft, SlotAuth::None, &env(&[("PAHOA_ADMIN_TOKEN", "t")]));
        assert_ne!(with_server_password, without);
    }

    /// A malformed map must not fall back to hashing the passwords themselves.
    #[test]
    fn an_unparseable_slot_map_contributes_no_values() {
        assert_eq!(slot_numbers("not json"), Vec::<String>::new());
        assert_eq!(
            slot_numbers(r#"{"1":"a","10":"b","2":"c"}"#),
            ["1", "2", "10"]
        );
    }

    /// Pahoa's own numbers, so a drift in either direction is visible here.
    #[test]
    fn the_memory_budget_matches_pahoas_heuristic() {
        // The floor: a small room does not get a cap so low it binds during ordinary play.
        assert_eq!(outbound_budget_bytes(1), 64 * 1024 * 1024);
        assert_eq!(outbound_budget_bytes(0), 64 * 1024 * 1024);
        // 228 slots is where three connections at 96 KiB each first passes the floor; at 227 the
        // formula is still below it and the floor is what binds.
        assert_eq!(outbound_budget_bytes(227), 64 * 1024 * 1024);
        assert_eq!(outbound_budget_bytes(228), 228 * 3 * 96 * 1024);
        // The number pahoa's own help text quotes for a 2000-slot room.
        assert_eq!(outbound_budget_bytes(2000), 562 * 1024 * 1024 + 512 * 1024);

        // A negative slot count cannot come from the database, but it must not become a huge
        // request if it ever does.
        assert_eq!(outbound_budget_bytes(-5), 64 * 1024 * 1024);
    }

    #[test]
    fn the_limit_leaves_headroom_over_the_request() {
        for slots in [1, 96, 2000] {
            let request = memory_request_bytes(slots);
            let limit = memory_limit_bytes(slots);
            assert!(
                limit > request,
                "{slots} slots: {limit} must exceed {request}"
            );
            assert!(
                request > outbound_budget_bytes(slots),
                "the request has to cover more than the outbound budget alone"
            );
        }
    }
}
