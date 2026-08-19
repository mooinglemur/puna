//! Rocket `FromParam` for puna-core's typed ids.
//!
//! The impls live here rather than in `puna-core` because that crate must not depend on rocket --
//! see the note at the top of its `Cargo.toml`. The orphan rule then forces a wrapper: `RoomId`
//! and `FromParam` are both foreign to this crate, so a direct impl is not possible.
//!
//! The wrapper earns its keep anyway. `RoomId` and `TrackerId` are both UUIDs and deliberately
//! different capabilities -- `/room/<id>` reaches the room, `/tracker/<id>` must not be walkable
//! back to it -- so a route that accidentally took the wrong one would leak the room URL through a
//! tracker link. Distinct parameter types make that a compile error rather than a review question.

use puna_core::ids::RoomId;
use rocket::request::FromParam;

macro_rules! id_param {
    ($name:ident, $inner:ty) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $name(pub $inner);

        impl std::ops::Deref for $name {
            type Target = $inner;
            fn deref(&self) -> &$inner {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl<'a> FromParam<'a> for $name {
            type Error = &'a str;

            fn from_param(param: &'a str) -> Result<Self, Self::Error> {
                param.parse::<$inner>().map(Self).map_err(|_| param)
            }
        }
    };
}

id_param!(RoomParam, RoomId);
// `TrackerParam` lands with the tracker tier at M8b, from the same macro. It is deliberately not
// declared ahead of its route: a parameter type nothing accepts is a type nothing can get wrong.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_uuid_parses_and_anything_else_does_not() {
        let id = RoomId::new();
        let parsed = RoomParam::from_param(&id.to_string()).expect("valid");
        assert_eq!(parsed.0, id);

        for bad in ["", "not-a-uuid", "../etc/passwd", "1"] {
            assert!(RoomParam::from_param(bad).is_err(), "{bad:?}");
        }
    }
}
