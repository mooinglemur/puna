//! A room's argv.
//!
//! **The highest-risk surface in the system**, and the reason is pahoa's parser rather than
//! anything Puna does: an unknown *or repeated* option is a hard `exit 1`. So a typo here is a room
//! that never starts, diagnosable only from a container log that says one line and stops. The
//! failure pahoa's own parser exists to prevent is worth restating, because it is the shape this
//! module guards against too — a misspelled `--save-dirr` used to be silently ignored, and started
//! a room that persisted nothing.
//!
//! Three guards, in increasing order of how much they buy:
//!
//!   1. [`PAHOA_SERVE_OPTS`], transcribed from pahoa's own `SERVE_OPTS`. Every name and every
//!      value-or-flag decision is checked against it, so a spelling that pahoa would reject fails
//!      in a unit test instead of in a pod.
//!   2. [`ArgBuilder`] refuses a repeated option, which is the other half of pahoa's `exit 1`.
//!   3. [`NEVER_ARGV`] refuses options that would *work* and be wrong — a password in argv, a
//!      plaintext listener, a gameplay option the room's own save will overrule. Each carries its
//!      reason, so adding one back is a decision with the argument attached rather than an edit.
//!
//! All three panic rather than returning an error, deliberately: every input is a constant chosen in
//! this file, so a failure is a bug in this file and cannot be triggered by data. A `Result` here
//! would have no honest caller — a room whose argv is malformed must not be created, and there is
//! nothing else to do about it.
//!
//! **Aliases are deliberately not transcribed.** Pahoa accepts the reference server's underscored
//! spellings (`--hint_cost`, `--log_format`) for people arriving from it. Puna uses canonical
//! kebab-case only, so a table without the aliases turns a mixed-spelling argv into a test failure.

use crate::cluster::RoomSpec;
use crate::spec::{SAVE_DIR, SEED_PATH, SNAPSHOT_PATH, TLS_CERT_PATH, TLS_KEY_PATH};

/// One option pahoa's `serve` accepts.
pub struct Opt {
    pub name: &'static str,
    /// Whether it consumes a value. Getting this backwards is its own `exit 1`: pahoa refuses a
    /// value on a flag, and a flag given where a value is expected swallows the next token.
    pub takes_value: bool,
}

const fn flag(name: &'static str) -> Opt {
    Opt {
        name,
        takes_value: false,
    }
}

const fn value(name: &'static str) -> Opt {
    Opt {
        name,
        takes_value: true,
    }
}

/// Transcribed from `pahoa/crates/pahoa/src/main.rs`'s `SERVE_OPTS`, in its order.
///
/// A copy, and it has to be: the two repositories deploy independently, so Puna cannot import the
/// list from the binary it is building argv for. Bumping the pinned pahoa image is the moment to
/// re-read that table.
pub const PAHOA_SERVE_OPTS: &[Opt] = &[
    flag("--help"),
    value("--bind"),
    value("--port"),
    value("--snapshot"),
    value("--save-dir"),
    value("--save-interval"),
    value("--outbound-budget"),
    value("--log-level"),
    value("--log-format"),
    value("--filtered-port"),
    value("--tls-cert"),
    value("--tls-key"),
    flag("--allow-plaintext"),
    flag("--open-tracker"),
    flag("--journal"),
    value("--password"),
    value("--server-password"),
    value("--hint-cost"),
    value("--location-check-points"),
    value("--release-mode"),
    value("--collect-mode"),
    value("--remaining-mode"),
    value("--countdown-mode"),
    flag("--no-item-cheat"),
    value("--compatibility"),
    flag("--use-embedded-options"),
];

/// The room's save is authoritative for every gameplay option.
///
/// `save::encode_options` persists them and `Room::restore` takes them from the snapshot, so a flag
/// here describes how a room *started* and is overruled from the first save onward — including by an
/// organizer's live `!admin /option`, which is legitimate. Puna deliberately stores no gameplay
/// option to pass, and a room-settings UI would have to write through to the running room rather
/// than to a column the next restart ignores.
const SAVE_AUTHORITATIVE: &str = "the room's save is authoritative for gameplay options: a flag \
                                  here is overruled from the first save onward, and Puna stores \
                                  none to pass";

