//! The enhanced tracker: what a room's own participants write on its tracker page.
//!
//! Two things, stored differently because they are about different subjects:
//!
//! * **Per slot** — a progression status and a short note, on `room_slots` (see the migration for
//!   why they are columns rather than a table). Written by the slot's owner, and by the room's
//!   staff, who may need to correct or remove what a player left.
//! * **Per person** — how somebody wants to be pinged about this room, in
//!   [`room_ping_preferences`](set_preference). **Only they set it**: staff may edit a note or a
//!   progression, never somebody's stated willingness to be contacted, because that is the one
//!   field here that records what a person agreed to rather than a fact about a world.
//!
//! ## Every reader of this is gated, and this module does not do the gating
//!
//! Nothing here is public. A note, a handle and a ping preference reach the room's staff and the
//! people holding slots in it, and nobody else — an anonymous viewer of a `link`-policy tracker
//! sees the page exactly as it was before any of this existed. That decision is made where the
//! viewer is known, in `routes::tracker`, and the digest is handed a tier rather than working one
//! out. This module answers "what is stored", never "who may see it".

use diesel::sql_types::{BigInt, Nullable, Text, Uuid as SqlUuid};
use diesel_async::{AsyncPgConnection, RunQueryDsl};

use crate::ids::RoomId;

/// How far along a slot is, as its player describes it.
///
/// Community vocabulary rather than anything Archipelago reports: the server knows how many
/// locations are checked and cannot know whether the player is *stuck*, which is the entire
/// question an async organizer is asking. So it is self-reported and may be wrong or stale, and
/// nothing should ever derive from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProgressionStatus {
    /// Nothing said. The default, and rendered as no chip at all rather than as a word — a row
    /// carrying "unknown" beside four that say something real reads as a fifth answer.
    #[default]
    Unknown,
    /// Playing, with things to do.
    Unblocked,
    /// Beat Known: out of checks reachable without receiving something.
    Bk,
    /// Nearly out — a few checks left, all of them awkward. Distinct from `Bk` because it is the
    /// state where a well-aimed release actually helps.
    SoftBk,
    /// Everything needed is in hand; only the finish remains.
    GoMode,
}

impl ProgressionStatus {
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Unblocked => "unblocked",
            Self::Bk => "bk",
            Self::SoftBk => "soft_bk",
            Self::GoMode => "go_mode",
        }
    }

    /// What a page calls it. Separate from [`as_sql`](Self::as_sql) because the wire spelling is a
    /// contract with the database and the label is prose: `soft_bk` is a column value and "Soft BK"
    /// is what somebody reads.
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::Unblocked => "Unblocked",
            Self::Bk => "BK",
            Self::SoftBk => "Soft BK",
            Self::GoMode => "Go mode",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "unknown" => Some(Self::Unknown),
            "unblocked" => Some(Self::Unblocked),
            "bk" => Some(Self::Bk),
            "soft_bk" => Some(Self::SoftBk),
            "go_mode" => Some(Self::GoMode),
            _ => None,
        }
    }

    /// Every value, for rendering a picker and for holding the two spellings together in tests.
    pub const ALL: [Self; 5] = [
        Self::Unknown,
        Self::Unblocked,
        Self::Bk,
        Self::SoftBk,
        Self::GoMode,
    ];
}

/// Whether a participant wants to be contacted about this room, and on what terms.
///
/// **The values are not a scale and must not be rendered as one.** `SeeNotes` is a pointer at the
/// slot's own note, and `ForHints` is narrower than `Yes` in kind rather than in degree — it is
/// consent for one specific reason. Sorting or comparing these would invent an ordering the person
/// did not express.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PingPreference {
    /// Not by other players. **Staff may still ping**, which is why this hides the handle from
    /// participants and not from organizers and helpers — they have the room to run.
    No,
    /// The default, and an honest one: the handle is shown to other players and nothing has been
    /// said about when contact is welcome.
    #[default]
    Unknown,
    /// The terms are in the slot's note. The one value that only means something alongside another
    /// field, which is why a room with this set and no note is worth a nudge in the UI rather than
    /// an error.
    SeeNotes,
    /// Implied consent for one reason: you hold an item another slot needs.
    ForHints,
    /// Any good reason about this multiworld.
    Yes,
}

