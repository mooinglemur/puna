//! Generating the credentials Puna hands out.
//!
//! Four kinds, and they differ in who types them rather than in how they are made:
//!
//! | | Where it appears | Shape |
//! |---|---|---|
//! | [`admin_token`] | `PAHOA_ADMIN_TOKEN`, never rendered | 52 chars, unbroken |
//! | [`url_token`] | a claim or invite link | 32 chars, unbroken |
//! | [`slot_password`] | typed into a game client by a player | per the room's [`PasswordComplexity`] |
//! | [`room_password`] | typed into a game client, shared, and the remote-admin one | ditto |
//!
//! **The bottom two are a room's choice and the top two are not**, which is the line the type
//! draws: [`admin_token`] and [`url_token`] are bearer credentials nobody reads aloud, and the
//! first of them has a floor pahoa enforces by refusing to start. Neither takes a complexity.
//!
//! ## The alphabet
//!
//! Crockford base32 minus its check symbols: digits plus uppercase letters with **I, L, O and U
//! removed**. `I`/`1`, `O`/`0` and `L`/`1` are the pairs people mistype when reading a password off
//! a screen into a game client, and `U` is dropped because Crockford drops it. Lowercased for
//! typing comfort, which costs nothing: the alphabet has no case-collisions left once those four
//! are gone.
//!
//! That leaves **32 symbols, so exactly five bits each**, which is also what lets `random_string`
//! mask five bits with no rejection sampling at all. So the three password tiers are **25, 50 and
//! 75 bits**, a 32-symbol URL token is **160 bits**, and a 52-symbol admin token is **260 bits**.
//!
//! (This paragraph read "28 symbols, so 4.807 bits each" and put a slot password at 72 bits until
//! 2026-09-06. Both numbers described an earlier alphabet; the code has masked five bits out of
//! thirty-two since it was written, and `the_alphabet_excludes_the_characters_people_mistype`
//! asserts the count. Corrected while the shape below was changing, since it is the arithmetic that
//! change rests on.)
//!
//! ## Why not a wordlist
//!
//! `quiet-harbor-ledger` reads better, and an early draft of the plan used exactly that. It needs a
//! wordlist long enough to be strong (2048 words for 33 bits across three words) and curated enough
//! that no room hands a player something unfortunate. Dash-grouped base32 gets more entropy per
//! typed character with no list to embed, curate or translate.

use rand::RngCore;

/// Crockford base32's alphabet: no `I`, `L`, `O` or `U`.
const ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// How long a password a room generates, for the three a person types.
///
/// Per room, on `rooms.password_complexity`, and it governs the room-wide password, every slot
/// password and the remote-admin password. **Never [`admin_token`]**: that is a bearer token for a
/// mutating internet-reachable API which nothing renders, and pahoa refuses to start on one under
/// 32 bytes, so even [`Self::High`] would be a room that never comes up. The type makes that
/// structural rather than remembered, since `admin_token` takes no complexity at all.
///
/// **Changing it regenerates nothing.** It decides what the *next* password looks like, so an
/// organizer can tighten a race room without invalidating credentials their players hold, and
/// loosen one to debug a client without re-issuing a roster.
///
/// The alphabet is 32 symbols, so each is exactly five bits and the three tiers are 25, 50 and 75
/// bits. pahoa rate-limits authentication failures to **ten a minute per room**, so the floor is
/// still years of guessing for one slot in one game; what the tiers actually trade is how much a
/// player has to type off a web page on a phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordComplexity {
    /// `a1b2c`. Five symbols and no separator: the shortest thing worth calling a password, for a
    /// room whose players are fighting their client rather than each other.
    Low,
    /// `a1b2c-3d4e5`. The default.
    Medium,
    /// `a1b2c-3d4e5-f6g7h`. What every room-wide password looked like before this was a choice.
    High,
}

impl PasswordComplexity {
    /// Every value, for rendering the control from the enum rather than from a list in markup.
    pub const ALL: [Self; 3] = [Self::Low, Self::Medium, Self::High];

    /// How many symbols, before grouping. Always a multiple of [`Self::GROUP`], so `low` comes out
    /// with no separator at all rather than with a trailing one.
    fn symbols(self) -> usize {
        match self {
            Self::Low => 5,
            Self::Medium => 10,
            Self::High => 15,
        }
    }

    /// Symbols between separators. One constant rather than one per tier: the grouping exists to
    /// make a long string readable, and groups that changed size between tiers would make the three
    /// look like three different kinds of credential.
    const GROUP: usize = 5;

    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    /// What the control says, and an example of what it produces.
    ///
    /// **The example is generated, not written**, so a label cannot describe a shape the generator
    /// stopped producing. That is not hypothetical here: this file's own module doc carried an
    /// entropy figure for an alphabet the code never had.
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Short",
            Self::Medium => "Standard",
            Self::High => "Long",
        }
    }
}