/// Options that pahoa accepts, that Puna must never send, with the reason attached.
///
/// Not a test — a refusal in the builder, so the argument has to be answered before the flag can be
/// added rather than after somebody deletes an assertion.
const NEVER_ARGV: &[(&str, &str)] = &[
    (
        "--password",
        "credentials arrive from the room's Secret as environment variables. argv is readable \
         through `kubectl get pod -o yaml` and /proc/<pid>/cmdline",
    ),
    (
        "--server-password",
        "same, and Puna sets no remote-admin gate at all: the console drives the bearer-token API \
         rather than in-game !admin",
    ),
    (
        "--allow-plaintext",
        "pahoa refuses plaintext with 426 once a certificate is configured, and that is the point. \
         This flag would put a mutating, internet-reachable admin API in the clear",
    ),
    (
        "--open-tracker",
        "tracker_policy is Puna's to enforce at its own edge, and every Puna room's tracker is \
         gated because an open one turns a port scan into room identification",
    ),
    ("--hint-cost", SAVE_AUTHORITATIVE),
    ("--location-check-points", SAVE_AUTHORITATIVE),
    ("--release-mode", SAVE_AUTHORITATIVE),
    ("--collect-mode", SAVE_AUTHORITATIVE),
    ("--remaining-mode", SAVE_AUTHORITATIVE),
    ("--countdown-mode", SAVE_AUTHORITATIVE),
    ("--no-item-cheat", SAVE_AUTHORITATIVE),
    ("--compatibility", SAVE_AUTHORITATIVE),
];

/// `[::]`, not `0.0.0.0`.
///
/// Verified in pahoa: it parses, binds `[::]` and accepts v4-mapped IPv4, with a regression test
/// that skips rather than fails where IPv6 is unavailable. The residual dependency is the kernel's
/// `net.ipv6.bindv6only=0` — the Linux default, but a sysctl, and flipping it makes a room silently
/// stop answering IPv4. That belongs in a cluster preflight, not here.
const BIND_ADDRESS: &str = "::";

/// One object per line on stderr, and **no stdout output at all**.
///
/// Pahoa's default is `text`, for a person at a terminal. Every Puna room takes `json`, because a
/// container merges stdout and stderr into one pod log and a prose line inside a JSON stream is one
/// unparseable entry per room forever. The consequence Puna acts on: the startup announcement is an
/// event with `message == "serving"`, not a line on stdout.
const LOG_FORMAT: &str = "json";

/// Builds an argv, refusing everything pahoa would refuse and a few things it would not.
pub struct ArgBuilder {
    args: Vec<String>,
    used: Vec<&'static str>,
}

impl ArgBuilder {
    /// Start an argv with its subcommand, which pahoa consumes before parsing options.
    pub fn new(subcommand: &'static str) -> Self {
        Self {
            args: vec![subcommand.to_string()],
            used: Vec::new(),
        }
    }

    /// Look the option up and record it, panicking on anything pahoa would reject.
    fn claim(&mut self, name: &'static str, takes_value: bool) {
        if let Some((_, reason)) = NEVER_ARGV.iter().find(|(n, _)| *n == name) {
            panic!("{name} must never appear in a room's argv: {reason}");
        }

        let opt = PAHOA_SERVE_OPTS
            .iter()
            .find(|o| o.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "{name} is not an option pahoa's `serve` accepts. An unknown option is exit 1 \
                     -- a room that never starts. Check SERVE_OPTS in the pinned pahoa, and use \
                     the canonical kebab-case spelling rather than a reference-server alias"
                )
            });
        assert_eq!(
            opt.takes_value,
            takes_value,
            "{name} is declared as {} in PAHOA_SERVE_OPTS",
            if opt.takes_value {
                "taking a value"
            } else {
                "a flag"
            }
        );
        assert!(
            !self.used.contains(&name),
            "{name} given more than once, which pahoa treats as a mistake rather than \
             last-one-wins"
        );
        self.used.push(name);
    }

    pub fn flag(&mut self, name: &'static str) -> &mut Self {
        self.claim(name, false);
        self.args.push(name.to_string());
        self
    }

    /// `--name=value` as one token, which pahoa splits on the first `=`.
    pub fn value(&mut self, name: &'static str, value: impl std::fmt::Display) -> &mut Self {
        self.claim(name, true);
        self.args.push(format!("{name}={value}"));
        self
    }

    /// The trailing positional, which for `serve` is the seed.
    ///
    /// Taken by `finish` rather than pushed like the rest, so the seed cannot end up in the middle
    /// of the options where a value-taking flag would swallow it.
    pub fn finish(mut self, positional: &str) -> Vec<String> {
        self.args.push(positional.to_string());
        self.args
    }
}