impl PingPreference {
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::No => "no",
            Self::Unknown => "unknown",
            Self::SeeNotes => "see_notes",
            Self::ForHints => "for_hints",
            Self::Yes => "yes",
        }
    }

    /// The chip's text, or **`None` for `Unknown`**.
    ///
    /// An absent chip is what "nobody has said" looks like. Rendering the word would put a fifth
    /// answer on the page: a row reading "unknown" beside four saying something real looks like a
    /// stated position, when it is the absence of one — and on a young room it would be a column of
    /// them, which is a lot of ink for no information.
    ///
    /// Separate from [`label`](Self::label), which the preferences form still needs: there "Unknown"
    /// is a choice somebody selects and has to be named.
    pub fn chip(self) -> Option<&'static str> {
        match self {
            Self::Unknown => None,
            other => Some(other.label()),
        }
    }

    /// What the form calls it. Every value has one, including `Unknown`.
    pub fn label(self) -> &'static str {
        match self {
            Self::No => "no pings",
            Self::Unknown => "unknown",
            Self::SeeNotes => "see notes",
            Self::ForHints => "hints only",
            Self::Yes => "pings ok",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "no" => Some(Self::No),
            "unknown" => Some(Self::Unknown),
            "see_notes" => Some(Self::SeeNotes),
            "for_hints" => Some(Self::ForHints),
            "yes" => Some(Self::Yes),
            _ => None,
        }
    }

    /// **Whether other PLAYERS get this person's handle.** Staff always do, and ask this of
    /// nobody — the check at the call site is `is_staff || preference.shows_handle_to_players()`.
    ///
    /// The one value this is false for is [`No`](Self::No), which is the only reading of it: the
    /// handle is how somebody gets pinged, so withholding contact means withholding the handle.
    pub fn shows_handle_to_players(self) -> bool {
        self != Self::No
    }

    /// What choosing this means, in the words the form offers it in.
    ///
    /// Beside [`label`](Self::label) rather than instead of it, because they are two different
    /// jobs: the label is a chip in a table cell and has to be two words, and this is the sentence
    /// somebody reads once while deciding. Collapsing them would make the chip a paragraph or the
    /// explanation a fragment.
    pub fn explanation(self) -> &'static str {
        match self {
            Self::No => {
                "Not by other players. Organizers and helpers may still choose to ping you."
            }
            Self::Unknown => {
                "Your handle is shown in the tracker to other players, but you have not told \
                 anyone when and if you would like to be pinged."
            }
            Self::SeeNotes => "Explain your ping preference in your per-slot notes.",
            Self::ForHints => {
                "If you have an item that another slot needs, they have implied consent to ping you."
            }
            Self::Yes => "You do not mind being pinged about this multiworld for any valid reason.",
        }
    }

    pub const ALL: [Self; 5] = [
        Self::No,
        Self::Unknown,
        Self::SeeNotes,
        Self::ForHints,
        Self::Yes,
    ];
}

/// The longest a note may be, in **characters** rather than bytes, so the limit does not depend on
/// the alphabet somebody writes in — the same rule `room::validate_name` follows.
///
/// Enforced by the column as well as by the route: this one is rendered into a panel on a page that
/// polls, so the bound is worth having in the place that cannot be bypassed.
pub const MAX_NOTE_CHARS: usize = 1000;

/// Set or clear one slot's annotations.
///
/// **An empty note is a deletion**, not an empty note: the column refuses `''` outright, so absence
/// is the only way to say nothing and no reader has to treat two values as one. That is also the
/// whole of the delete affordance the UI needs — a player clears the box.
///
/// `actor` is whoever asked, which is the slot's owner or a member of the room's staff. Recorded
/// because staff may edit somebody else's, and "who wrote this" is the first question about a note
/// that says something surprising.
pub async fn set_slot_annotation(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    slot_number: i32,
    progression: ProgressionStatus,
    note: Option<&str>,
    actor: i64,
) -> Result<(), diesel::result::Error> {
    // Trimmed here rather than at the route, so every caller gets the same answer about what counts
    // as empty, and so the column's own CHECK is never the thing that reports a blank note, which
    // would surface as a database error rather than as a deletion.
    let note = note.map(str::trim).filter(|n| !n.is_empty());

    diesel::sql_query(
        "UPDATE room_slots
            SET progression = $3::progression_status,
                note = $4,
                annotated_at = now(),
                annotated_by = $5
          WHERE room_id = $1 AND slot_number = $2",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<diesel::sql_types::Integer, _>(slot_number)
    .bind::<Text, _>(progression.as_sql())
    .bind::<Nullable<Text>, _>(note)
    .bind::<BigInt, _>(actor)
    .execute(conn)
    .await?;

    Ok(())
}

/// One person's ping preference for one room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preference {
    pub user_id: i64,
    pub preference: PingPreference,
}

#[derive(diesel::QueryableByName)]
struct PreferenceRow {
    #[diesel(sql_type = BigInt)]
    user_id: i64,
    #[diesel(sql_type = Text)]
    preference: String,
}

/// Every stated preference in one room.
///
/// **Absent means [`Unknown`](PingPreference::Unknown)**, so a room where nobody has answered
/// stores nothing and this returns nothing. Callers look a slot's owner up in this and fall back to
/// the default rather than expecting a row per participant.
pub async fn preferences(
    conn: &mut AsyncPgConnection,
    room: RoomId,
) -> Result<Vec<Preference>, diesel::result::Error> {
    let rows: Vec<PreferenceRow> = diesel::sql_query(
        "SELECT user_id, preference::text AS preference
           FROM room_ping_preferences WHERE room_id = $1",
    )
    .bind::<SqlUuid, _>(room)
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| Preference {
            user_id: row.user_id,
            // A value this build does not recognize reads as `No`, which withholds the handle. The
            // default is `Unknown`, which publishes it, so unlike every other enum here the
            // fallback and the default are deliberately different values.
            preference: PingPreference::parse(&row.preference).unwrap_or(PingPreference::No),
        })
        .collect())
}

