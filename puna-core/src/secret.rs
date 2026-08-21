//! Generating the credentials Puna hands out.
//!
//! Four kinds, and they differ in who types them rather than in how they are made:
//!
//! | | Where it appears | Shape |
//! |---|---|---|
//! | [`admin_token`] | `PAHOA_ADMIN_TOKEN`, never rendered | 52 chars, unbroken |
//! | [`url_token`] | a claim or invite link | 32 chars, unbroken |
//! | [`slot_password`] | typed into a game client by a player | 15 chars, dash-grouped |
//! | [`room_password`] | typed into a game client, shared | 15 chars, dash-grouped |
//!
//! ## The alphabet
//!
//! Crockford base32 minus its check symbols: digits plus uppercase letters with **I, L, O and U
//! removed**. `I`/`1`, `O`/`0` and `L`/`1` are the pairs people mistype when reading a password off
//! a screen into a game client, and `U` is dropped because Crockford drops it. Lowercased for
//! typing comfort, which costs nothing: the alphabet has no case-collisions left once those four
//! are gone.
//!
//! That is 28 symbols, so 4.807 bits each. A 15-character slot password is **72 bits**, and a
//! 32-character URL token is **153 bits** -- both far past the point where the limiting factor is
//! the server rather than the secret.
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

/// One slot's password, in dash-separated groups of five.
///
/// **Ten symbols, not fifteen**, decided 2026-08-21. The alphabet is 32 characters, so each symbol
/// is exactly five bits: ten of them is 2^50, a little over a quadrillion combinations, against an
/// endpoint that rate-limits authentication failures to ten a minute per room. Guessing one at that
/// rate is not a threat model, and a slot password is not protecting much anyway -- it keeps a
/// stranger out of somebody's slot in a game, it is not a credential for the platform.
///
/// What the five symbols bought was length in a field a player types by hand, having read it off a
/// web page, often on a phone. That is the cost this removes.
///
/// **Nice-to-have, not built:** a deployment-configurable pattern -- `PUNA_SLOT_PASSWORD_PATTERN`
/// or similar, defaulting to what this generates -- so an operator running a race can ask for more
/// without a code change. Recorded in the plan.
pub fn slot_password() -> String {
    grouped(10, 5)
}

/// A room-wide password. Same shape as a slot's -- one person types either.
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
/// Rejection-sampled rather than reduced modulo 28. The bias from `% 28` over a byte is small --
/// the first 4 symbols come up 1.14x as often as the rest -- but it is free to avoid and it is the
/// kind of shortcut that looks harmless in a password generator right up until someone quantifies
/// it.
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

    #[test]
    fn passwords_are_grouped_and_url_tokens_are_not() {
        let password = slot_password();
        assert_eq!(password, password.to_lowercase());
        assert_eq!(password.len(), 10 + 1, "10 symbols plus one dash");
        assert_eq!(password.matches('-').count(), 1);

        // The entropy claim, asserted rather than left in a comment: 32 symbols is five bits each,
        // so ten symbols is 2^50 and the shape above is what carries it.
        let symbols = password.replace('-', "").len() as u32;
        assert_eq!(ALPHABET.len(), 32);
        assert!(
            2f64.powi((symbols * 5) as i32) > 1e15,
            "a slot password fell below a quadrillion combinations"
        );
        for group in password.split('-') {
            assert_eq!(group.len(), 5);
        }

        // A token in a URL should not carry separators to be mangled by a copy-paste.
        let token = url_token();
        assert_eq!(token.len(), 32);
        assert!(!token.contains('-'));
    }

    /// Not a randomness test -- it cannot be, from inside. It catches the failure that actually
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
