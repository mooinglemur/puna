//! Turning counts into sentences.
//!
//! One helper, because the alternative is a `match` on the count at every site that reports one and
//! the shape they were all written as instead was `{n} slot(s)`.
//!
//! **`(s)` is not a plural, it is a refusal to pick one**, and it reads as machine output at exactly
//! the moments somebody is being told what just happened to their room. It also cannot fix the verb:
//! `1 slot(s) have no owner` is wrong however the noun is spelled, so the sites that carry one need
//! [`plural`] as well.
//!
//! Templates do not use this. Askama ships `pluralize`, so a template writes
//! `{{ n }} slot{{ n|pluralize }}` and `{{ n|pluralize("is", "are") }}` directly.

/// `1 slot`, `3 slots`, `0 slots` — the count and its noun, agreeing.
///
/// Regular `-s` only. A noun that pluralizes some other way wants [`plural`] with both forms
/// written out, rather than an exception list here that nobody would think to look in.
pub fn count(n: impl Count, singular: &str) -> String {
    if n.is_one() {
        format!("{n} {singular}")
    } else {
        format!("{n} {singular}s")
    }
}

/// Pick the form that agrees with `n`: `plural(n, "has", "have")`, `plural(n, "is", "are")`.
///
/// Takes both words rather than a suffix, so it covers verbs and irregular nouns with one rule.
pub fn plural<'a>(n: impl Count, one: &'a str, many: &'a str) -> &'a str {
    if n.is_one() { one } else { many }
}

/// Implemented for the integer types counts actually arrive as, so a call site does not have to cast.
///
/// **A cast would be the wrong fix**: `len() as i64` is noise at fifteen sites, and `as` on a signed
/// count would wrap a negative into a huge positive rather than failing — which is the class of
/// silent breakage this codebase keeps paying for. Every implementation asks only whether the value
/// is exactly one, which is a question every integer type answers correctly.
pub trait Count: Copy + std::fmt::Display {
    fn is_one(self) -> bool;
}

macro_rules! impl_count {
    ($($t:ty),* $(,)?) => {
        $(impl Count for $t {
            fn is_one(self) -> bool {
                self == 1
            }
        })*
    };
}

impl_count!(usize, u32, u64, i32, i64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_is_singular_and_everything_else_is_not() {
        assert_eq!(count(1usize, "slot"), "1 slot");
        assert_eq!(count(3usize, "slot"), "3 slots");
        assert_eq!(count(0usize, "slot"), "0 slots");

        assert_eq!(plural(1i64, "has", "have"), "has");
        assert_eq!(plural(2i64, "has", "have"), "have");
        assert_eq!(plural(0i64, "is", "are"), "are");
    }

    /// Zero is plural in English — *no slots have an owner*, not *no slot has* — and it is the case
    /// a naive `n > 1` gets wrong. Worth its own assertion because zero is a routine answer here:
    /// every one of these counters starts there.
    #[test]
    fn zero_is_plural() {
        assert_eq!(count(0i32, "generation"), "0 generations");
        assert_eq!(plural(0usize, "was", "were"), "were");
    }

    /// A negative count cannot occur — these come from `len()` and from SQL `COUNT(*)` — but the
    /// answer has to be *defined* rather than wrapped, which is the whole reason `Count` exists
    /// instead of `as u64` at the call sites.
    #[test]
    fn a_negative_count_is_plural_rather_than_wrapping() {
        assert_eq!(count(-1i64, "slot"), "-1 slots");
        assert_eq!(plural(-1i64, "has", "have"), "have");
    }
}
