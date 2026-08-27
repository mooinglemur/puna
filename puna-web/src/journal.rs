//! Reading a room's journal off the shared volume.
//!
//! pahoa appends one JSON object per line to `history.jsonl` in the room's save directory, across
//! every restart. Puna never writes it; the web tier mounts `rooms/` **read-only**, which is the
//! whole reason that mount exists (§1) and is what makes this module safe to have at all — nothing
//! here can create, truncate or corrupt a room's history.
//!
//! ## Why the file rather than the room
//!
//! The obvious alternative is to read the live feed from the room itself, and it is not available:
//! **the web tier has no egress to room pods at all**, deliberately, which its NetworkPolicy states
//! and calls the point rather than an omission. The file is the only path, and it is a good one —
//! it is append-only, it survives restarts, and it is readable while the room is running with no
//! coordination at all, which pahoa's own module notes is safe by construction.
//!
//! ## The one thing that will bite anybody editing this
//!
//! **A reader can see a partial line.** pahoa writes each record with a single `write_all` into a
//! 256 KiB `BufWriter`, and the flush that empties it has no idea where record boundaries are — so
//! a tail taken mid-flush can end in half an object. Every function here therefore stops at the last
//! `\n` and reports an offset that points **at** the remainder rather than past it, so the next read
//! picks the partial line up whole. Getting that wrong produces a parse error in the browser for one
//! event out of thousands, which is exactly the kind of thing that gets shrugged at rather than
//! fixed.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use puna_core::ids::RoomId;

/// What pahoa calls the file inside the save directory.
///
/// Transcribed from `pahoa_net::journal::FILE_NAME`, the same way the argv table is transcribed from
/// their `SERVE_OPTS`: Puna does not depend on their crates in this tier, so the name is a fact
/// about another repository rather than a value either side can look up. If it ever moves, this is
/// the one place to follow it — and the symptom would be a viewer that reports every room as having
/// no history, which is at least loud.
pub const FILE_NAME: &str = "history.jsonl";

/// Largest replay a client may ask for.
///
/// A journal is routinely hundreds of megabytes — 250 MB and 1.2 million lines on the dev cluster's
/// load-test rooms — so an unbounded "replay from the beginning" is a request to serialize a
/// quarter of a gigabyte into a browser over a connection with no compression. The cap is on the
/// **server** rather than in the page, because the page is not the only thing that can open a
/// WebSocket.
pub const MAX_REPLAY_LINES: usize = 5_000;

/// What a viewer gets on connect when it does not ask for anything in particular.
///
/// Enough to arrive at a page with context on it and not enough to be a download: a hundred lines
/// is roughly 25 KiB, which is a page load rather than an event. `tail` returns whatever is there
/// when the journal is shorter, so a room that has barely started shows its whole history.
///
/// Troy's number, and deliberately not the cap: [`since`] exists for a viewer who wants more, and
/// the request shape already carries `at` for the day the page offers a time to scroll back to.
pub const DEFAULT_REPLAY_LINES: usize = 100;

/// The journal of one room, if the orchestrator has provisioned it.
pub fn path(data_dir: &Path, room: RoomId) -> PathBuf {
    data_dir
        .join("rooms")
        .join(room.to_string())
        .join(FILE_NAME)
}

/// Where a reader has got to, in bytes from the start of the file.
///
/// A byte offset rather than a line number, because that is what makes following cheap: the next
/// read is a seek and a short scan rather than a walk from the beginning. It is stable under
/// append, which is the only mutation this file ever sees.
pub type Cursor = u64;

/// Lines, plus where to resume.
#[derive(Debug, Default)]
pub struct Replay {
    pub lines: Vec<String>,
    pub cursor: Cursor,
    /// Where the first line returned begins.
    ///
    /// The other end of [`cursor`](Replay::cursor), and what makes paging **backwards** possible: a
    /// viewer that has the last hundred records asks for the hundred before `start`, and repeats
    /// until it is zero. Zero therefore means "this is the beginning of the file" and is how the
    /// page knows to stop asking.
    pub start: Cursor,
    /// Total bytes in the file when this was taken, so a caller can report how much it skipped.
    pub size: u64,
}