impl Default for PasswordComplexity {
    /// **Medium, and it is the one value that could not preserve every current shape.** Unifying
    /// three credentials under one policy means a room-wide password rotated after this gets ten
    /// symbols where it got fifteen, and a slot password gets ten grouped where the 2026-09-06
    /// hotfix gave nine unbroken. Both are the point; `High` restores the older room-wide shape for
    /// a room that wants it.
    fn default() -> Self {
        Self::Medium
    }
}

/// A room's `PAHOA_ADMIN_TOKEN`.
///
/// Pahoa refuses to start on a token under **32 bytes** and compares in constant time, so this is
/// deliberately longer than that floor rather than exactly at it: the surface it protects is
/// mutating and internet-reachable, and the token is its only control.
pub fn admin_token() -> String {
    random_string(52)
}

/// A claim or invite token, for a URL.
///
/// Same capability class as a room id: unguessable, bearer, and the only thing standing between a
/// link and the thing it grants.
pub fn url_token() -> String {
    random_string(32)
}

/// One slot's password, at the room's chosen complexity.
///
/// **The shape stopped being a constant on 2026-09-07 and became a room option**, which is the end
/// of an argument this file had three times: fifteen symbols, then ten on 2026-08-21, then nine and
/// unbroken on 2026-09-06 as a hotfix while a client was suspected of mangling what it was given.
/// The client turned out to have a different and fixable problem, so the shortening was not needed
/// and is not the default; what it showed is that the right length is a property of the room rather
/// than of the software.
///
/// **Existing passwords are untouched by a policy change**, which needs no mechanism: they live in
/// `room_slots.password`, nothing re-derives them and nothing anywhere validates their shape.
pub fn slot_password(complexity: PasswordComplexity) -> String {
    grouped(complexity)
}

/// A room-wide password, at the room's chosen complexity.
///
/// **Also the remote-admin password** (`rooms.server_password`, pahoa's `!admin login` gate), which
/// is generated by calling this. One person types either, so one shape covers both. It is
/// emphatically not [`admin_token`], which no one types and which pahoa requires to be at least 32
/// bytes.
pub fn room_password(complexity: PasswordComplexity) -> String {
    grouped(complexity)
}

/// `complexity.symbols()` random symbols, in dash-separated groups of five.
///
/// One group produces no separator at all, which is what makes [`PasswordComplexity::Low`] read as
/// `a1b2c` rather than as `a1b2c-`: `join` puts a separator *between* chunks, so a single chunk
/// needs no special case. Asserted, because a length that stopped being a multiple of the group
/// size would silently produce a ragged last group.
fn grouped(complexity: PasswordComplexity) -> String {
    let raw = random_string(complexity.symbols());
    debug_assert_eq!(
        complexity.symbols() % PasswordComplexity::GROUP,
        0,
        "a tier whose length is not a whole number of groups renders a ragged tail"
    );
    raw.as_bytes()
        .chunks(PasswordComplexity::GROUP)
        .map(|chunk| std::str::from_utf8(chunk).expect("ascii"))
        .collect::<Vec<_>>()
        .join("-")
}