/// Record what one person said about being pinged in one room.
///
/// **Only ever called for the person themselves.** There is no `actor` parameter because there is
/// no case where somebody sets another person's: staff editing this would put words in somebody's
/// mouth about contact they are willing to receive, which is the one thing on this page that is not
/// a fact about a world.
pub async fn set_preference(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    user_id: i64,
    preference: PingPreference,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query(
        "INSERT INTO room_ping_preferences (room_id, user_id, preference)
              VALUES ($1, $2, $3::ping_preference)
         ON CONFLICT (room_id, user_id)
         DO UPDATE SET preference = EXCLUDED.preference, updated_at = now()",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<BigInt, _>(user_id)
    .bind::<Text, _>(preference.as_sql())
    .execute(conn)
    .await?;

    Ok(())
}

/// Carry every stated preference from one room to a clone of it.
///
/// **The preference travels and the annotations do not**, and the split is the subject: a ping
/// preference is a standing statement about a person, which a new room does not change, where a
/// progression and a note describe a playthrough that the clone is starting over. Carrying BK
/// status into a fresh room would have it describe the old one's progress.
///
/// The annotations need no code to be dropped — a clone inserts fresh slots and copies only owners
/// over them — which is precisely why there is a test pinning it. Nothing here would fail if
/// somebody widened that copy.
pub async fn copy_preferences(
    conn: &mut AsyncPgConnection,
    from: RoomId,
    to: RoomId,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query(
        "INSERT INTO room_ping_preferences (room_id, user_id, preference)
              SELECT $2, user_id, preference FROM room_ping_preferences WHERE room_id = $1
         ON CONFLICT (room_id, user_id) DO NOTHING",
    )
    .bind::<SqlUuid, _>(from)
    .bind::<SqlUuid, _>(to)
    .execute(conn)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both spellings of every value, held together.
    ///
    /// `as_sql` is a contract with a Postgres enum and `label` is prose on a page; they drift in
    /// opposite directions — a rename in the database silently stops parsing, and a reworded label
    /// silently changes a column value if the two are ever collapsed into one function.
    #[test]
    fn every_value_round_trips_through_its_wire_spelling() {
        for value in ProgressionStatus::ALL {
            assert_eq!(ProgressionStatus::parse(value.as_sql()), Some(value));
            assert!(!value.label().is_empty());
        }
        for value in PingPreference::ALL {
            assert_eq!(PingPreference::parse(value.as_sql()), Some(value));
            assert!(!value.label().is_empty());
        }
    }

    /// **The default and the fail-safe are different values here**, which is true of nothing else in
    /// this schema and is the thing to get wrong.
    ///
    /// Absent means `Unknown`, which shows the handle to other players — that is the answer for
    /// somebody who has simply not been asked. An *unrecognized* value means `No`, which hides it.
    /// One `unwrap_or_default()` in `preferences` would collapse the two and publish a handle for a
    /// value nobody could read.
    #[test]
    fn an_unreadable_preference_withholds_the_handle_and_the_default_does_not() {
        assert_eq!(PingPreference::default(), PingPreference::Unknown);
        assert!(PingPreference::default().shows_handle_to_players());

        assert_eq!(PingPreference::parse("something_new"), None);
        assert!(!PingPreference::No.shows_handle_to_players());
    }

    /// **An unanswered preference shows no chip, and every answered one does.**
    ///
    /// Absence is what "nobody has said" looks like on the page. Rendering the word would put a
    /// fifth answer beside the four that are real positions — and on a young room it would be a
    /// column of "unknown", which is a lot of ink for the information that nobody has been asked.
    ///
    /// `Unknown` keeps its `label`, because the preferences form offers it as a choice somebody
    /// selects and a radio with no words is not a choice. The two functions differ for exactly one
    /// value, which is why they are two functions.
    #[test]
    fn only_an_unanswered_preference_has_no_chip() {
        let silent: Vec<&str> = PingPreference::ALL
            .into_iter()
            .filter(|p| p.chip().is_none())
            .map(PingPreference::as_sql)
            .collect();
        assert_eq!(silent, ["unknown"]);

        // Every other value's chip is its label, so the two cannot drift into saying different
        // things about the same choice.
        for value in PingPreference::ALL {
            if value == PingPreference::Unknown {
                assert!(!value.label().is_empty(), "the form still needs a word");
            } else {
                assert_eq!(value.chip(), Some(value.label()));
            }
        }
    }

    /// Exactly one value withholds the handle from other players.
    ///
    /// Spelled out rather than derived, because widening this is how a preference somebody set
    /// stops being honored — and the direction that matters is that nothing except `No` may hide a
    /// handle, since the rest are all forms of yes.
    #[test]
    fn only_no_withholds_a_handle() {
        let hidden: Vec<&str> = PingPreference::ALL
            .into_iter()
            .filter(|p| !p.shows_handle_to_players())
            .map(PingPreference::as_sql)
            .collect();
        assert_eq!(hidden, ["no"]);
    }
}