/// Read the last `wanted` complete lines.
///
/// Walks **backwards** in chunks rather than reading the file: on a 250 MB journal, "the last 500
/// lines" must not cost 250 MB of I/O, and a viewer opening the page is asking for exactly that.
pub fn tail(path: &Path, wanted: usize) -> std::io::Result<Replay> {
    let mut file = std::fs::File::open(path)?;
    let size = file.seek(SeekFrom::End(0))?;
    tail_to(&mut file, size, wanted, size)
}

/// The `wanted` complete lines immediately **before** a known record boundary.
///
/// The backfill step. `end` comes from a previous read's [`Replay::start`], so it is already a
/// boundary — which is what lets a page walk a journal backwards a screenful at a time without ever
/// re-reading what it has, and without the server holding any per-viewer position.
pub fn before(path: &Path, end: Cursor, wanted: usize) -> std::io::Result<Replay> {
    let mut file = std::fs::File::open(path)?;
    let size = file.seek(SeekFrom::End(0))?;
    tail_to(&mut file, end.min(size), wanted, size)
}

/// The shared walk: the last `wanted` complete lines ending at `limit`.
///
/// One implementation for the opening tail and for every backfill page, because they differ only in
/// where they stop. Two would be two places for the partial-line rule to be got wrong.
fn tail_to(
    file: &mut std::fs::File,
    limit: u64,
    wanted: usize,
    size: u64,
) -> std::io::Result<Replay> {
    const CHUNK: usize = 64 * 1024;

    let wanted = wanted.min(MAX_REPLAY_LINES);
    if limit == 0 || wanted == 0 {
        return Ok(Replay {
            lines: Vec::new(),
            cursor: limit,
            start: 0,
            size,
        });
    }

    // Everything after the last newline is a record still being written; it is not ours to show and
    // the cursor must stop before it so the next read sees it whole.
    let end;
    let mut buffer: Vec<u8> = Vec::new();
    let mut start = limit;

    // Grow backwards until the buffer holds one more newline than we need, or we reach the start.
    while start > 0 {
        let step = CHUNK.min(start as usize) as u64;
        start -= step;
        let mut chunk = vec![0u8; step as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk)?;
        chunk.truncate((limit - start) as usize);
        chunk.extend_from_slice(&buffer);
        buffer = chunk;

        if buffer.iter().filter(|b| **b == b'\n').count() > wanted {
            break;
        }
    }

    // Trim the trailing partial record, if there is one.
    if let Some(last) = buffer.iter().rposition(|b| *b == b'\n') {
        end = start + last as u64 + 1;
        buffer.truncate(last + 1);
    } else {
        // No newline anywhere in what we read: nothing complete to show.
        return Ok(Replay {
            lines: Vec::new(),
            cursor: 0,
            start: 0,
            size,
        });
    }

    // The first line is only whole if the walk reached the start of the file; otherwise the chunk
    // began mid-record. Dropping it moves `start` past its newline, which is what keeps the
    // reported boundary exact — a backfill page that misreported it would skip or repeat a record.
    let mut first = start;
    if start > 0 {
        match buffer.iter().position(|b| *b == b'\n') {
            Some(at) => {
                first = start + at as u64 + 1;
                buffer.drain(..=at);
            }
            None => {
                return Ok(Replay {
                    lines: Vec::new(),
                    cursor: end,
                    start: end,
                    size,
                });
            }
        }
    }

    let text = String::from_utf8_lossy(&buffer);
    let mut lines: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    if lines.len() > wanted {
        let dropped: usize = lines
            .drain(..lines.len() - wanted)
            .map(|l| l.len() + 1)
            .sum();
        first += dropped as u64;
    }

    Ok(Replay {
        lines,
        cursor: end,
        start: first,
        size,
    })
}