/// The argv for one room's pod.
///
/// Every path here is a `spec::` constant shared with the Deployment's `volumeMounts`, because the
/// two are the same fact stated twice and a disagreement between them is a room that starts and
/// persists nothing.
pub fn serve(spec: &RoomSpec) -> Vec<String> {
    let mut args = ArgBuilder::new("serve");

    args.value("--bind", BIND_ADDRESS)
        .value("--port", spec.base_port);

    if spec.wants_filtered {
        // The adjacent half of the reserved pair, never allocated separately. Pahoa refuses to
        // start if the two match or either is in use, which makes a misallocation loud.
        let filtered = spec
            .base_port
            .checked_add(1)
            .expect("a base port is at most 49998, per the port_reservations CHECK");
        args.value("--filtered-port", filtered);
    }

    args.value("--save-dir", SAVE_DIR)
        .value("--save-interval", spec.save_interval_secs)
        .value("--snapshot", SNAPSHOT_PATH)
        .value("--tls-cert", TLS_CERT_PATH)
        .value("--tls-key", TLS_KEY_PATH)
        .value("--log-format", LOG_FORMAT)
        // On every room, always. `history.jsonl` in the save directory: one JSON line per check,
        // appended across every restart, which is the organizer-facing answer to "when did each
        // check happen".
        //
        // **Deliberately not the log stream**, and the reason is access rather than durability.
        // Loki has no label-level authorization, so "this organizer reads this room and nothing
        // else" is not something the store can enforce; its retention is a platform setting an
        // async room outlives; and a restarted room is a new pod, so reassembling one room's
        // history from pod logs needs a stable label promoted through the shipper. A file in the
        // room's own directory needs none of that.
        //
        // Two consequences for Puna. It needs `--save-dir`, which is above and always passed. And
        // **it grows monotonically and pahoa never prunes it** -- about 264 bytes per check, so
        // ~6 MB for a 96-slot seed and ~90 MB for a 2000-slot one, on a volume whose quota is
        // shared across every room. This is the file that will find that quota first.
        .flag("--journal");

    if spec.use_embedded_options {
        // Load-bearing rather than cosmetic: precedence is environment → seed → argv, so a seed's
        // own `server_options` can carry gameplay options. Without this the seed's are ignored;
        // with it, pahoa warns when it overrules one and names both values.
        args.flag("--use-embedded-options");
    }

    args.finish(SEED_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use puna_core::ids::RoomId;

    fn spec() -> RoomSpec {
        RoomSpec {
            room_id: RoomId::new(),
            spec_hash: "hash-1".into(),
            image: "registry.example/pahoa:sha-abc123".into(),
            base_port: 40000,
            wants_filtered: true,
            slot_count: 96,
            save_interval_secs: 30,
            use_embedded_options: true,
        }
    }

    /// The whole argv, pinned. Anything that changes it should be a deliberate edit here.
    #[test]
    fn the_rendered_argv() {
        assert_eq!(
            serve(&spec()),
            [
                "serve",
                "--bind=::",
                "--port=40000",
                "--filtered-port=40001",
                "--save-dir=/var/lib/pahoa",
                "--save-interval=30",
                "--snapshot=/shared/datapackage.json",
                "--tls-cert=/etc/pahoa/tls/tls.crt",
                "--tls-key=/etc/pahoa/tls/tls.key",
                "--log-format=json",
                "--journal",
                "--use-embedded-options",
                "/var/lib/pahoa/seed.archipelago",
            ]
        );
    }

    /// The check that makes the transcription worth having: parse the argv the way pahoa's own
    /// parser does, and assert every token is something it would accept.
    #[test]
    fn every_argument_is_one_pahoa_accepts() {
        let argv = serve(&spec());
        assert_eq!(argv.first().map(String::as_str), Some("serve"));

        for token in &argv[1..argv.len() - 1] {
            let (name, inline) = match token.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (token.as_str(), None),
            };
            let opt = PAHOA_SERVE_OPTS
                .iter()
                .find(|o| o.name == name)
                .unwrap_or_else(|| panic!("{token} is not in SERVE_OPTS"));

            assert_eq!(
                opt.takes_value,
                inline.is_some(),
                "{token} disagrees with SERVE_OPTS about taking a value"
            );
            // Pahoa takes the value after `=` verbatim, so an empty one is accepted and wrong.
            assert!(inline != Some(""), "{token} has an empty value");
        }

        // Options are one token each, so the seed is the only bare positional and nothing can
        // swallow it.
        assert_eq!(argv.last().map(String::as_str), Some(SEED_PATH));
    }

    /// No credential is ever argv. The builder enforces it; this states it about the output too.
    #[test]
    fn the_argv_carries_no_credential() {
        let argv = serve(&spec()).join(" ");
        for name in ["--password", "--server-password"] {
            assert!(!argv.contains(name), "{name} in {argv}");
        }
    }

    #[test]
    fn plaintext_and_the_open_tracker_are_never_passed() {
        let argv = serve(&spec()).join(" ");
        assert!(!argv.contains("--allow-plaintext"));
        assert!(!argv.contains("--open-tracker"));
    }

    /// Puna passes no gameplay option at all, and the reason is not that it forgot: after the first
    /// save the room's copy wins, so a flag here would describe a room that no longer exists.
    #[test]
    fn no_gameplay_option_is_passed() {
        let argv = serve(&spec()).join(" ");
        for (name, _) in NEVER_ARGV {
            assert!(!argv.contains(name), "{name} in {argv}");
        }
    }

    /// The room log is what "the room came up" is read from, so the format is not a preference.
    #[test]
    fn the_log_format_is_json() {
        assert!(serve(&spec()).contains(&"--log-format=json".to_string()));
    }

    /// Every room, unconditionally. A room started without it has a history with a hole in it that
    /// nothing can fill in afterwards — the events are gone, not merely unrecorded.
    #[test]
    fn the_journal_is_always_on() {
        let mut spec = spec();
        assert!(serve(&spec).contains(&"--journal".to_string()));

        // Not tied to any room setting: there is no configuration under which a room should keep
        // no history.
        spec.wants_filtered = false;
        spec.use_embedded_options = false;
        assert!(serve(&spec).contains(&"--journal".to_string()));

        // A flag, not a value option -- pahoa refuses a value on it.
        assert!(!serve(&spec).iter().any(|a| a.starts_with("--journal=")));
    }

    /// One statement of a path, used by both the argv and the mounts.
    #[test]
    fn every_path_in_argv_is_under_a_mount() {
        let argv = serve(&spec());
        let value_of = |name: &str| {
            argv.iter()
                .find_map(|a| a.strip_prefix(&format!("{name}=")))
                .unwrap_or_else(|| panic!("{name} missing"))
        };

        assert_eq!(value_of("--save-dir"), SAVE_DIR);
        assert!(
            SEED_PATH.starts_with(SAVE_DIR),
            "the seed lives in the room's own directory"
        );
        assert_eq!(value_of("--snapshot"), SNAPSHOT_PATH);
        assert_eq!(value_of("--tls-cert"), TLS_CERT_PATH);
        assert_eq!(value_of("--tls-key"), TLS_KEY_PATH);
    }

    #[test]
    fn the_filtered_port_is_the_adjacent_one_and_optional() {
        let mut spec = spec();
        spec.base_port = 44998;
        assert!(serve(&spec).contains(&"--filtered-port=44999".to_string()));

        spec.wants_filtered = false;
        let argv = serve(&spec).join(" ");
        assert!(!argv.contains("--filtered-port"));
        // The pair stays reserved either way; only the second listener goes away.
        assert!(argv.contains("--port=44998"));
    }

    #[test]
    fn embedded_options_can_be_turned_off() {
        let mut spec = spec();
        spec.use_embedded_options = false;
        assert!(!serve(&spec).join(" ").contains("--use-embedded-options"));
    }

    #[test]
    #[should_panic(expected = "given more than once")]
    fn a_repeated_option_panics() {
        let mut args = ArgBuilder::new("serve");
        args.value("--port", 40000).value("--port", 40002);
    }

    #[test]
    #[should_panic(expected = "not an option pahoa's `serve` accepts")]
    fn an_unknown_option_panics() {
        ArgBuilder::new("serve").value("--save-dirr", SAVE_DIR);
    }

    /// The reference-server spelling pahoa accepts and Puna does not use.
    #[test]
    #[should_panic(expected = "canonical kebab-case")]
    fn an_alias_spelling_panics() {
        ArgBuilder::new("serve").value("--log_format", "json");
    }

    #[test]
    #[should_panic(expected = "declared as a flag")]
    fn giving_a_flag_a_value_panics() {
        ArgBuilder::new("serve").value("--use-embedded-options", "true");
    }

    #[test]
    #[should_panic(expected = "declared as taking a value")]
    fn giving_a_value_option_no_value_panics() {
        ArgBuilder::new("serve").flag("--port");
    }

    #[test]
    #[should_panic(expected = "argv is readable through")]
    fn a_password_in_argv_panics() {
        ArgBuilder::new("serve").value("--password", "hunter2");
    }

    #[test]
    #[should_panic(expected = "save is authoritative")]
    fn a_gameplay_option_panics_and_says_why() {
        ArgBuilder::new("serve").value("--release-mode", "auto-enabled");
    }

    /// Everything refused is something pahoa would otherwise accept -- a refusal for an option that
    /// does not exist would be a comment pretending to be a guard.
    #[test]
    fn every_forbidden_option_is_a_real_one() {
        for (name, reason) in NEVER_ARGV {
            assert!(
                PAHOA_SERVE_OPTS.iter().any(|o| o.name == *name),
                "{name} is not in SERVE_OPTS, so refusing it guards nothing"
            );
            assert!(!reason.is_empty(), "{name} needs its reason");
        }
    }
}
