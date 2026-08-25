//! A protocol-4 pickle **writer**, opposite `pahoa_pickle`'s reader.
//!
//! `MultiData::parse` is a decoder with no encoder beside it, which is why a synthetic seed could
//! not be built before this existed and why a malformed one still cannot be pushed through
//! `artifact::inspect` in a test. This is that missing half, and it is deliberately the smallest
//! one that works.
//!
//! ## The contract is the reader, not the format
//!
//! This does not implement pickle. It implements *the subset `pahoa_pickle` accepts*, which is 33
//! opcodes enumerated in its `reader.rs`, and its correctness condition is a round trip: whatever
//! [`dumps`] writes, `pahoa_pickle::from_slice` must read back as the same [`PyObj`]. That is
//! asserted in this module's own tests against the real reader rather than against a second
//! opinion about what pickle means — which matters, because the reader is the thing that will
//! actually consume the output in production.
//!
//! ## What it does not do, on purpose
//!
//! **No memoization.** Python's pickler emits `MEMOIZE` and back-references so a repeated object
//! is written once; it must, because Python objects can form cycles. A `PyObj` tree cannot, so the
//! writer emits every value inline and never `MEMOIZE`/`BINGET`. The cost is repeated strings —
//! game names once per slot, class references once per instance — and zlib, which every
//! `.archipelago` is wrapped in, removes almost all of it.
//!
//! **No `FRAME`.** It is a buffering hint for the reader and carries no meaning.
//!
//! **`REDUCE` for every instance**, never `NEWOBJ`. Python picks between them by whether the class
//! defines `__getnewargs__` — namedtuples get `NEWOBJ`, by-value enums get `REDUCE` — and the
//! reader collapses both to `PyObj::Instance` (`reader.rs:628`), so reproducing the distinction
//! would be work in service of nothing.

use pahoa_pickle::{ClassId, PyObj};

/// Opcodes this writer emits. A strict subset of the reader's set, named identically so the two
/// can be diffed by eye.
mod op {
    pub const MARK: u8 = b'(';
    pub const STOP: u8 = b'.';
    pub const NONE: u8 = b'N';
    pub const BININT: u8 = b'J';
    pub const BININT1: u8 = b'K';
    pub const BININT2: u8 = b'M';
    pub const BINFLOAT: u8 = b'G';
    pub const BINUNICODE: u8 = b'X';
    pub const EMPTY_LIST: u8 = b']';
    pub const APPENDS: u8 = b'e';
    pub const EMPTY_DICT: u8 = b'}';
    pub const SETITEMS: u8 = b'u';
    pub const EMPTY_TUPLE: u8 = b')';
    pub const TUPLE: u8 = b't';
    pub const REDUCE: u8 = b'R';
    pub const PROTO: u8 = 0x80;
    pub const TUPLE1: u8 = 0x85;
    pub const TUPLE2: u8 = 0x86;
    pub const TUPLE3: u8 = 0x87;
    pub const NEWTRUE: u8 = 0x88;
    pub const NEWFALSE: u8 = 0x89;
    pub const LONG1: u8 = 0x8a;
    pub const SHORT_BINUNICODE: u8 = 0x8c;
    pub const EMPTY_SET: u8 = 0x8f;
    pub const ADDITEMS: u8 = 0x90;
    pub const STACK_GLOBAL: u8 = 0x93;
}

/// The protocol. **Pinned rather than inherited**: Python's `pickle.DEFAULT_PROTOCOL` is 4 today,
/// which is the only reason real seeds parse at all, and a future release moving it to 5 would
/// start emitting opcodes the reader refuses. Writing the number down means this cannot drift.
const PROTOCOL: u8 = 4;

/// Serialize a `PyObj` tree to a protocol-4 pickle stream.
pub fn dumps(value: &PyObj) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(op::PROTO);
    out.push(PROTOCOL);
    write(&mut out, value);
    out.push(op::STOP);
    out
}

