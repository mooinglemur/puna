//! Startup configuration, read from the environment.
//!
//! Everything here is a hard input with no default that could plausibly be wrong. The port range
//! in particular: dev and prod share one public address and therefore one port space, and Cilium
//! does not report a collision as an error. It **refuses the room an address entirely** -- every
//! room Service requests a specific IP, and that branch of LB-IPAM answers a conflict with
//! `already_allocated_incompatible_service` and no allocation -- so the room never starts and
//! nothing on Puna's side counts it. A defaulted environment would be a way to get that wrong
//! quietly, so there is no default.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

/// Which half of the deployment a process is.
///
/// `puna-web` and `puna-tracker` are the same binary under different roles; the orchestrator is
/// its own binary because it links `kube` and `puna-core` must not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Rooms, admin, auth, artifact ingest, the console UI.
    Web,
    /// `/tracker/**` and the two proxied JSON endpoints. No PVC, no Discord credentials.
    Tracker,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Web => "web",
            Role::Tracker => "tracker",
        }
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "web" => Ok(Role::Web),
            "tracker" => Ok(Role::Tracker),
            other => Err(format!(
                "unknown PUNA_ROLE {other:?}; expected \"web\" or \"tracker\""
            )),
        }
    }
}

/// The environment a process serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Dev,
    Prod,
}

impl Environment {
    pub fn as_str(self) -> &'static str {
        match self {
            Environment::Dev => "dev",
            Environment::Prod => "prod",
        }
    }
}

impl FromStr for Environment {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "dev" => Ok(Environment::Dev),
            "prod" => Ok(Environment::Prod),
            other => Err(format!(
                "unknown PUNA_ENVIRONMENT {other:?}; expected \"dev\" or \"prod\""
            )),
        }
    }
}

/// Configuration both tiers need.
#[derive(Debug, Clone)]
pub struct CommonConfig {
    pub environment: Environment,
    pub database_url: String,
    /// The hostname rooms are advertised on, e.g. `rooms.example.com`.
    ///
    /// A DNS name rather than the literal VIP, so the address can be re-pointed without
    /// invalidating every bookmarked room -- and it is also the name on the room certificate,
    /// which makes it load-bearing rather than cosmetic.
    pub advertise_host: String,
    /// Root of the shared CephFS volume.
    ///
    /// The two tiers mount DIFFERENT things at this path, by `subPath`, and that is the point:
    /// the web tier gets `generations/` read-write and nothing else, so it physically cannot
    /// reach a room's state directory. The orchestrator gets the volume root.
    pub data_dir: PathBuf,
}

impl CommonConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            environment: parse_env("PUNA_ENVIRONMENT")?,
            database_url: require("DATABASE_URL")?,
            advertise_host: require("PUNA_ADVERTISE_HOST")?,
            data_dir: PathBuf::from(
                std::env::var("PUNA_DATA_DIR").unwrap_or_else(|_| "/var/lib/puna".to_string()),
            ),
        })
    }
}

