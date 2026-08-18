//! Everything both tiers must agree on byte-for-byte.
//!
//! The schema, the id newtypes, the state enums, the port allocator, the metric names and the
//! room-probe contract live here. Filesystem and Kubernetes code deliberately do not: see the
//! dependency note in `Cargo.toml`.

/// Build identity, shared by every binary so `--version` means the same thing everywhere.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Which half of the deployment a process is.
///
/// `puna-web` and `puna-tracker` are the same binary under different roles; the orchestrator is
/// its own binary because it links `kube` and this crate must not.
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

impl std::str::FromStr for Role {
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

/// The environment a process serves. Dev and prod share one public address and therefore one
/// port space, so this value picks the half of the range an allocator may draw from — which is
/// why it is a hard startup input rather than a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Dev,
    Prod,
}

impl Environment {
    /// The inclusive base-port range this environment owns. Base ports are even; each room
    /// reserves `base` and `base + 1` as an adjacent pair.
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

impl std::str::FromStr for Environment {
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
        // Both bounds even, so `base + 1` never collides with the next base.
        for p in [dev_lo, dev_hi, prod_lo, prod_hi] {
            assert_eq!(p % 2, 0, "{p} must be an even base port");
        }
    }

    #[test]
    fn roles_round_trip() {
        for r in [Role::Web, Role::Tracker] {
            assert_eq!(r.as_str().parse::<Role>().unwrap(), r);
        }
        assert!("orchestrator".parse::<Role>().is_err());
    }
}