fn write(out: &mut Vec<u8>, value: &PyObj) {
    match value {
        PyObj::None => out.push(op::NONE),
        PyObj::Bool(true) => out.push(op::NEWTRUE),
        PyObj::Bool(false) => out.push(op::NEWFALSE),
        PyObj::Int(n) => write_int(out, *n),
        PyObj::Float(f) => {
            out.push(op::BINFLOAT);
            // Big-endian, alone among pickle's numbers: it is an IEEE-754 double in network order,
            // where every integer opcode is little-endian.
            out.extend_from_slice(&f.to_be_bytes());
        }
        PyObj::Str(s) => write_str(out, s),
        PyObj::Big(_) => {
            // Nothing this crate builds needs one. Reachable only by hand-constructing a tree, and
            // silently writing a truncated value would be worse than saying so.
            panic!("puna-tools does not generate integers outside i64");
        }
        PyObj::List(items) => {
            out.push(op::EMPTY_LIST);
            if !items.is_empty() {
                out.push(op::MARK);
                for item in items {
                    write(out, item);
                }
                out.push(op::APPENDS);
            }
        }
        PyObj::Set(items) => {
            out.push(op::EMPTY_SET);
            if !items.is_empty() {
                out.push(op::MARK);
                for item in items {
                    write(out, item);
                }
                out.push(op::ADDITEMS);
            }
        }
        PyObj::Dict(pairs) => {
            out.push(op::EMPTY_DICT);
            if !pairs.is_empty() {
                out.push(op::MARK);
                for (k, v) in pairs {
                    write(out, k);
                    write(out, v);
                }
                out.push(op::SETITEMS);
            }
        }
        PyObj::Tuple(items) => write_tuple(out, items),
        PyObj::Global(class) => write_global(out, class),
        PyObj::Instance { class, args } => {
            write_global(out, class);
            write_tuple(out, args);
            out.push(op::REDUCE);
        }
    }
}

/// `module`, then `name`, then `STACK_GLOBAL` — which pops them in the opposite order
/// (`reader.rs:STACK_GLOBAL`). Getting this backwards produces a class named `NetworkSlot.NetUtils`
/// and an allowlist rejection that reads like a permissions problem.
fn write_global(out: &mut Vec<u8>, class: &ClassId) {
    write_str(out, &class.module);
    write_str(out, &class.name);
    out.push(op::STACK_GLOBAL);
}

/// The short forms exist for arity 1..=3 and are what Python emits; `TUPLE` over a `MARK` is the
/// general case. Both are read identically, so this is purely about producing a stream that looks
/// like the ones in the corpus.
fn write_tuple(out: &mut Vec<u8>, items: &[PyObj]) {
    match items.len() {
        0 => out.push(op::EMPTY_TUPLE),
        1..=3 => {
            for item in items {
                write(out, item);
            }
            out.push(match items.len() {
                1 => op::TUPLE1,
                2 => op::TUPLE2,
                _ => op::TUPLE3,
            });
        }
        _ => {
            out.push(op::MARK);
            for item in items {
                write(out, item);
            }
            out.push(op::TUPLE);
        }
    }
}

/// The narrowest opcode that holds the value.
///
/// `BININT1` and `BININT2` are **unsigned** and `BININT` is signed 32-bit; anything else needs
/// `LONG1`, which carries a little-endian two's-complement body. Location and item ids in a
/// synthetic seed sit above 2^31 on purpose — well outside real Archipelago ranges — so the
/// `LONG1` path is the common one here rather than an edge case, which is why it is tested at the
/// boundaries rather than assumed.
fn write_int(out: &mut Vec<u8>, n: i64) {
    match n {
        0..=0xff => {
            out.push(op::BININT1);
            out.push(n as u8);
        }
        0x100..=0xffff => {
            out.push(op::BININT2);
            out.extend_from_slice(&(n as u16).to_le_bytes());
        }
        _ if i32::try_from(n).is_ok() => {
            out.push(op::BININT);
            out.extend_from_slice(&(n as i32).to_le_bytes());
        }
        _ => {
            out.push(op::LONG1);
            let bytes = long1_body(n);
            out.push(bytes.len() as u8);
            out.extend_from_slice(&bytes);
        }
    }
}