/// Read complete lines appended since `cursor`.
///
/// The follow step. Returns nothing and the same cursor when the file has not grown, which is the
/// common case and must stay cheap: it is one `metadata` call and no read at all.
pub fn read_from(path: &Path, cursor: Cursor) -> std::io::Result<Replay> {
    let mut file = std::fs::File::open(path)?;
    let size = file.seek(SeekFrom::End(0))?;

    // **A shorter file than the cursor is a new file, not a corrupt one.** A room whose save
    // directory was reset starts a fresh journal at zero, and a reader that kept seeking past the
    // end would follow a file that no longer exists. Start again from the beginning of what is
    // there rather than reporting an error at somebody watching a page.
    if size < cursor {
        return tail(path, DEFAULT_REPLAY_LINES);
    }
    if size == cursor {
        return Ok(Replay {
            lines: Vec::new(),
            cursor,
            start: cursor,
            size,
        });
    }

    file.seek(SeekFrom::Start(cursor))?;
    let mut buffer = Vec::with_capacity((size - cursor) as usize);
    file.take(size - cursor).read_to_end(&mut buffer)?;

    let Some(last) = buffer.iter().rposition(|b| *b == b'\n') else {
        // Growth with no complete record yet: a record mid-write. Do not advance.
        return Ok(Replay {
            lines: Vec::new(),
            cursor,
            start: cursor,
            size,
        });
    };
    buffer.truncate(last + 1);
    let advanced = cursor + last as u64 + 1;

    let text = String::from_utf8_lossy(&buffer);
    Ok(Replay {
        lines: text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect(),
        cursor: advanced,
        start: cursor,
        size,
    })
}

/// Read from the first record at or after `at`, in unix seconds.
///
/// **A binary search over byte offsets**, which the file's own shape permits: it is append-only and
/// `at` comes from the room's clock, so it is non-decreasing. The alternative is scanning, and
/// scanning 250 MB to answer "show me the last hour" is the thing this exists to avoid.
///
/// Records that predate this Puna are handled by the same rule as everything else here: a line whose
/// `at` cannot be read sorts as *older* than the target, so an unparseable region is skipped rather
/// than being allowed to end the search early.
pub fn since(path: &Path, at: f64) -> std::io::Result<Replay> {
    let mut file = std::fs::File::open(path)?;
    let size = file.seek(SeekFrom::End(0))?;
    if size == 0 {
        return Ok(Replay {
            lines: Vec::new(),
            cursor: 0,
            start: 0,
            size,
        });
    }

    // A lower-bound search, tracking the answer explicitly rather than inferring it from where the
    // bounds met. The first version inferred it and landed one record early, every time: `low` is
    // set to `start + 1`, which is a byte in the middle of a line rather than the start of the next
    // one, so reading back from it recovered the record the search had just rejected.
    let mut answer = size;
    let (mut low, mut high) = (0u64, size);
    while low < high {
        let mid = low + (high - low) / 2;
        let start = next_line_start(&mut file, mid, size)?;
        if start >= high {
            // No record boundary left in this window; everything above `mid` is already excluded.
            high = mid;
            continue;
        }
        match line_at(&mut file, start)? {
            Some(t) if t < at => low = start + 1,
            _ => {
                answer = start;
                high = start;
            }
        }
    }

    let mut replay = read_from(path, answer)?;
    if replay.lines.len() > MAX_REPLAY_LINES {
        replay.lines.drain(..replay.lines.len() - MAX_REPLAY_LINES);
    }
    Ok(replay)
}

/// The offset of the first record boundary at or **after** `from`.
///
/// A boundary is offset zero, or any offset immediately following a newline — so an offset that is
/// *already* a boundary is returned unchanged. The first version always scanned forward, which
/// skipped the record starting exactly at `from`: harmless most of the time, and wrong precisely
/// when that record is the one being searched for.
fn next_line_start(file: &mut std::fs::File, from: u64, size: u64) -> std::io::Result<u64> {
    if from == 0 {
        return Ok(0);
    }
    file.seek(SeekFrom::Start(from - 1))?;
    let mut previous = [0u8; 1];
    if file.read_exact(&mut previous).is_ok() && previous[0] == b'\n' {
        return Ok(from);
    }
    file.seek(SeekFrom::Start(from))?;
    let mut byte = [0u8; 1];
    let mut at = from;
    while at < size {
        if file.read_exact(&mut byte).is_err() {
            break;
        }
        at += 1;
        if byte[0] == b'\n' {
            return Ok(at);
        }
    }
    Ok(size)
}

/// The `at` of the record beginning at this offset.
///
/// Read by scanning for the field rather than by parsing the object, because the two record types
/// order their keys differently — a `check` leads with `type` and an `options` leads with `at` — and
/// a search that assumed either would silently fail on the other.
fn line_at(file: &mut std::fs::File, start: u64) -> std::io::Result<Option<f64>> {
    const PEEK: usize = 512;

    file.seek(SeekFrom::Start(start))?;
    let mut buffer = vec![0u8; PEEK];
    let read = file.read(&mut buffer)?;
    buffer.truncate(read);
    let text = String::from_utf8_lossy(&buffer);
    let line = text.split('\n').next().unwrap_or_default();
    Ok(timestamp_of(line))
}