/// Orchestrator-only configuration.
#[derive(Debug, Clone)]
pub struct OrchestratorConfig {
    pub common: CommonConfig,
    /// Namespace room objects are created in.
    pub namespace: String,
    /// The shared public address every room Service must land on. Read back after Service
    /// creation and asserted, because a mismatch is the silent Cilium failure.
    pub lb_ip: String,
    pub lb_sharing_key: String,
    pub pahoa_image: String,
    /// The Secret holding the room certificate, mounted read-only into every room pod.
    ///
    /// One certificate for the single name every room shares — they differ only by port, so no
    /// wildcard is needed — and pahoa reloads it in place, which is what makes a renewal invisible
    /// to connected players.
    pub room_tls_secret: String,
    /// The CephFS claim holding `generations/`, `rooms/`, `shared/` and `trash/`.
    ///
    /// One PVC per environment, `subPath` per room. Never a PVC per room: the quota is per-claim, so
    /// hundreds of claims would be hundreds of quotas to manage, and CephFS subvolumes are not free.
    pub data_pvc: String,
    pub reconcile_interval: Duration,
    /// How often the loop looks again **while a room is mid-transition**, as opposed to the full
    /// pass above.
    ///
    /// It exists because a restart crosses two passes — one stops the room, one starts it — and at
    /// the full interval that gap is most of a room's downtime. A convergence pass reads the
    /// cluster, plans and applies, and skips everything that is about the fleet rather than about a
    /// room in flight: the probe, the sweep, the filesystem checks.
    ///
    /// **It deliberately does not accelerate a redeploy.** Recreates are emitted only on full
    /// passes, so their pace stays one per [`Self::reconcile_interval`] no matter how often this
    /// fires — a fleet-wide restart rolls at the same speed either way. See `plan::plan`.
    pub converge_interval: Duration,
    /// How long a running room may go without any client speaking before the orchestrator takes it
    /// down. Zero disables the reaper entirely.
    ///
    /// **Measured from the last client MESSAGE, never from the socket count.** One player commonly
    /// holds three connections — game client, text client, tracker — and a tab left open overnight
    /// keeps a socket alive indefinitely while nobody is playing. A reaper counting sockets would
    /// never fire on exactly the rooms it exists for.
    ///
    /// **Idle means nobody has CHECKED A LOCATION, which is what the reference means by it.**
    /// Puna reads pahoa's `activity.last_check_at` (its P23), which moves only on a genuinely new
    /// location check — the same signal the reference server's own `auto_shutdown` uses
    /// (`MultiServer.py:2675`, over `client_activity_timers`). A room full of people idling in chat
    /// reaps; a room where somebody is playing does not.
    ///
    /// Deliberately **not** `activity.idle_seconds`, which moves on any packet at all and would
    /// keep such a room up forever. That number is still recorded and shown, because "somebody is
    /// connected and talking" is worth knowing on its own — it is just not this decision.
    ///
    /// **Floored at how long the room has been up.** pahoa persists the check timer across a
    /// restart, so a room stopped for three days reports three days of check-idle the moment it
    /// returns; compared naively that is a room reaped thirty seconds after somebody started it,
    /// then started again, then reaped again. See `reconcile::idle_since`.
    ///
    /// A reaped room is stopped, not deleted: it keeps its port, its save and its files, and anyone
    /// with the link starts it again. That is the same lifecycle the reference implementation has,
    /// and the reason the whole port-reservation table exists.
    pub idle_timeout: Duration,
    pub command_timeout: Duration,
    pub trash_retention: Duration,
    /// Which probe reaches rooms. `https` is the default because pahoa has shipped the whole admin
    /// surface; `tcp` exists for a room pinned to an image older than that, and under it the
    /// console is hidden entirely rather than shown greyed out.
    pub room_probe: crate::probe::ProbeKind,
    /// How Puna reaches a room: in-cluster by Service name, or out through the public address.
    ///
    /// `service` is the default and the right answer in a cluster — the public route hairpins
    /// in-cluster traffic out to the load balancer and back. Either way TLS is verified against the
    /// advertised host, which is the only name on the room certificate.
    pub room_route: crate::room::Route,
    /// How long a room has to answer a probe.
    ///
    /// Short on purpose: a tick reconciles every room, so a room that has wedged must not be able
    /// to hold the sweep open behind it.
    pub room_probe_timeout: Duration,
    /// How many rooms one tick may restart.
    ///
    /// **One by default, because a redeploy is a real outage for the people in that room** and
    /// nothing else in the apply loop bounds this: it is a sequential pass with no throttle, and a
    /// foreground delete returns as soon as the API server accepts it rather than when the pod is
    /// gone. Uncapped, a fleet-wide redeploy stops every room within a single tick and brings them
    /// all back together — one simultaneous final save and restore per room, onto one shared CephFS
    /// volume.
    ///
    /// Raising it trades that risk for wall-clock. At one per tick a rollout moves at roughly two
    /// rooms a minute, which is the right default for an environment where a room is a game in
    /// progress rather than a stateless replica.
    pub max_recreates_per_tick: usize,
    /// The inclusive port range this environment may allocate from, as `(low, high)` **base**
    /// ports — both even, because each room reserves `base` and `base + 1` as an adjacent pair.
    ///
    /// **From the environment, not from the code.** Which ports a deployment owns is a property of
    /// that deployment's network, not of Puna: the range depends on what else shares the address,
    /// what the firewall permits, and how the space was divided. A constant here would be one
    /// deployment's answer compiled into everyone's binary.
    ///
    /// It stays load-bearing, though, and the reason is worth keeping: where two environments share
    /// one public address they share one port space, and an overlap is the one mistake in this
    /// system that is unrecoverable — the second allocation silently lands on a different address
    /// rather than erroring, leaving a room reachable at a name DNS never mentions. The database
    /// records the configured range per environment and refuses reservations outside it, but it can
    /// only see its own environment. **Non-overlap between environments is the deployment's to get
    /// right**, which is why it belongs beside the addresses rather than in here.
    pub port_range: (u16, u16),
    /// The label and annotation KEYS this cluster uses, and the address-pool value.
    ///
    /// Prefixed keys belong to whoever owns the domain in them, and two of these are matched by
    /// objects outside this repository — an address-pool selector and an L2 announcement policy —
    /// so they are the cluster's vocabulary rather than Puna's and arrive from the deployment.
    ///
    /// **Changing `room_label_key` on a live deployment is not a config change.** It is the
    /// Deployment's `spec.selector`, which Kubernetes will not let you update, and it is what every
    /// object is read back through — so a new value makes the whole fleet unrecognizable at once.
    /// The orchestrator refuses to start rather than let that proceed silently.
    pub room_label_key: String,
    pub lb_pool_label_key: String,
    pub lb_pool_value: String,
    pub spec_hash_annotation: String,
}