/// The shortest little-endian two's-complement encoding of `n` whose sign bit is correct.
///
/// Truncating one byte too far is the classic bug here: it does not error, it silently flips the
/// sign, and a location id would come back negative from a seed that otherwise looks fine.
fn long1_body(n: i64) -> Vec<u8> {
    let full = n.to_le_bytes();
    let mut len = 8;
    // Drop sign-extension bytes while the remaining top byte still carries the sign.
    let pad = if n < 0 { 0xff } else { 0x00 };
    let sign_ok = |b: u8| if n < 0 { b & 0x80 != 0 } else { b & 0x80 == 0 };
    while len > 1 && full[len - 1] == pad && sign_ok(full[len - 2]) {
        len -= 1;
    }
    full[..len].to_vec()
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    if let Ok(len) = u8::try_from(bytes.len()) {
        out.push(op::SHORT_BINUNICODE);
        out.push(len);
    } else {
        out.push(op::BINUNICODE);
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    }
    out.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pahoa_pickle::Allowlist;

    /// Write it, then read it back with **the reader that will consume it in production**.
    fn round_trip(value: &PyObj) -> PyObj {
        let bytes = dumps(value);
        pahoa_pickle::from_slice(&bytes, &Allowlist::archipelago())
            .unwrap_or_else(|e| panic!("pahoa's reader refused what we wrote: {e}"))
    }

    fn s(v: &str) -> PyObj {
        PyObj::Str(v.into())
    }

    #[test]
    fn every_shape_survives_pahoas_reader() {
        for value in [
            PyObj::None,
            PyObj::Bool(true),
            PyObj::Bool(false),
            PyObj::Float(0.5),
            s(""),
            s("Overworld Blue Goomba"),
            // Past 255 bytes the string opcode changes, and the boundary is where a length prefix
            // written in the wrong width would first show up.
            s(&"x".repeat(255)),
            s(&"x".repeat(256)),
            PyObj::List(vec![]),
            PyObj::List(vec![PyObj::Int(1), s("two")]),
            PyObj::Set(vec![]),
            PyObj::Set(vec![PyObj::Int(3)]),
            PyObj::Dict(vec![]),
            PyObj::Tuple(vec![]),
            PyObj::Tuple(vec![PyObj::Int(1)]),
            PyObj::Tuple(vec![PyObj::Int(1), PyObj::Int(2)]),
            PyObj::Tuple(vec![PyObj::Int(1), PyObj::Int(2), PyObj::Int(3)]),
            // Past three, the short opcodes run out and it becomes MARK + TUPLE.
            PyObj::Tuple(vec![
                PyObj::Int(1),
                PyObj::Int(2),
                PyObj::Int(3),
                PyObj::Int(4),
            ]),
        ] {
            assert_eq!(round_trip(&value), value, "{value:?}");
        }
    }

    /// **Every integer opcode boundary**, because each one is a silent wrong answer rather than an
    /// error: the narrowing is by value, so a seed only exercises the branch its ids happen to
    /// land in, and the ids this tool generates sit above 2^31 where `LONG1` is the only option.
    #[test]
    fn integers_narrow_without_changing_value() {
        for n in [
            0,
            1,
            0xff,
            0x100,
            0xffff,
            0x1_0000,
            i32::MAX as i64,
            i32::MAX as i64 + 1,
            i32::MIN as i64,
            i32::MIN as i64 - 1,
            -1,
            -128,
            -129,
            // The block a synthetic seed actually allocates ids from.
            3_000_000_000,
            3_000_999_999,
            i64::MAX,
            i64::MIN,
        ] {
            assert_eq!(round_trip(&PyObj::Int(n)), PyObj::Int(n), "{n}");
        }
    }

    /// The two class shapes a multidata contains, and the only two the allowlist permits us to
    /// write. `SlotType` nests inside `NetworkSlot`, which is the case that would break if the
    /// writer emitted a class reference without its arguments.
    #[test]
    fn the_multidatas_two_classes_round_trip() {
        let slot_type = PyObj::Instance {
            class: ClassId::new("NetUtils", "SlotType"),
            args: vec![PyObj::Int(1)],
        };
        let slot = PyObj::Instance {
            class: ClassId::new("NetUtils", "NetworkSlot"),
            args: vec![
                s("MooingYacht"),
                s("Yacht Dice Bliss"),
                slot_type,
                PyObj::Tuple(vec![]),
            ],
        };
        assert_eq!(round_trip(&slot), slot);
    }

    /// A class the allowlist forbids must be refused by the READER, not smuggled past it. The
    /// writer is deliberately not the thing enforcing this -- it writes what it is given, and the
    /// gate is the same one a real seed passes through.
    #[test]
    fn a_forbidden_class_is_refused_on_the_way_back_in() {
        let bytes = dumps(&PyObj::Instance {
            class: ClassId::new("os", "system"),
            args: vec![s("rm -rf /")],
        });
        assert!(
            pahoa_pickle::from_slice(&bytes, &Allowlist::archipelago()).is_err(),
            "the allowlist let a forbidden class through"
        );
    }

    /// Nesting, which is what a multidata actually is: dicts of dicts of tuples.
    #[test]
    fn a_nested_structure_survives() {
        let value = PyObj::Dict(vec![
            (
                s("locations"),
                PyObj::Dict(vec![(
                    PyObj::Int(1),
                    PyObj::Dict(vec![(
                        PyObj::Int(3_000_000_501),
                        PyObj::Tuple(vec![
                            PyObj::Int(3_000_000_042),
                            PyObj::Int(2),
                            PyObj::Int(1),
                        ]),
                    )]),
                )]),
            ),
            (
                s("version"),
                PyObj::Tuple(vec![PyObj::Int(0), PyObj::Int(6), PyObj::Int(8)]),
            ),
        ]);
        assert_eq!(round_trip(&value), value);
    }
}
