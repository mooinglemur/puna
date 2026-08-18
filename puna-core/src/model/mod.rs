//! Domain operations over the schema.

pub mod port;
pub mod user;

/// Proof that the caller is the orchestrator, required by every function that writes an
/// observed-state column or mutates a port reservation.
///
/// The web tier and the orchestrator share one database credential, so nothing at the SQL layer
/// separates them. This token is what keeps the split honest in code: a web-tier handler cannot
/// call [`port::allocate_pair`] without first constructing one, and constructing one is a
/// deliberate, greppable act rather than an accident.
///
/// M6 REPLACES [`Orchestrator::assume_leader`] with a constructor taking a `LeaderLock` witness,
/// so the token becomes proof that `pg_try_advisory_lock` actually succeeded rather than proof
/// that someone meant well. Until then the guarantee is review-visible, not compile-enforced --
/// stated plainly because a token that looks stronger than it is would be worse than none.
#[derive(Debug, Clone, Copy)]
pub struct Orchestrator(());

impl Orchestrator {
    /// Assert that this process holds the orchestrator leader lock.
    ///
    /// Call this once, in the orchestrator, immediately after acquiring the advisory lock.
    /// Calling it anywhere else -- and in particular anywhere in `puna-web` -- is a bug.
    pub fn assume_leader() -> Self {
        Self(())
    }
}
