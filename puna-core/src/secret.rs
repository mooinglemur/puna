//! Generating the credentials Puna hands out.
//!
//! Four kinds, and they differ in who types them rather than in how they are made:
//!
//! | | Where it appears | Shape |
//! |---|---|---|
//! | [`admin_token`] | `PAHOA_ADMIN_TOKEN`, never rendered | 52 chars, unbroken |
//! | [`url_token`] | a claim or invite link | 32 chars, unbroken |
//! | [`slot_password`] | typed into a game client by a player | 9 chars, unbroken |
//! | [`room_password`] | typed into a game client, shared | 15 symbols, dash-grouped |
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
//! mask five bits with no rejection sampling at all. A 9-symbol slot password is **45 bits**, a
//! 15-symbol room password is **75 bits**, and a 32-symbol URL token is **160 bits**.
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

/// One slot's password: nine symbols, unbroken.
///
/// **The third shape this has had, and each step was the same argument.** Fifteen symbols in three
/// groups, then ten in two on 2026-08-21, then nine and no dash at all on 2026-09-06. What is being
/// spent each time is length in a field a player types by hand, off a web page, often on a phone;
/// what it buys is entropy against an endpoint that rate-limits authentication failures to **ten a
/// minute per room**.
///
/// Nine symbols at five bits each is 2^45, about 35 trillion, which at ten guesses a minute is
/// millions of years. The limiting factor has never been the secret, and a slot password is not a
/// platform credential: it keeps a stranger out of somebody's slot in a game.
///
/// **Dropping the dash is the usability half rather than the entropy half.** A grouped password
/// asks a question a player should not have to answer at a login prompt: whether the separator is
/// part of it. `url_token` has been unbroken for the same reason from the start, and its test says
/// so. At nine characters there is nothing left to group.
///
/// **Existing passwords are untouched**, which needs nothing: they live in `room_slots.password`,
/// nothing re-derives them and nothing anywhere validates their shape, so a room already running
/// keeps the credential its players hold. Only what this generates next is new: a claim, a
/// rotation, a room switched into per-slot mode.
///
/// **Nice-to-have, not built:** a deployment-configurable pattern (`PUNA_SLOT_PASSWORD_PATTERN`
/// or similar, defaulting to what this generates) so an operator running a race can ask for more
/// without a code change. Recorded in the plan, and this is the third time the constant has moved
/// without it.
pub fn slot_password() -> String {
    random_string(9)
}

/// A room-wide password. Same shape as a slot's: one person types either.
pub fn room_password() -> String {
    grouped(15, 5)
}

/// `len` random symbols, split into groups of `group`.
fn grouped(len: usize, group: usize) -> String {
    let raw = random_string(len);
    raw.as_bytes()
        .chunks(group)
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

    /// **What a person is asked to type, and what it is worth.**
    ///
    /// A slot password is nine unbroken symbols: nothing to mistype, and no question about whether
    /// a separator is part of it. The room-wide one keeps its groups, because fifteen symbols in a
    /// row is a different reading problem from nine.
    #[test]
    fn a_slot_password_is_nine_symbols_with_nothing_to_mistype() {
        let password = slot_password();
        assert_eq!(password, password.to_lowercase());
        assert_eq!(password.len(), 9, "nine symbols: {password}");
        assert!(
            !password.contains('-'),
            "a slot password carries a separator a player has to guess at: {password}"
        );
        assert!(
            password.bytes().all(|b| ALPHABET.contains(&b)),
            "a symbol outside the confusable-free alphabet: {password}"
        );

        // The entropy claim, asserted rather than left in a comment. 32 symbols is exactly five bits
        // each, so nine of them is 2^45. The bound is stated against the thing that actually limits
        // a guesser: ten authentication failures a minute per room, which is millions of years.
        assert_eq!(ALPHABET.len(), 32);
        let combinations = 2f64.powi(9 * 5);
        assert!(
            combinations / (10.0 * 60.0 * 24.0 * 365.0) > 1e6,
            "a slot password fell to a size ten guesses a minute could work through"
        );

        // The room-wide password is a separate decision and did not move.
        let room = room_password();
        assert_eq!(
            room.len(),
            15 + 2,
            "15 symbols in three dash-separated groups"
        );
        for group in room.split('-') {
            assert_eq!(group.len(), 5);
        }

        // A token in a URL should not carry separators to be mangled by a copy-paste.
        let token = url_token();
        assert_eq!(token.len(), 32);
        assert!(!token.contains('-'));
    }

    /// Not a randomness test: it cannot be, from inside. It catches the failure that actually
    /// happens: a generator wired to a constant seed, or to nothing at all.
    #[test]
    fn generated_secrets_do_not_repeat() {
        let tokens: HashSet<String> = (0..1000).map(|_| url_token()).collect();
        assert_eq!(tokens.len(), 1000);

        let passwords: HashSet<String> = (0..1000).map(|_| slot_password()).collect();
        assert_eq!(passwords.len(), 1000);
    }

    #[test]
    fn every_character_comes_from_the_alphabet() {
        let sample = format!("{}{}{}", admin_token(), url_token(), slot_password());
        for c in sample.chars() {
            assert!(
                c == '-' || ALPHABET.contains(&(c as u8)),
                "{c:?} is not in the alphabet"
            );
        }
    }
}
