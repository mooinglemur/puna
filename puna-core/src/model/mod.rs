//! Domain operations over the schema.

pub mod annotation;
pub mod command;
pub mod event;
pub mod filter;
pub mod fleet;
pub mod generation;
pub mod member;
pub mod names;
pub mod port;
pub mod room;
pub mod settings;
pub mod slot;
pub mod tracker;
pub mod user;

/// Where a room came from.
///
/// Maps to the `room_source` enum, and names the `settings` key holding that source's creation
/// gate -- so adding a third source cannot compile without deciding which switch admits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomSource {
    /// A zip uploaded through Puna's own form.
    Direct,
    /// A generation pushed by Archipelago-lobby. Contract only until M14.
    Lobby,
}

impl RoomSource {
    /// The value as the `room_source` enum spells it.
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Lobby => "lobby",
        }
    }

    /// The `settings` row holding this source's creation gate.
    ///
    /// Two independent switches on purpose: disabling direct uploads should not disable the
    /// lobby's pipeline, and vice versa.
    pub fn settings_key(self) -> &'static str {
        match self {
            Self::Direct => "room_creation.direct",
            Self::Lobby => "room_creation.lobby",
        }
    }
}

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
