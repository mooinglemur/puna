//! Typed UUID newtypes, so ids of different things cannot be swapped.
//!
//! Adapted from `Archipelago-lobby/lobby/src/db/types.rs`. The point is that `RoomId` and
//! `TrackerId` are both UUIDs and must never be interchangeable: `/room/<id>` and
//! `/tracker/<id>` are deliberately different capabilities, and passing one where the other is
//! expected would leak a room URL through a tracker link. A bare `Uuid` everywhere makes that a
//! code-review question; a newtype makes it a compile error.
//!
//! Rocket's `FromParam` and `UriDisplayPath` impls live in the web crate, not here -- puna-core
//! must not depend on rocket (see the note in Cargo.toml).

use std::fmt;
use std::str::FromStr;

use diesel::backend::Backend;
use diesel::deserialize::{self, FromSql};
use diesel::serialize::{self, Output, ToSql};
use diesel::sql_types::Uuid as SqlUuid;
use uuid::Uuid;

/// Define a UUID newtype that round-trips through diesel, serde and strings.
macro_rules! new_id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
            serde::Serialize, serde::Deserialize,
            diesel::AsExpression, diesel::FromSqlRow,
        )]
        #[diesel(sql_type = SqlUuid)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// A fresh random id. uuid4 throughout: these are capability URLs, so unguessability
            /// is the security property, not sortability.
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::from_str(s)?))
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Uuid {
                id.0
            }
        }

        impl<DB> ToSql<SqlUuid, DB> for $name
        where
            DB: Backend,
            Uuid: ToSql<SqlUuid, DB>,
        {
            fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, DB>) -> serialize::Result {
                self.0.to_sql(out)
            }
        }

        impl<DB> FromSql<SqlUuid, DB> for $name
        where
            DB: Backend,
            Uuid: FromSql<SqlUuid, DB>,
        {
            fn from_sql(bytes: DB::RawValue<'_>) -> deserialize::Result<Self> {
                Ok(Self(Uuid::from_sql(bytes)?))
            }
        }
    };
}

new_id_type!(
    /// A room. Appears in `/room/<id>`, which is a player-facing capability URL.
    RoomId
);

new_id_type!(
    /// A generation (one ingested zip). Never appears in a URL: generations are addressed on
    /// disk by their sha256, and in the API by the room that references them.
    GenerationId
);

new_id_type!(
    /// A tracker's URL segment. **Deliberately not derivable from [`RoomId`]** -- that
    /// independence is the whole reason a tracker link can be shared without leaking the room.
    /// Both rooms and individual slots have one, drawn from the same space so a bare
    /// `/tracker/<uuid>` does not disclose which kind it is until it resolves.
    TrackerId
);

new_id_type!(
    /// A feed's URL segment. **Derivable from neither [`RoomId`] nor [`TrackerId`]**, which is what
    /// lets a room's feed be handed to a stream chat without handing over the room or its tracker.
    JournalId
);

new_id_type!(
    /// One console command in flight.
    CommandId
);

new_id_type!(
    /// A set of commands enqueued together by one bulk action.
    ///
    /// **Not a capability.** It appears in a URL an operator is redirected to, but every read of a
    /// batch is scoped to the room the guard already authorized — holding one must not be a way to
    /// read another room's commands, for the same reason a [`CommandId`] is not.
    BatchId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_strings() {
        let id = RoomId::new();
        assert_eq!(id.to_string().parse::<RoomId>().unwrap(), id);
    }

    #[test]
    fn ids_are_unguessable_and_distinct() {
        // uuid4, so two fresh ids must differ. This is the property the tracker's separation
        // rests on, so it is worth an assertion rather than an assumption about the crate.
        assert_ne!(TrackerId::new(), TrackerId::new());
    }

    #[test]
    fn distinct_id_types_do_not_unify() {
        // The compile-time property this module exists for, asserted at runtime as far as it
        // can be: converting between them must go through Uuid explicitly and never implicitly.
        let room = RoomId::new();
        let tracker = TrackerId::from(*room.as_uuid());
        assert_eq!(room.as_uuid(), tracker.as_uuid());
        // `let _: RoomId = tracker;` would not compile, which is the actual guarantee.
    }
}