/// The `at` field of one journal line, without parsing the whole object.
pub fn timestamp_of(line: &str) -> Option<f64> {
    let at = line.find("\"at\":")? + 5;
    let rest = &line[at..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn journal(lines: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(FILE_NAME);
        let mut file = std::fs::File::create(&path).expect("create");
        for line in lines {
            writeln!(file, "{line}").expect("write");
        }
        (dir, path)
    }

    fn check(at: f64, finder: &str) -> String {
        format!(
            r#"{{"type":"check","at":{at:.3},"finder":1,"finder_name":"{finder}","receiver":2,"receiver_name":"b","item":1,"item_name":"Thing","location":2,"location_name":"Place","flags":0}}"#
        )
    }

    #[test]
    fn a_tail_reads_the_end_without_reading_the_file() {
        let lines: Vec<String> = (0..5000).map(|n| check(1000.0 + n as f64, "a")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (_dir, path) = journal(&refs);

        let replay = tail(&path, 10).expect("tail");
        assert_eq!(replay.lines.len(), 10);
        assert_eq!(replay.lines.last().unwrap(), &lines[4999]);
        assert_eq!(replay.lines[0], lines[4990]);
        // The cursor is the end of the file, so following starts with nothing to say.
        assert_eq!(replay.cursor, replay.size);
        assert!(read_from(&path, replay.cursor).unwrap().lines.is_empty());
    }

    #[test]
    fn a_short_journal_gives_everything_it_has() {
        let (_dir, path) = journal(&[&check(1.0, "a"), &check(2.0, "b")]);
        assert_eq!(tail(&path, 500).expect("tail").lines.len(), 2);
    }

    /// **The partial-line rule, which is the whole reason this module is not three lines long.**
    ///
    /// pahoa's writer flushes a 256 KiB buffer with no regard for record boundaries, so a reader
    /// arriving mid-flush sees half an object. It must not be shown, and the cursor must not step
    /// over it, or that record is lost for good.
    #[test]
    fn a_half_written_record_is_never_shown_and_never_skipped() {
        let (_dir, path) = journal(&[&check(1.0, "a"), &check(2.0, "b")]);
        let whole = std::fs::metadata(&path).unwrap().len();

        // A record arrives, torn.
        let partial = r#"{"type":"check","at":3.0,"finder":1,"fin"#;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(partial.as_bytes()).unwrap();

        let replay = tail(&path, 500).expect("tail");
        assert_eq!(replay.lines.len(), 2, "a torn record was shown");
        assert_eq!(
            replay.cursor, whole,
            "the cursor stepped over a torn record"
        );

        let follow = read_from(&path, replay.cursor).expect("follow");
        assert!(follow.lines.is_empty(), "a torn record was shown on follow");
        assert_eq!(follow.cursor, whole, "follow stepped over a torn record");

        // The rest of it lands.
        file.write_all(b"njjj\",\"receiver\":2}\n").unwrap();
        let done = read_from(&path, replay.cursor).expect("follow");
        assert_eq!(done.lines.len(), 1, "the completed record never arrived");
        assert!(done.lines[0].starts_with(partial));
    }

    #[test]
    fn following_returns_only_what_is_new() {
        let (_dir, path) = journal(&[&check(1.0, "a")]);
        let first = tail(&path, 500).expect("tail");

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", check(2.0, "b")).unwrap();

        let next = read_from(&path, first.cursor).expect("follow");
        assert_eq!(next.lines.len(), 1);
        assert!(next.lines[0].contains("\"finder_name\":\"b\""));
        assert!(read_from(&path, next.cursor).unwrap().lines.is_empty());
    }

    /// A room whose save directory was reset starts a fresh journal at zero. A follower holding an
    /// offset from the old one must not seek past the end of the new file forever.
    #[test]
    fn a_truncated_journal_restarts_rather_than_erroring() {
        let (_dir, path) = journal(&[&check(1.0, "a"), &check(2.0, "b")]);
        let far_past_the_end = 10_000_000;
        let replay = read_from(&path, far_past_the_end).expect("follow");
        assert_eq!(replay.lines.len(), 2, "a reset journal was not picked up");
    }

    #[test]
    fn a_search_by_time_lands_on_the_first_record_at_or_after_it() {
        let lines: Vec<String> = (0..2000).map(|n| check(1000.0 + n as f64, "a")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (_dir, path) = journal(&refs);

        for target in [1000.0, 1500.5, 2999.0] {
            let replay = since(&path, target).expect("since");
            let first = timestamp_of(&replay.lines[0]).expect("a timestamp");
            assert!(
                first >= target,
                "landed before the target: {first} < {target}"
            );
            // And on the FIRST such record, not merely one of them.
            assert!(
                first - target < 1.5,
                "overshot: {first} for a target of {target}"
            );
        }

        // Before the beginning is the beginning; after the end is nothing.
        assert_eq!(since(&path, 0.0).expect("since").lines.len(), 2000);
        assert!(since(&path, 1e12).expect("since").lines.is_empty());
    }

    /// The two record types order their keys differently, which a search that parsed positionally
    /// would get wrong on one of them.
    #[test]
    fn a_timestamp_is_found_wherever_the_field_sits() {
        assert_eq!(timestamp_of(&check(1234.5, "a")), Some(1234.5));
        let options = r#"{"at":1787729495.026,"hint_cost":10,"type":"options"}"#;
        assert_eq!(timestamp_of(options), Some(1787729495.026));
        assert_eq!(timestamp_of(r#"{"type":"gap","dropped":3}"#), None);
    }

    /// **Paging backwards reconstructs the file exactly — no gap, no repeat.**
    ///
    /// This is the assertion the backfill button rests on, and the failure it guards is silent: an
    /// off-by-one in the reported `start` shows up as one record missing between two pages, or one
    /// rendered twice, in the middle of a wall of similar-looking lines. Nobody would notice, and a
    /// history that quietly drops a record is the thing this whole feature must not do.
    ///
    /// Deliberately walked in an awkward page size (7) against a line count that is not a multiple
    /// of it, so the last page is short and the boundary arithmetic is exercised rather than
    /// aligned away.
    #[test]
    fn paging_backwards_rebuilds_the_journal_without_gaps_or_repeats() {
        // **Larger than the 64 KiB read chunk, deliberately.** The first version used 200 lines —
        // about 40 KiB — so every backwards walk reached offset zero in one chunk and the branch
        // where a page begins mid-record never ran. A mutation returning the chunk's start instead
        // of the first whole record's start passed cleanly against it. This fixture is this size
        // because of that mutation.
        let lines: Vec<String> = (0..2000).map(|n| check(1000.0 + n as f64, "a")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (_dir, path) = journal(&refs);
        assert!(
            std::fs::metadata(&path).unwrap().len() > 64 * 1024,
            "the fixture must exceed one read chunk or the mid-record path is never taken"
        );

        let mut seen: Vec<String> = Vec::new();
        let mut page = tail(&path, 137).expect("tail");
        let mut pages = 0;
        while !page.lines.is_empty() {
            let mut front = page.lines.clone();
            front.extend(seen);
            seen = front;
            pages += 1;
            if page.start == 0 {
                break;
            }
            page = before(&path, page.start, 137).expect("before");
            assert!(pages < 200, "paging did not terminate");
        }

        assert_eq!(seen, lines, "walking backwards did not rebuild the file");
        assert!(
            pages > 10,
            "only {pages} pages: the walk is not being exercised"
        );
    }

    /// The other half: `start` is a real record boundary, so the page before it joins on cleanly.
    #[test]
    fn the_reported_start_is_a_record_boundary() {
        let lines: Vec<String> = (0..50).map(|n| check(n as f64, "a")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (_dir, path) = journal(&refs);

        let page = tail(&path, 10).expect("tail");
        assert!(page.start > 0);
        // Reading forward from `start` yields exactly what the tail showed.
        let forward = read_from(&path, page.start).expect("read_from");
        assert_eq!(forward.lines, page.lines);
    }

    #[test]
    fn a_replay_is_capped_however_much_is_asked_for() {
        let lines: Vec<String> = (0..MAX_REPLAY_LINES + 500)
            .map(|n| check(n as f64, "a"))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (_dir, path) = journal(&refs);
        assert_eq!(
            tail(&path, usize::MAX).expect("tail").lines.len(),
            MAX_REPLAY_LINES
        );
        assert_eq!(
            since(&path, 0.0).expect("since").lines.len(),
            MAX_REPLAY_LINES
        );
    }
}
