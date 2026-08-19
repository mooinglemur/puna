//! Startup configuration, read from the environment.
//!
//! Everything here is a hard input with no default that could plausibly be wrong. The port range
//! in particular: dev and prod share one public address and therefore one port space, and Cilium
//! does not report a collision -- it silently allocates a second IP, leaving a room reachable on
//! an address DNS never mentions. A defaulted environment would be a way to get that wrong
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
    /// The inclusive base-port range this environment owns. Base ports are even; each room
    /// reserves `base` and `base + 1` as an adjacent pair.
    ///
    /// These bounds are duplicated as a CHECK constraint on `port_reservations`. That is
    /// deliberate belt-and-braces on the one mistake in the system that is unrecoverable: a room
    /// allocated outside its environment's half would collide with the other environment on the
    /// shared address, and the symptom is a room nobody can reach rather than an error.
    pub fn port_range(self) -> (u16, u16) {
        match self {
            Environment::Dev => (40000, 44998),
            Environment::Prod => (45000, 49998),
        }
    }

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
    /// The hostname rooms are advertised on, e.g. `mw.ionium-dev.us`.
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
    pub command_timeout: Duration,
    pub trash_retention: Duration,
}

impl OrchestratorConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            common: CommonConfig::from_env()?,
            namespace: require("PUNA_NAMESPACE")?,
            lb_ip: require("PUNA_LB_IP")?,
            lb_sharing_key: optional("PUNA_LB_SHARING_KEY", "ap-lobby-public"),
            pahoa_image: require("PUNA_PAHOA_IMAGE")?,
            room_tls_secret: optional("PUNA_ROOM_TLS_SECRET", "puna-room-tls"),
            data_pvc: optional("PUNA_DATA_PVC", "puna-data"),
            reconcile_interval: parse_duration("PUNA_RECONCILE_INTERVAL", 30)?,
            command_timeout: parse_duration("PUNA_COMMAND_TIMEOUT", 15)?,
            trash_retention: parse_duration("PUNA_TRASH_RETENTION", 7 * 24 * 3600)?,
        })
    }
}

fn require(key: &str) -> anyhow::Result<String> {
    std::env::var(key).map_err(|_| anyhow::anyhow!("{key} must be set"))
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

    #[test]
    fn port_ranges_do_not_overlap() {
        let (dev_lo, dev_hi) = Environment::Dev.port_range();
        let (prod_lo, prod_hi) = Environment::Prod.port_range();
        assert!(
            dev_hi < prod_lo,
            "dev and prod port ranges must be disjoint"
        );
        for p in [dev_lo, dev_hi, prod_lo, prod_hi] {
            assert_eq!(p % 2, 0, "{p} must be an even base port");
        }
    }

    #[test]
    fn port_ranges_leave_room_for_the_pair() {
        // Each reservation covers base and base+1, so the top of a range must not run into the
        // next one even after the +1.
        let (_, dev_hi) = Environment::Dev.port_range();
        let (prod_lo, _) = Environment::Prod.port_range();
        assert!(dev_hi + 1 < prod_lo);
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