/// `len` symbols from a CSPRNG.
///
/// **Masked, not reduced, and it needs no rejection sampling because the alphabet is a power of
/// two.** 32 symbols is five bits, so the low five bits of a random byte are already uniform over
/// it. Reducing modulo a non-power-of-two would bias the first few symbols upward, which is the
/// kind of shortcut that looks harmless in a password generator right up until somebody quantifies
/// it; here there is nothing to trade off, and keeping the alphabet at 32 is what buys that. The
/// inline comment below is the one this repeats.
///
/// (This said "rejection-sampled rather than reduced modulo 28" until 2026-09-06, describing an
/// alphabet the code has never had and an approach it does not take, two lines above the comment
/// that contradicts it.)
fn random_string(len: usize) -> String {
    let mut rng = rand::thread_rng();
    let mut out = String::with_capacity(len);
    let mut buf = [0u8; 64];

    while out.len() < len {
        rng.fill_bytes(&mut buf);
        for byte in buf {
            if out.len() == len {
                break;
            }
            // 256 is not a multiple of 32, but 32 is a power of two, so masking the low 5 bits is
            // uniform with no rejection needed at all.
            let index = (byte & 0b1_1111) as usize;
            out.push(ALPHABET[index] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn the_alphabet_excludes_the_characters_people_mistype() {
        assert_eq!(ALPHABET.len(), 32, "masking 5 bits requires exactly 32");
        for c in ['i', 'l', 'o', 'u'] {
            assert!(
                !ALPHABET.contains(&(c as u8)),
                "{c} is confusable and must not be in the alphabet"
            );
        }
        // Every symbol distinct, or the entropy calculation above is wrong.
        let unique: HashSet<u8> = ALPHABET.iter().copied().collect();
        assert_eq!(unique.len(), ALPHABET.len());
    }

    /// Pahoa refuses to start below 32 bytes, and the failure is a room that never comes up.
    #[test]
    fn an_admin_token_clears_pahoas_floor_with_room_to_spare() {
        let token = admin_token();
        assert!(token.len() >= 32, "{} bytes", token.len());
        assert_eq!(token.len(), 52);
        assert!(token.is_ascii(), "byte length must equal character count");
    }

    /// **The three tiers are exactly the shapes the option promises.**
    ///
    /// The control names them by example, so the examples are what has to hold: `a1b2c`,
    /// `a1b2c-3d4e5`, `a1b2c-3d4e5-f6g7h`. Asserted by rendering each rather than by reading a
    /// constant, which is what catches the ragged-tail case a length that stopped being a whole
    /// number of groups would produce.
    #[test]
    fn each_tier_is_the_shape_its_example_promises() {
        for (tier, groups) in [
            (PasswordComplexity::Low, 1),
            (PasswordComplexity::Medium, 2),
            (PasswordComplexity::High, 3),
        ] {
            for made in [slot_password(tier), room_password(tier)] {
                assert_eq!(made, made.to_lowercase(), "{made}");
                assert_eq!(
                    made.matches('-').count(),
                    groups - 1,
                    "{tier:?} should render {groups} group(s): {made}"
                );
                // **The floor with no separator, which is what `low` is for.** `join` puts a
                // separator between chunks, so one chunk needs no special case, and a trailing dash
                // would be a character a player has to decide about.
                assert!(
                    !made.starts_with('-') && !made.ends_with('-'),
                    "a separator with nothing on one side of it: {made}"
                );
                for group in made.split('-') {
                    assert_eq!(group.len(), 5, "ragged group in {made}");
                }
                assert!(
                    made.bytes().all(|b| b == b'-' || ALPHABET.contains(&b)),
                    "a symbol outside the confusable-free alphabet: {made}"
                );
            }
        }

        // The entropy claim, asserted rather than left in a comment, and stated against the thing
        // that actually limits a guesser: pahoa allows ten authentication failures a minute per
        // room. Even the FLOOR has to be years, or the tier is not a password.
        assert_eq!(ALPHABET.len(), 32, "five bits per symbol");
        let low = 2f64.powi(5 * 5);
        assert!(
            low / (10.0 * 60.0 * 24.0 * 365.0) > 5.0,
            "the shortest tier fell to a size ten guesses a minute could work through in under five \
             years"
        );

        // A token in a URL should not carry separators to be mangled by a copy-paste, and takes no
        // complexity at all: it is not a thing anybody types.
        let token = url_token();
        assert_eq!(token.len(), 32);
        assert!(!token.contains('-'));
    }

    /// **The wire spellings round-trip, and every tier is offered.**
    ///
    /// `as_sql` is a Postgres enum label and `parse` reads it back, so a mismatch is a room whose
    /// stored policy cannot be loaded. `ALL` is what the options form renders from, so a tier
    /// missing from it is one nobody can select while rooms can still hold it.
    #[test]
    fn every_tier_round_trips_through_its_wire_spelling() {
        for tier in PasswordComplexity::ALL {
            assert_eq!(PasswordComplexity::parse(tier.as_sql()), Some(tier));
            assert!(!tier.label().is_empty());
        }
        assert_eq!(PasswordComplexity::ALL.len(), 3);
        assert_eq!(PasswordComplexity::parse("nonsense"), None);
        // The default is a real tier rather than a fourth state, and it is the one the migration's
        // column default agrees with.
        assert_eq!(PasswordComplexity::default(), PasswordComplexity::Medium);
    }

    /// **The admin token is outside the policy, structurally.**
    ///
    /// It takes no complexity, so there is no call site that could pass one. This asserts the
    /// consequence that matters: even the longest tier is far below pahoa's 32-byte floor, so a
    /// version of this that *did* thread the policy through would be every room in the environment
    /// failing to start behind a healthy-looking banner.
    #[test]
    fn no_tier_could_ever_stand_in_for_an_admin_token() {
        for tier in PasswordComplexity::ALL {
            assert!(
                room_password(tier).len() < 32,
                "a tier reached pahoa's admin-token floor, which is the coincidence that would make \
                 putting the token under this policy look survivable"
            );
        }
        assert!(admin_token().len() >= 32);
    }

    /// Not a randomness test: it cannot be, from inside. It catches the failure that actually
    /// happens: a generator wired to a constant seed, or to nothing at all.
    #[test]
    fn generated_secrets_do_not_repeat() {
        let tokens: HashSet<String> = (0..1000).map(|_| url_token()).collect();
        assert_eq!(tokens.len(), 1000);

        let passwords: HashSet<String> = (0..1000)
            .map(|_| slot_password(PasswordComplexity::High))
            .collect();
        assert_eq!(passwords.len(), 1000);
    }

    #[test]
    fn every_character_comes_from_the_alphabet() {
        let sample = format!(
            "{}{}{}",
            admin_token(),
            url_token(),
            slot_password(PasswordComplexity::High).replace('-', "")
        );
        for c in sample.chars() {
            assert!(
                c == '-' || ALPHABET.contains(&(c as u8)),
                "{c:?} is not in the alphabet"
            );
        }
    }
}