impl OrchestratorConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            common: CommonConfig::from_env()?,
            namespace: require("PUNA_NAMESPACE")?,
            lb_ip: require("PUNA_LB_IP")?,
            // Required, not defaulted. A default here would name one deployment's sharing group,
            // so a second deployment that forgot to set it would silently join the first's shared
            // address instead of failing at startup -- and sharing an address is exactly the thing
            // whose failures are hardest to see.
            lb_sharing_key: require("PUNA_LB_SHARING_KEY")?,
            pahoa_image: require("PUNA_PAHOA_IMAGE")?,
            room_tls_secret: optional("PUNA_ROOM_TLS_SECRET", "puna-room-tls"),
            data_pvc: optional("PUNA_DATA_PVC", "puna-data"),
            reconcile_interval: parse_duration("PUNA_RECONCILE_INTERVAL", 30)?,
            converge_interval: parse_duration("PUNA_CONVERGE_INTERVAL", 3)?,
            idle_timeout: parse_duration("PUNA_IDLE_TIMEOUT", 4 * 3600)?,
            command_timeout: parse_duration("PUNA_COMMAND_TIMEOUT", 15)?,
            trash_retention: parse_duration("PUNA_TRASH_RETENTION", 7 * 24 * 3600)?,
            room_probe: optional("PUNA_ROOM_PROBE", "https")
                .parse()
                .map_err(|e| anyhow::anyhow!("{e}"))?,
            room_route: match optional("PUNA_ROOM_ROUTE", "service").as_str() {
                "public" => crate::room::Route::Public,
                _ => crate::room::Route::Service {
                    namespace: require("PUNA_NAMESPACE")?,
                },
            },
            room_probe_timeout: parse_duration("PUNA_ROOM_PROBE_TIMEOUT", 5)?,
            max_recreates_per_tick: parse_count("PUNA_MAX_RECREATES_PER_TICK", 1)?,
            port_range: parse_port_range("PUNA_PORT_RANGE")?,
            room_label_key: require("PUNA_ROOM_LABEL_KEY")?,
            lb_pool_label_key: require("PUNA_LB_POOL_LABEL_KEY")?,
            lb_pool_value: require("PUNA_LB_POOL_VALUE")?,
            spec_hash_annotation: require("PUNA_SPEC_HASH_ANNOTATION")?,
        })
    }
}

fn require(key: &str) -> anyhow::Result<String> {
    std::env::var(key).map_err(|_| anyhow::anyhow!("{key} must be set"))
}

/// `"40000-44999"` — the inclusive range of PORTS the environment owns, returned as the inclusive
/// range of **base** ports.
///
/// Written as ports rather than as bases because that is what an operator reads off a firewall rule
/// or a load balancer, and asking them to subtract one for the filtered half is asking for the
/// off-by-one. So the parser does it: `40000-44999` is 2500 pairs, base ports 40000 through 44998.
///
/// Both ends are checked rather than rounded. A range starting on an odd port or ending on an even
/// one is a typo, and quietly repairing it would hand back a range the operator did not write —
/// which, for the one value where an overlap between environments is unrecoverable, is the wrong
/// kindness.
fn parse_port_range(key: &str) -> anyhow::Result<(u16, u16)> {
    let raw = require(key)?;
    let (low, high) = raw.trim().split_once('-').ok_or_else(|| {
        anyhow::anyhow!("{key} must look like \"40000-44999\" (inclusive), got {raw:?}")
    })?;

    let parse = |s: &str, which| -> anyhow::Result<u16> {
        s.trim()
            .parse::<u16>()
            .map_err(|_| anyhow::anyhow!("{key}'s {which} bound is not a port number: {s:?}"))
    };
    let (low, high) = (parse(low, "lower")?, parse(high, "upper")?);

    anyhow::ensure!(low < high, "{key}: the lower bound must be below the upper");
    anyhow::ensure!(
        low % 2 == 0,
        "{key}: the range must start on an EVEN port -- each room takes a pair, and the lower of \
         the two is the one advertised. Got {low}."
    );
    anyhow::ensure!(
        high % 2 == 1,
        "{key}: the range must end on an ODD port, so it holds whole pairs. Got {high}; did you \
         mean {}?",
        high.saturating_add(1)
    );

    Ok((low, high - 1))
}

fn optional(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn parse_env<T: FromStr<Err = String>>(key: &str) -> anyhow::Result<T> {
    let raw = require(key)?;
    T::from_str(&raw).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Seconds, as a bare integer. Deliberately not a humantime string: these appear in a Deployment
/// manifest where `30` is unambiguous and `30s` invites someone to write `30m` and mean it.
/// A positive whole number, rejecting zero.
///
/// Zero is refused rather than treated as "unlimited" because the two readings are opposite and a
/// typo would pick the dangerous one: a cap of zero that meant unbounded would restart the whole
/// environment at once, which is the exact failure the cap exists to prevent.
fn parse_count(key: &str, default: usize) -> anyhow::Result<usize> {
    match std::env::var(key) {
        Err(_) => Ok(default),
        Ok(raw) => {
            let value: usize = raw
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("{key} must be a whole number, got {raw:?}"))?;
            anyhow::ensure!(value > 0, "{key} must be greater than zero");
            Ok(value)
        }
    }
}

fn parse_duration(key: &str, default_secs: u64) -> anyhow::Result<Duration> {
    match std::env::var(key) {
        Err(_) => Ok(Duration::from_secs(default_secs)),
        Ok(raw) => {
            let secs: u64 = raw.trim().trim_end_matches('s').parse().map_err(|_| {
                anyhow::anyhow!("{key} must be a whole number of seconds, got {raw:?}")
            })?;
            anyhow::ensure!(secs > 0, "{key} must be greater than zero");
            Ok(Duration::from_secs(secs))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The range is written as PORTS and returned as BASE ports, so the upper bound moves by one.
    ///
    /// Getting this backwards would hand out a base port whose `base + 1` sits outside the range —
    /// the filtered half landing in the next environment's space, which is the collision the whole
    /// partition exists to prevent and which nothing downstream would notice.
    #[test]
    fn a_port_range_is_parsed_as_ports_and_returned_as_base_ports() {
        let parse = |value: &str| {
            // SAFETY: single-threaded test, and the variable is read immediately below.
            unsafe { std::env::set_var("PUNA_TEST_RANGE", value) };
            parse_port_range("PUNA_TEST_RANGE")
        };

        let (low, high) = parse("40000-44999").expect("a whole number of pairs");
        assert_eq!((low, high), (40000, 44998), "2500 pairs, top base is 44998");
        assert_eq!(high + 1, 44999, "the pair's upper half is still inside");
    }

    /// Both ends are checked rather than rounded. Quietly repairing a typo would hand back a range
    /// the operator did not write, and for this value an overlap between environments is the one
    /// mistake that cannot be undone.
    #[test]
    fn a_range_that_is_not_whole_pairs_is_refused() {
        let parse = |value: &str| {
            unsafe { std::env::set_var("PUNA_TEST_RANGE2", value) };
            parse_port_range("PUNA_TEST_RANGE2")
        };

        assert!(parse("40001-44999").is_err(), "starts on an odd port");
        assert!(parse("40000-44998").is_err(), "ends mid-pair");
        assert!(parse("44999-40000").is_err(), "inverted");
        assert!(parse("40000").is_err(), "not a range");
        assert!(parse("forty-thousand").is_err(), "not numbers");
    }

    #[test]
    fn roles_round_trip() {
        for r in [Role::Web, Role::Tracker] {
            assert_eq!(r.as_str().parse::<Role>().unwrap(), r);
        }
        assert!("orchestrator".parse::<Role>().is_err());
    }

    #[test]
    fn environments_round_trip() {
        for e in [Environment::Dev, Environment::Prod] {
            assert_eq!(e.as_str().parse::<Environment>().unwrap(), e);
        }
        assert!("staging".parse::<Environment>().is_err());
    }
}
