//! The journal viewer: a feed of item movements, and the file behind it.
//!
//! ## Two questions, asked in order
//!
//! `history.jsonl` carries nine record types and they do not belong to one audience, so reaching a
//! feed and reading all of it are separate decisions:
//!
//! 1. **May this viewer reach the feed at all?** `may_see_tracker`, the same rule and the same
//!    per-room policy the tracker uses. A race room's feed is behind a login exactly as its
//!    tracker is.
//! 2. **How much do they get?** [`room::JournalPolicy`], per room. `disabled` is staff-only,
//!    `feed` is what happened in the *games* with everything else withheld and counted, `full` is
//!    the file as written plus the download.
//!
//! An organizer reads everything under all three, so the column names what the tier below them
//! gets. **The line `feed` draws is gameplay against the room's operation**: item movements, the
//! Link conventions, and goals — the wall of *"X sent Y to Z (location)"* the room already
//! broadcasts to every unfiltered client, plus the line that explains why a world suddenly emptied
//! into it. The rest — `chat` above all, every line anybody typed, and the moderation and
//! configuration beside it — is the part a room may reasonably want to keep among its participants,
//! or may not.
//!
//! **Why this is a room's decision rather than a constant.** The feed link is rendered on the room
//! page wherever the tracker link is, so the people holding it are the people holding the room
//! link — which for most rooms means withholding the history protects nobody and only makes the
//! ordinary case need a setting change. For a streamed race it is the opposite. Neither answer is
//! right for every room, and a single default was wrong for one of them whichever way it pointed.
//!
//! **Half of this is not expressible.** If the download served the file while the socket filtered
//! it, the filter would be decorative: the file is *where the withheld records are*. So `full`
//! opens both and `feed` opens neither, and there is no arm that opens one.
//!
//! ## This module previously said the opposite, and the mistake is worth keeping
//!
//! An earlier version of this file asserted the journal held "exactly three record types — `check`,
//! `options` and `gap` — with no chat, no hints and no cheats anywhere in it", and used that to put
//! the whole file at the tracker tier. It was read off a real 250 MB journal from the dev cluster.
//!
//! **The file was accurate and unrepresentative.** It was produced by `room-load`, a synthetic
//! client that sends `LocationChecks` and `StatusUpdate` and nothing else — so the only records it
//! could ever contain are the ones a robot can generate. An empty category in a sample is not an
//! absent category in a format, and the sample was drawn from something with no mouth. pahoa caught
//! it before it shipped.
//!
//! The rule that follows: **a format is read from the code that writes it**, and a corpus is
//! evidence about a corpus. `JournalEvent`'s constructors in `pahoa-room/src/effect.rs` are the
//! authority, and [`PUBLIC_KINDS`] is transcribed against them rather than against a file.

use rocket::response::stream::ByteStream;
use rocket::response::{self, Responder};
use rocket::{Request, Response, Route, State, get, http::Header, routes};
use rocket_ws as ws;

use puna_core::model::member::RoomRole;
use puna_core::model::{room, slot};

use crate::auth::Session;
use crate::error::{Result, not_found};
use crate::journal;
use crate::params::JournalParam;
use crate::routes::rooms::resolve_role;
use crate::tpl::TplContext;
use crate::{DataDir, Pool};

pub fn routes() -> Vec<Route> {
    routes![page, download, feed]
}

/// How often a follower looks for new records.
///
/// The floor is what pahoa's writer can deliver rather than anything about this loop: records reach
/// the file when its buffer flushes, so polling faster than that only spends syscalls to find
/// nothing. A second is comfortably below any flush cadence and is imperceptible to a reader.
const POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// How often the server pings an idle feed.
///
/// **Not optional.** The route runs behind Envoy, which applies a stream idle timeout and will close
/// a connection that has said nothing — and a journal feed on a quiet room says nothing for minutes
/// at a time, which is its normal condition rather than an edge case. The server pings because the
/// server is the side that knows the connection is meant to stay open; a browser will answer a ping
/// without any script running.
const PING: std::time::Duration = std::time::Duration::from_secs(30);

/// Record types a viewer at the `feed` tier may see.
///
/// **Transcribed from pahoa's `docs/journal.md`**, which is the authoritative table, not from a
/// corpus — see the module note. Every entry here describes *a cross-game effect landing on
/// somebody*, which is the thing this feed is for:
///
///   * `check` — an item moved. The wall of "X sent Y to Z", and the reason the page exists.
///   * `gap` — pahoa's own marker that records were dropped. **Never filtered under any policy**:
///     it is the only evidence the history is incomplete, and a viewer that dropped it would
///     present a partial history as a whole one.
///   * `deathlink`, `traplink`, `ringlink` — the three link conventions. A link is the same kind of
///     event as a check: discrete, player-affecting and cross-game, and *"why did I get a trap I
///     never earned"* is exactly what a feed is opened to answer. DeathLink was only ever the
///     popular one rather than the only one; upstream has three, pahoa records them from one table,
///     and treating them differently here would be an editorial guess rather than a property of
///     the protocol.
///
///   * `goal` — a slot finished. pahoa writes it **before** the auto-release and auto-collect it
///     triggers, deliberately, so the flood of `check` records those produce sits underneath the
///     line that explains them. Withholding it left a public feed where a world empties out for no
///     stated reason, which is the one thing a reader most wants explained.
///
/// Everything else is withheld from a `feed` viewer and counted. That is the whole list of what a
/// room's participants said and did to each other rather than to each other's *games*: `chat`,
/// `admin`, `cheat`, `hints`, `connected`, `disconnected`, `tags_changed`, `started`, `stopped`,
/// `options`, `option_changed`, `slot_password_changed`.
///
/// **pahoa proposed a wider set** — `started`, `stopped`, `goal` and `disconnected`. `goal` is now
/// taken, on its own merits above. The other three are not: they are true statements about the
/// room's *operation* rather than about play, and a room that wants them public has `full` to say
/// so.
///
/// --- WHAT A RELEASE LOOKS LIKE HERE, because the obvious answer is wrong twice over -------------
///
/// There is **no `release` record and no `collect` record**, and the announcement everybody knows —
/// *"X (Team #1) has released all remaining items from their world."* — is **not journaled at all**.
/// `room.rs`'s `release_player` broadcasts it as a `PrintJSON` and writes nothing; `chat` is
/// player-typed `Say` only (`commands.rs`'s `broadcast_chat`), so it is not hiding there either.
///
/// So a release reaches this feed the only way it can: as the flood of `check` records it produces,
/// with the `goal` above it saying why. That is most of what the announcement was for — and adding
/// `"release"` to the list above would match nothing, which is a change that reads as a feature and
/// does nothing.
///
/// An explicitly-run `!release` does leave an `admin` record, and `admin` stays withheld: the same
/// type carries `say`, `hint`, `alias`, `option`, `kick` and `lock`, which is precisely the
/// moderation and configuration a `feed` viewer is kept out of. Publishing one verb of it would
/// mean reading the `command` field, and that is not worth building for a line the room never
/// wrote down.
pub const PUBLIC_KINDS: [&str; 6] = ["check", "gap", "deathlink", "traplink", "ringlink", "goal"];

/// Whether one record may be shown to a viewer at the `feed` tier.
///
/// Fails closed: a record with no `type`, or a `type` this build has never heard of, is withheld. A
/// `feed` built today must not start publishing whatever pahoa adds tomorrow.
pub fn is_public(event: &serde_json::Value) -> bool {
    event
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| PUBLIC_KINDS.contains(&kind))
}

/// How much of the history this viewer gets.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Visibility {
    /// What happened in the games: items, Links, and goals. Anyone the room's `tracker_policy`
    /// admits. See [`PUBLIC_KINDS`].
    Feed,
    /// The file as written. Organizers.
    Everything,
}

/// Resolve the room and decide how much of its history this viewer may read.
///
/// One function for every route, so the page cannot offer a feed the socket refuses or a download
/// the page would not link. `404` rather than `403` for a refusal, matching the tracker: the room's
/// existence is not the secret, but under a `disabled` policy neither is anything else, and a
/// distinguishable refusal is a probe oracle.
///
/// **Both gates answer the same `404`**, and that is what keeps `journal_policy` from being an
/// oracle in its own right: a viewer refused by the room's tracker policy and one refused by its
/// journal policy cannot tell which of the two turned them away, so neither refusal reports a
/// setting to somebody who is not entitled to read it.
async fn readable(
    conn: &mut diesel_async::AsyncPgConnection,
    session: &Session,
    id: JournalParam,
) -> Result<(room::Room, Visibility)> {
    // **Resolved by the feed's own id, never the room's.** That is the whole reason the column
    // exists: `/journal/<id>` is the link handed to a stream chat, and the room it belongs to must
    // not be recoverable from it.
    let room = room::by_journal_id(conn, id.0)
        .await?
        .ok_or_else(|| not_found("no such feed"))?;
    let role = resolve_role(conn, session, room.id).await?;
    let owns_a_slot = match session.user_id {
        Some(user_id) => slot::owns_a_slot(conn, room.id, user_id).await?,
        None => false,
    };
    if !room::may_see_tracker(room.tracker_policy, role.is_some(), owns_a_slot) {
        return Err(not_found("this room's history is not available"));
    }
    let visibility = visibility_for(role, room.journal_policy)
        .ok_or_else(|| not_found("this room's history is not available"))?;
    Ok((room, visibility))
}

/// How much of the history a role may read under a room's policy.
///
/// Its own function so the rule is testable without a database and stated once — the page, the
/// socket and the download all reach it through [`readable`], so none of them can hold a different
/// opinion about what a viewer is owed.
///
/// **`None` is a refusal, not a default.** Returning [`Visibility::Feed`] for a `disabled` room
/// would be the quietest possible way to widen this: every caller would go on working, and the
/// setting would appear in the UI doing nothing.
///
/// **Organizer, not helper**, for the tier that reads regardless: the file carries every line
/// anybody typed in the room, and M20's line puts "who is trusted with the room" on the
/// organizer's side of it. A helper is bound by the room's policy like anybody else.
pub(crate) fn visibility_for(
    role: Option<RoomRole>,
    policy: room::JournalPolicy,
) -> Option<Visibility> {
    if role.is_some_and(|r| r >= RoomRole::Organizer) {
        return Some(Visibility::Everything);
    }
    match policy {
        room::JournalPolicy::Disabled => None,
        room::JournalPolicy::Feed => Some(Visibility::Feed),
        room::JournalPolicy::Full => Some(Visibility::Everything),
    }
}

#[derive(askama::Template, askama_web::WebTemplate)]
#[template(path = "rooms/journal.html")]
pub struct JournalTemplate {
    base: TplContext,
    room: room::Room,
    /// Bytes on disk, so the page can say what the download costs before somebody starts it. `None`
    /// when the room has never been started and the file does not exist yet.
    size: Option<u64>,
    /// Whether to offer the whole-file download at all. Decided here, never in markup: the file
    /// carries chat, and a template cannot prove it did not render a link.
    may_download: bool,
}

#[get("/journal/<id>")]
async fn page(
    id: JournalParam,
    session: Session,
    pool: &State<Pool>,
    data_dir: &State<DataDir>,
) -> Result<JournalTemplate> {
    let mut conn = pool.get().await?;
    let (room, visibility) = readable(&mut conn, &session, id).await?;
    let size = tokio::fs::metadata(journal::path(&data_dir.0, room.id))
        .await
        .ok()
        .map(|m| m.len());

    Ok(JournalTemplate {
        base: TplContext::new(&session),
        room,
        size,
        may_download: visibility == Visibility::Everything,
    })
}

/// A streamed, gzipped journal with a filename attached.
///
/// Hand-rolled rather than `#[derive(Responder)]`, which cannot hold a `ByteStream!` — the macro
/// produces an opaque type and a struct field has to be nameable. Building the response here is
/// three lines and keeps the body a stream rather than a `Vec`, which is the whole point.
struct Journal {
    blocks: tokio::sync::mpsc::Receiver<Vec<u8>>,
    filename: String,
}

impl<'r> Responder<'r, 'r> for Journal {
    fn respond_to(self, request: &'r Request<'_>) -> response::Result<'r> {
        let mut blocks = self.blocks;
        let body = ByteStream! {
            while let Some(block) = blocks.recv().await {
                yield block;
            }
        };
        Response::build_from(body.respond_to(request)?)
            .header(rocket::http::ContentType::new("application", "x-ndjson"))
            .header(Header::new(
                "Content-Disposition",
                format!("attachment; filename=\"{}\"", self.filename),
            ))
            // The body is gzip and the resource is JSONL, so the browser inflates it and saves
            // `.jsonl`. Set by hand rather than naming the file `.gz`, so what lands on disk is the
            // thing the page offered.
            .header(Header::new("Content-Encoding", "gzip"))
            .ok()
    }
}

/// The whole journal, gzipped on the way out.
///
/// **Streamed and never buffered**, which is not a style preference: these files are routinely
/// hundreds of megabytes — 250 MB on the cluster's load-test rooms — and reading one into a `Vec`
/// the way the patch route does would be a quarter of a gigabyte of web-tier memory per click.
///
/// Gzipped because it is worth 7.5× on real data (48.8 MiB of records became 6.5 MiB) and because
/// this is ordinary HTTP, where compression is available — unlike the feed below, which cannot have
/// it. The encoder runs on a blocking thread and hands finished blocks to the response, so a slow
/// client backs the work up rather than occupying a runtime worker with it.
#[get("/journal/<id>/download")]
async fn download(
    id: JournalParam,
    session: Session,
    pool: &State<Pool>,
    data_dir: &State<DataDir>,
) -> Result<Journal> {
    let mut conn = pool.get().await?;
    let (room, visibility) = readable(&mut conn, &session, id).await?;
    // **The file is where the withheld records are**, so it goes only to a viewer entitled to all
    // of them -- an organizer, or anybody at all on a `full` room. Serving it to a `Feed` viewer
    // would hand over precisely what the socket just filtered, which is the one combination
    // `JournalPolicy` deliberately cannot express. `404` rather than `403`, so a refusal says
    // nothing a probe could use -- the same answer the gates above give.
    if visibility != Visibility::Everything {
        return Err(not_found("this room's history is not available"));
    }
    let path = journal::path(&data_dir.0, room.id);

    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Err(not_found(
            "this room has no history yet. A journal is written while the room runs.",
        ));
    }

    // A bounded channel is the backpressure: the encoder blocks once the client is a few blocks
    // behind, so a paused download stops reading the file rather than filling memory with it.
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let for_log = path.clone();
    std::thread::spawn(move || {
        use std::io::Read;
        let Ok(file) = std::fs::File::open(&path) else {
            return;
        };
        let mut encoder = flate2::read::GzEncoder::new(
            std::io::BufReader::new(file),
            flate2::Compression::fast(),
        );
        let mut block = vec![0u8; 64 * 1024];
        loop {
            match encoder.read(&mut block) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.blocking_send(block[..n].to_vec()).is_err() {
                        // The reader went away mid-download, which is ordinary.
                        break;
                    }
                }
                Err(error) => {
                    tracing::warn!(path = %for_log.display(), %error, "journal download failed");
                    break;
                }
            }
        }
    });

    Ok(Journal {
        blocks: rx,
        filename: download_name(&room),
    })
}

/// What the saved file is called.
///
/// **Never the room's id.** The whole point of the feed having an id of its own is that holding the
/// link does not hand over the room — and a `Content-Disposition` naming `room.id` would have put it
/// back, in a file that outlives the page and gets forwarded. That is the same class of leak as a
/// tracker page rendering the room's address: the capability escapes through the artifact rather
/// than through the URL.
///
/// The room's **name** is fine and is what a person wants when three of these are in one downloads
/// folder: it is already rendered on this page and on the public room page, and it names nothing
/// that can be navigated to. Sanitized the way patch downloads are — an allowlist, with no `.` at
/// all, so `..` and a leading dot are unspellable rather than merely filtered.
fn download_name(room: &room::Room) -> String {
    let stem: String = room
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let stem = stem.trim_matches('-');
    if stem.is_empty() {
        "feed.jsonl".to_string()
    } else {
        format!("{stem}-feed.jsonl")
    }
}

/// Where a viewer wants the replay to start.
///
/// Named `Anchor` rather than `From`, which would shadow the trait, and `FeedRequest` rather than
/// `Request`, which is Rocket's.
#[derive(serde::Deserialize, Default)]
struct Anchor {
    /// The last N records. Capped server-side.
    lines: Option<usize>,
    /// Or everything at or after this instant, in unix seconds.
    at: Option<f64>,
    /// Or everything after this byte offset — **what a RECONNECT sends**, being the `cursor` the
    /// page already has.
    ///
    /// A byte offset rather than the `at` beside it, because this one has to join exactly: `since`
    /// is inclusive and several records routinely share a timestamp, so resuming by instant would
    /// re-send whatever tied with the last line on the page. The offset is the position `read_from`
    /// already follows with, so a resume is the same read the follow loop makes, once.
    after: Option<journal::Cursor>,
}

#[derive(serde::Deserialize, Default)]
struct FeedRequest {
    from: Option<Anchor>,
    /// Page **backwards**: the records immediately before this byte offset.
    ///
    /// Sent repeatedly by the page's "load the whole feed" control, each time with the `start` the
    /// previous answer reported, until that reaches zero. The server holds no per-viewer position —
    /// the offset the client returns *is* the position — so a viewer that reconnects mid-backfill
    /// simply carries on, and two tabs backfilling at once cost nothing between them.
    before: Option<u64>,
}

/// The live feed: replay from a point, then follow.
///
/// The client speaks first, naming where to start; a client that says nothing within a moment gets
/// the default tail, so a viewer whose script failed to send still sees a page with history on it.
/// The first read a feed makes, from whatever the client anchored on.
///
/// **A resume can decline to be one, and `start` is how it says so.** `read_from` is uncapped — the
/// follow loop calls it every poll, so its increments are a handful of lines — but a resume's gap is
/// however long the viewer was away, and a laptop that slept through a mass release would ask for
/// six figures of records in one frame. Past the cap this serves a fresh tail instead.
///
/// So the contract with the page is: **`start == after` means the resume joined**, and anything else
/// means it got a tail and must replace what it is showing rather than extend it. The other fallback
/// rides the same signal for free — `read_from` already answers a truncated file (a room whose save
/// was reset) with a tail, and that tail's `start` is not the offset asked for either.
///
/// Falling back *silently* is the shape being avoided. Either fallback appended to a page that kept
/// its lines is a hole in the middle of the feed with nothing marking it, which is the failure this
/// module's own history is about: an incomplete record that does not announce itself.
fn opening_replay(path: &std::path::Path, from: &Anchor) -> std::io::Result<journal::Replay> {
    match (from.after, from.at) {
        (Some(after), _) => {
            let replay = journal::read_from(path, after)?;
            if replay.lines.len() > journal::MAX_REPLAY_LINES {
                journal::tail(path, journal::DEFAULT_REPLAY_LINES)
            } else {
                Ok(replay)
            }
        }
        (None, Some(at)) => journal::since(path, at),
        (None, None) => journal::tail(path, from.lines.unwrap_or(journal::DEFAULT_REPLAY_LINES)),
    }
}

#[get("/journal/<id>/feed")]
async fn feed(
    id: JournalParam,
    ws: ws::WebSocket,
    session: Session,
    pool: &State<Pool>,
    data_dir: &State<DataDir>,
    shutdown: rocket::Shutdown,
) -> Result<ws::Channel<'static>> {
    let mut conn = pool.get().await?;
    // **Authorized before the upgrade**, so a refused viewer gets an ordinary 404 rather than an
    // open socket that says nothing.
    let (room, visibility) = readable(&mut conn, &session, id).await?;
    let path = journal::path(&data_dir.0, room.id);

    Ok(ws.channel(move |mut stream| {
        Box::pin(async move {
            use rocket::futures::{SinkExt, StreamExt};

            // A client that never speaks still gets a page with history on it.
            let request: FeedRequest = match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                stream.next(),
            )
            .await
            {
                Ok(Some(Ok(ws::Message::Text(text)))) => {
                    serde_json::from_str(&text).unwrap_or_default()
                }
                Ok(Some(Err(_)) | None) => return Ok(()),
                _ => FeedRequest::default(),
            };

            let from = request.from.unwrap_or_default();
            let opening = {
                let path = path.clone();
                tokio::task::spawn_blocking(move || opening_replay(&path, &from)).await
            };

            let mut cursor = match opening {
                Ok(Ok(replay)) => {
                    let cursor = replay.cursor;
                    // `start` rides the opening frame as well as every backfill page, because it is
                    // what the page anchors its walk on -- and it has to be re-sent on every replay,
                    // including after a reconnect, or a viewer that dropped mid-backfill would ask
                    // for a region its current view no longer joins on to.
                    let mut frame: serde_json::Value = serde_json::from_str(&batch(
                        "replay",
                        &replay.lines,
                        cursor,
                        Some(replay.size),
                        visibility,
                    ))
                    .unwrap_or_default();
                    frame["start"] = replay.start.into();
                    // **How often the page may expect to hear something, from the side that
                    // decides it.** The client's watchdog is a multiple of this. Hardcoding the
                    // number there instead would be two constants in two files that must agree,
                    // and the failure of that agreement is silent in the worse direction: a
                    // watchdog shorter than the heartbeat tears down a healthy connection on a
                    // timer.
                    frame["heartbeat_ms"] = (PING.as_millis() as u64).into();
                    if stream
                        .send(ws::Message::Text(frame.to_string()))
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                    cursor
                }
                // A room that has never run has no file, which is not an error worth closing over:
                // say so and follow, because the file appears the moment it starts.
                _ => {
                    let _ = stream
                        .send(ws::Message::Text(
                            r#"{"kind":"empty","message":"no history yet"}"#.to_string(),
                        ))
                        .await;
                    0
                }
            };

            let mut poll = tokio::time::interval(POLL);
            let mut ping = tokio::time::interval(PING);
            ping.reset();

            // Cloned per connection, and a `Future` as well as a request guard: it resolves the
            // moment Rocket is asked to shut down, whether by SIGTERM or by anything else.
            let mut shutdown = shutdown;

            loop {
                tokio::select! {
                    biased;

                    // **The room's feed gets out of the way of a rollout, first.**
                    //
                    // `biased`, so this arm is checked before the others: a poll or a ping that ran
                    // first would spend a syscall on a connection that is about to close, and on a
                    // busy room the poll arm is ready every tick.
                    //
                    // Without this, an open feed holds a pod for the WHOLE grace period. Rocket's
                    // `CancellableIo` keeps doing ordinary I/O until `grace` expires and hyper's
                    // graceful shutdown waits for open connections -- and a feed socket never
                    // completes on its own, that being the point of it. So a grace long enough for
                    // a download would be paid on every rollout by every reader, for nothing.
                    //
                    // Closing cleanly is also better for the page than being cut: the browser fires
                    // `close` at once rather than after a TCP timeout, so it redials in a few
                    // hundred milliseconds and resumes at its cursor. A reader sees the dot blink
                    // rather than a gap.
                    _ = &mut shutdown => {
                        let _ = stream.send(ws::Message::Close(Some(ws::frame::CloseFrame {
                            // 1001 "going away", which is what this is: the endpoint is shutting
                            // down and the client should reconnect. Not 1000, which reads as "we
                            // are done here" and invites a client to stay closed.
                            code: ws::frame::CloseCode::Away,
                            reason: "the server is restarting".into(),
                        }))).await;
                        return Ok(());
                    }
                    _ = poll.tick() => {
                        let path = path.clone();
                        let at = cursor;
                        let Ok(Ok(replay)) = tokio::task::spawn_blocking(move ||
                            journal::read_from(&path, at)).await else { continue };
                        if replay.lines.is_empty() {
                            continue;
                        }
                        cursor = replay.cursor;
                        let frame = batch("append", &replay.lines, cursor, None, visibility);
                        if stream.send(ws::Message::Text(frame)).await.is_err() {
                            return Ok(());
                        }
                    }
                    _ = ping.tick() => {
                        // **Two frames, doing two different jobs, and the second is the one the
                        // page can act on.**
                        //
                        // The protocol Ping keeps the connection alive through Envoy, which closes
                        // an idle stream and a quiet room is idle by nature. A browser answers it
                        // itself, with no script running — which is exactly what makes it useless
                        // for detecting a dead link: **the WebSocket API exposes no ping or pong to
                        // JavaScript at all**, so a page cannot send one, cannot see one, and
                        // cannot tell a silent room from a black hole.
                        //
                        // Found by blocking the site with iptables: nothing was RESET, so TCP kept
                        // retransmitting into the void, the socket stayed open, `close` never
                        // fired, and the page sat on a green dot for five minutes. It recovered
                        // when the block lifted and a retransmission finally landed — the same
                        // connection catching up, never a reconnect.
                        //
                        // So the heartbeat is an ordinary text frame. It is the ONLY thing that
                        // reaches the page on a quiet room, which is what lets a watchdog there
                        // conclude anything.
                        if stream.send(ws::Message::Ping(Vec::new())).await.is_err() {
                            return Ok(());
                        }
                        if stream.send(ws::Message::Text(
                            r#"{"kind":"heartbeat"}"#.to_string()
                        )).await.is_err() {
                            return Ok(());
                        }
                    }
                    incoming = stream.next() => match incoming {
                        Some(Ok(ws::Message::Close(_))) | Some(Err(_)) | None => return Ok(()),

                        // **A backfill page, on the same socket as the follow.** One connection
                        // rather than a second endpoint: the viewer is already authorized here, the
                        // tier is already decided, and a page walking backwards while records
                        // arrive at the bottom is exactly what the `select!` above makes free.
                        Some(Ok(ws::Message::Text(text))) => {
                            let request: FeedRequest =
                                serde_json::from_str(&text).unwrap_or_default();
                            let Some(end) = request.before else { continue };

                            let path = path.clone();
                            let Ok(Ok(page)) = tokio::task::spawn_blocking(move ||
                                journal::before(&path, end, journal::MAX_REPLAY_LINES)).await
                            else { continue };

                            // `start` is what the client sends back next time, and zero is how it
                            // knows to stop. It travels even when the page is empty, or a viewer
                            // whose backfill found nothing would ask forever.
                            let mut frame: serde_json::Value = serde_json::from_str(&batch(
                                "earlier", &page.lines, cursor, None, visibility,
                            ))
                            .unwrap_or_default();
                            frame["start"] = page.start.into();
                            if stream.send(ws::Message::Text(frame.to_string())).await.is_err() {
                                return Ok(());
                            }
                        }

                        // A pong, or a frame this build has no use for.
                        _ => {}
                    }
                }
            }
        })
    }))
}

/// One frame of events, filtered to what this viewer may see.
///
/// **The filter is here rather than in the page**, and that is the whole tier. A viewer at the feed
/// tier is never *sent* a chat line, so no amount of reading the socket in a console recovers one —
/// where a client-side filter would be markup proving a negative, which this codebase has decided
/// twice already it cannot do.
///
/// Two rules about what does not get through:
///
/// * **A withheld record is counted, never silently dropped.** The frame carries `withheld`, so the
///   page can say that something happened without saying what. Hiding it outright would make an
///   incomplete history look complete, which is the exact failure pahoa's own `gap` record exists to
///   prevent.
/// * **An unrecognized type is withheld from the feed tier**, not passed through. That is the
///   opposite of the rule for the organizer view and deliberately so: a type this build has never
///   seen might be anything, and defaulting to "show it" on a public surface would mean the next
///   record pahoa adds is disclosed by a build that predates it. Fail closed and count it.
///
/// A line that will not parse *at all* still travels as `{"type":"unreadable"}` to an organizer,
/// because for them the point is fidelity; for the feed it is one more withheld record.
fn batch(
    kind: &str,
    lines: &[String],
    cursor: u64,
    size: Option<u64>,
    visibility: Visibility,
) -> String {
    let mut withheld = 0usize;
    let mut events: Vec<serde_json::Value> = Vec::with_capacity(lines.len());

    for line in lines {
        let parsed = serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|_| serde_json::json!({ "type": "unreadable", "raw": line }));
        let public = is_public(&parsed);

        match visibility {
            Visibility::Everything => events.push(parsed),
            Visibility::Feed if public => events.push(parsed),
            Visibility::Feed => withheld += 1,
        }
    }

    let mut frame = serde_json::json!({
        "kind": kind,
        "cursor": cursor,
        "events": events,
    });
    if withheld > 0 {
        frame["withheld"] = withheld.into();
    }
    if let Some(size) = size {
        frame["size"] = size.into();
    }
    frame.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: &str) -> String {
        format!(r#"{{"type":"{kind}","at":1.0,"text":"the quiet part","finder_name":"a"}}"#)
    }

    /// **Chat never reaches a viewer at the feed tier.**
    ///
    /// This is the test the whole module exists around, and it is here because the first version of
    /// this file got it wrong: it put the entire journal at the tracker tier on the belief that the
    /// file held item movements only. It does not — `chat` is player-typed free text, one record per
    /// line anybody says — and the belief came from a 250 MB sample produced by a synthetic client
    /// that can only check locations.
    ///
    /// So the assertion is deliberately about the *bytes on the wire* rather than about a flag: the
    /// filter is server-side precisely so that reading the socket in a browser console cannot
    /// recover what the tier withholds.
    #[test]
    fn a_public_viewer_is_never_sent_chat_or_anything_like_it() {
        // **Every type pahoa's `docs/journal.md` lists that is not in `PUBLIC_KINDS`**, which is
        // the whole point of enumerating them here rather than asserting the complement: a record
        // type added upstream and quietly admitted is the failure this guards, and a complement
        // would admit it by construction.
        //
        // The three link types are deliberately absent — they are a *feed* event, the same kind of
        // cross-game effect a check is, and they are asserted as public in the test below.
        // `goal` moved out of this list, for the reason given on PUBLIC_KINDS. `admin` stays in it:
        // an explicitly-run release leaves one, and it is withheld along with every other verb.
        let private = [
            "chat",
            "admin",
            "cheat",
            "hints",
            "connected",
            "disconnected",
            "tags_changed",
            "started",
            "stopped",
            "option_changed",
            "slot_password_changed",
            "options",
        ];
        let lines: Vec<String> = private.iter().map(|k| line(k)).collect();

        let frame = batch("replay", &lines, 1, None, Visibility::Feed);
        for kind in private {
            assert!(
                !frame.contains(kind),
                "a viewer at the feed tier was sent a `{kind}` record"
            );
        }
        assert!(
            !frame.contains("the quiet part"),
            "chat text reached the wire for a viewer who may not read it"
        );

        // Withheld, not hidden: an incomplete history must say that it is incomplete.
        let parsed: serde_json::Value = serde_json::from_str(&frame).expect("valid JSON");
        assert_eq!(parsed["withheld"], private.len());
        assert!(parsed["events"].as_array().expect("events").is_empty());
    }

    /// A goal is public, and it is the line that explains the flood underneath it.
    #[test]
    fn a_goal_reaches_a_public_viewer() {
        let frame = batch("replay", &[line("goal")], 1, None, Visibility::Feed);
        let parsed: serde_json::Value = serde_json::from_str(&frame).expect("valid JSON");
        assert_eq!(parsed["events"].as_array().expect("events").len(), 1);
        assert!(parsed.get("withheld").is_none());
    }

    /// The feed itself — the screenshot — is `check`, the three link conventions, and `gap`.
    ///
    /// **The links belong here for the same reason a check does**: a discrete cross-game effect
    /// landing on somebody. DeathLink was only ever the popular convention rather than the only
    /// one — pahoa records all three from one table — so admitting one and withholding the others
    /// would be an editorial guess rather than a property of the protocol, and *"why did I get a
    /// trap I never earned"* is precisely what a reader opens a feed to answer.
    ///
    /// `gap` rides with them because it is the only evidence the history has holes.
    #[test]
    fn the_feed_carries_checks_links_and_the_gap_marker() {
        let lines = vec![
            line("check"),
            line("deathlink"),
            line("traplink"),
            line("ringlink"),
            r#"{"type":"gap","dropped":7}"#.to_string(),
        ];
        let parsed: serde_json::Value =
            serde_json::from_str(&batch("append", &lines, 9, None, Visibility::Feed))
                .expect("valid JSON");

        let events = parsed["events"].as_array().expect("events");
        assert_eq!(events.len(), 5, "the feed dropped a record it should carry");
        assert_eq!(events[4]["dropped"], 7);
        assert!(parsed.get("withheld").is_none());
    }

    /// **An unknown type fails CLOSED on the public feed.**
    ///
    /// The opposite of the organizer rule below, deliberately: a record type this build has never
    /// seen could be anything, so defaulting to "show it" would mean the next thing pahoa journals
    /// is disclosed by a Puna that predates it and knows nothing about it.
    #[test]
    fn an_unknown_record_is_withheld_from_the_feed_and_kept_for_an_organizer() {
        let lines = vec![line("something_pahoa_added_later")];

        let public: serde_json::Value =
            serde_json::from_str(&batch("append", &lines, 1, None, Visibility::Feed))
                .expect("valid JSON");
        assert!(public["events"].as_array().expect("events").is_empty());
        assert_eq!(public["withheld"], 1);

        let staff: serde_json::Value =
            serde_json::from_str(&batch("append", &lines, 1, None, Visibility::Everything))
                .expect("valid JSON");
        assert_eq!(staff["events"][0]["type"], "something_pahoa_added_later");
        assert!(staff.get("withheld").is_none());
    }

    /// A torn line reaches an organizer as itself. Dropping it would present an incomplete history
    /// as a complete one, which is what pahoa's `gap` record exists to prevent.
    #[test]
    fn a_line_that_will_not_parse_is_carried_through_to_an_organizer() {
        let lines = vec![r#"{"type":"check","at":2.0,"fin"#.to_string()];
        let frame: serde_json::Value = serde_json::from_str(&batch(
            "replay",
            &lines,
            42,
            Some(99),
            Visibility::Everything,
        ))
        .expect("valid JSON");

        assert_eq!(frame["events"][0]["type"], "unreadable");
        assert_eq!(frame["events"][0]["raw"], lines[0]);
        assert_eq!(frame["cursor"], 42);
        assert_eq!(frame["size"], 99);
    }

    /// An append frame carries no size: the page uses it for the download hint, and a value that
    /// changed under it every second would be noise rather than information.
    #[test]
    fn only_the_opening_frame_reports_the_file_size() {
        let frame: serde_json::Value =
            serde_json::from_str(&batch("append", &[], 7, None, Visibility::Feed))
                .expect("valid JSON");
        assert_eq!(frame["kind"], "append");
        assert!(frame.get("size").is_none());
    }

    /// An organizer reads the whole file whatever the room says, and nobody else does.
    ///
    /// The download route is what this guards, and its refusal is additionally pinned by a source
    /// lint in `tests/templates.rs` — a mutation removing the check from the route left every test
    /// here green, which is the same gap `a_restart_would_land` had.
    ///
    /// **A helper is not an organizer here**, which is the one place this tier disagrees with M20's
    /// "a helper runs the multiworld": running it does not include reading everything anybody said
    /// in it, and a helper on a `disabled` room is refused like any other viewer.
    #[test]
    fn an_organizer_reads_everything_and_the_room_decides_for_everybody_else() {
        use room::JournalPolicy as P;

        for policy in [P::Disabled, P::Feed, P::Full] {
            assert_eq!(
                visibility_for(Some(RoomRole::Organizer), policy),
                Some(Visibility::Everything),
                "an organizer is bound by {} — the policy names the tier BELOW them",
                policy.as_sql()
            );
        }

        // Everybody else, including a helper, takes the room's answer.
        for role in [Some(RoomRole::Helper), None] {
            assert_eq!(visibility_for(role, P::Disabled), None);
            assert_eq!(visibility_for(role, P::Feed), Some(Visibility::Feed));
            assert_eq!(visibility_for(role, P::Full), Some(Visibility::Everything));
        }
    }

    /// **A refused viewer gets nothing, never a quieter feed.**
    ///
    /// Its own test because the failure is silent in the direction that matters: `visibility_for`
    /// returning `Some(Feed)` instead of `None` leaves every caller working, the page still
    /// renders, the socket still streams — and the setting an organizer just chose does nothing at
    /// all. Nothing else in the suite would notice, because `Feed` is a legitimate answer
    /// everywhere it appears.
    #[test]
    fn a_disabled_journal_refuses_rather_than_narrowing() {
        assert_eq!(
            visibility_for(None, room::JournalPolicy::Disabled),
            None,
            "a disabled journal must refuse; a narrower feed is not a refusal"
        );
        assert_eq!(
            visibility_for(Some(RoomRole::Helper), room::JournalPolicy::Disabled),
            None
        );
    }

    /// **The page offers the file exactly where the socket is not filtering.**
    ///
    /// The route refuses regardless, so this is about not offering a link that would 404 — but it
    /// is also the page saying, in words, what is in the file. A viewer who cannot have it should
    /// not be left wondering whether it is missing.
    ///
    /// The flag it renders is `visibility == Everything`, so it tracks the room's policy without
    /// the template ever seeing one: `may_download` is decided in the handler, which is the same
    /// rule `SlotView` follows and for the same reason — a template cannot prove it withheld
    /// something.
    #[test]
    fn the_file_is_offered_only_where_nothing_is_being_withheld() {
        use askama::Template;

        let render = |may_download| {
            JournalTemplate {
                base: crate::tpl::TplContext::new(&Session::default()),
                room: crate::routes::rooms::tests::a_room(),
                size: Some(1024 * 1024),
                may_download,
            }
            .render()
            .expect("renders")
        };

        let staff = render(true);
        assert!(
            staff.contains("/download"),
            "a viewer entitled to the whole history is not offered the file"
        );
        assert!(
            staff.contains("chat"),
            "the page does not say what is in the file it is handing over"
        );

        let viewer = render(false);
        assert!(
            !viewer.contains("/download"),
            "a filtered viewer is offered a download carrying exactly what was filtered"
        );
        // The feed itself is still there: that is the whole point of the tier split.
        assert!(viewer.contains("journal-status"));
    }

    /// **A feed link hands over the feed and not the room.**
    ///
    /// The page is addressed by `journal_id`, which is derivable from neither the room's id nor its
    /// tracker's — so this asserts what the markup must not carry. It is the same property M8b
    /// asserts for the tracker, and it is asserted by rendering rather than by reading, because the
    /// leak that matters is whatever reaches the browser: an `href` back to the room, a `data-`
    /// attribute the script reads, a download URL with the id in its path.
    ///
    /// The room's **name** is deliberately present. It tells a viewer which feed they are watching,
    /// it is already on the public room page, and it names nothing that can be navigated to.
    #[test]
    fn the_feed_page_never_names_the_room_it_belongs_to() {
        use askama::Template;

        let room = crate::routes::rooms::tests::a_room();
        let (id, tracker) = (room.id.to_string(), room.tracker_id.to_string());
        let journal = room.journal_id.to_string();

        for may_download in [true, false] {
            let html = JournalTemplate {
                base: crate::tpl::TplContext::new(&Session::default()),
                room: room.clone(),
                size: Some(4096),
                may_download,
            }
            .render()
            .expect("renders");

            assert!(
                !html.contains(&id),
                "the feed page carries the room's id, so sharing the feed shares the room"
            );
            assert!(
                !html.contains("/room/"),
                "the feed page links back to the room it belongs to"
            );
            assert!(
                !html.contains(&tracker),
                "the feed page carries the tracker id, collapsing two separate capabilities"
            );
            // What it must carry: its own id, or nothing on the page can reach the feed.
            assert!(
                html.contains(&journal),
                "the feed page does not carry its own id"
            );
        }
    }

    /// **The saved file does not name the room either.**
    ///
    /// A `Content-Disposition` is the one part of a download that outlives the page and gets
    /// forwarded, so putting `room.id` in it would have handed the room over through the artifact
    /// after the URL had been carefully arranged not to. The room's *name* is deliberately kept: it
    /// is what tells three files apart in a downloads folder, and it names nothing navigable.
    #[test]
    fn the_saved_file_is_named_for_the_room_and_never_addresses_it() {
        let mut room = crate::routes::rooms::tests::a_room();
        room.name = "Friday Async".into();
        let name = download_name(&room);
        assert_eq!(name, "Friday-Async-feed.jsonl");
        assert!(!name.contains(&room.id.to_string()));
        assert!(!name.contains(&room.journal_id.to_string()));

        // The stem is an allowlist with no `.` at all, so `..` and a leading dot are unspellable
        // rather than filtered — the same rule the patch download follows, and for the same reason:
        // this is untrusted text out of a room name heading for a response header.
        room.name = "../../etc/passwd".into();
        let escaped = download_name(&room);
        assert!(!escaped.contains(".."), "{escaped}");
        assert!(!escaped.contains('/'), "{escaped}");
        assert!(escaped.ends_with("-feed.jsonl"), "{escaped}");

        // A room named entirely in punctuation still produces a filename.
        room.name = "!!!".into();
        assert_eq!(download_name(&room), "feed.jsonl");
    }

    #[test]
    fn a_request_degrades_to_the_default_rather_than_failing() {
        let parsed: FeedRequest = serde_json::from_str("{}").expect("empty object");
        assert!(parsed.from.is_none());
        let anchored: FeedRequest =
            serde_json::from_str(r#"{"from":{"at":1787729723.5}}"#).expect("an anchor");
        assert_eq!(anchored.from.unwrap().at, Some(1787729723.5));
        let lines: FeedRequest = serde_json::from_str(r#"{"from":{"lines":50}}"#).expect("lines");
        assert_eq!(lines.from.unwrap().lines, Some(50));

        // What a reconnect sends. The page has this offset already; the wire has to carry it.
        let after: FeedRequest = serde_json::from_str(r#"{"from":{"after":4096}}"#).expect("after");
        assert_eq!(after.from.unwrap().after, Some(4096));
    }

    /// Write `lines` to a journal and hand back the directory holding it.
    fn journal_file(lines: &[String]) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.jsonl");
        let mut file = std::fs::File::create(&path).expect("create");
        for line in lines {
            writeln!(file, "{line}").expect("write");
        }
        (dir, path)
    }

    fn anchor(after: Option<journal::Cursor>, lines: Option<usize>) -> Anchor {
        Anchor {
            lines,
            at: None,
            after,
        }
    }

    /// **The rollout bug, at the level it happens.**
    ///
    /// A reconnect used to ask for a tail, so a page that already showed the last hundred records
    /// got them again and rendered them twice. Confirmed happening on a re-rollout of the web tier,
    /// where it reads as a busy room rather than as a fault — which is why it survived.
    ///
    /// Resuming at the cursor returns **only what arrived while the socket was down**, and `start`
    /// equal to the offset asked for is what tells the page it may keep what it is showing.
    #[test]
    fn a_resume_returns_only_what_arrived_while_the_socket_was_down() {
        let mut lines: Vec<String> = (0..10).map(|n| line(&format!("check{n}"))).collect();
        let (_dir, path) = journal_file(&lines);

        // Where a follower would have been when the pod went away.
        let before = opening_replay(&path, &anchor(None, Some(100))).expect("tail");
        assert_eq!(before.lines.len(), 10);
        let cursor = before.cursor;

        // Three records land during the rollout.
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("reopen");
            for n in 10..13 {
                lines.push(line(&format!("check{n}")));
                writeln!(file, "{}", lines[n]).expect("append");
            }
        }

        let resumed = opening_replay(&path, &anchor(Some(cursor), None)).expect("resume");
        assert_eq!(
            resumed.lines.len(),
            3,
            "a resume re-sent records the page already had"
        );
        assert_eq!(
            resumed.start, cursor,
            "start must equal the offset asked for, or the page throws its content away"
        );
    }

    /// Nothing happened while the socket was down: an empty frame that still joins.
    ///
    /// Worth its own case because it is the common one on a fast rollout, and because an empty
    /// replay must not read as a failed join — `start` still has to come back equal to the ask.
    #[test]
    fn a_resume_with_nothing_new_still_joins() {
        let lines: Vec<String> = (0..5).map(|n| line(&format!("check{n}"))).collect();
        let (_dir, path) = journal_file(&lines);

        let cursor = opening_replay(&path, &anchor(None, Some(100)))
            .expect("tail")
            .cursor;
        let resumed = opening_replay(&path, &anchor(Some(cursor), None)).expect("resume");

        assert!(resumed.lines.is_empty());
        assert_eq!(resumed.start, cursor);
    }

    /// **Both fallbacks report themselves the same way**, which is what lets the page carry one rule
    /// instead of two.
    ///
    /// A gap too large to send in one frame, and a file that was reset under the viewer, are
    /// different causes with the same correct answer: serve the latest records and let the page
    /// replace what it has. Appending either onto a page that kept its lines would leave a hole in
    /// the middle of the feed with nothing marking it.
    #[test]
    fn a_gap_too_large_and_a_reset_file_both_refuse_to_join() {
        // Too large: more than one frame may carry, asked for from the very beginning.
        let many: Vec<String> = (0..journal::MAX_REPLAY_LINES + 50)
            .map(|n| line(&format!("check{n}")))
            .collect();
        let (_dir, path) = journal_file(&many);

        let huge = opening_replay(&path, &anchor(Some(0), None)).expect("oversized resume");
        assert_eq!(
            huge.lines.len(),
            journal::DEFAULT_REPLAY_LINES,
            "an oversized resume serves a tail rather than everything"
        );
        assert_ne!(
            huge.start, 0,
            "and says so by reporting a start other than the offset asked for"
        );

        // Reset: the cursor is past the end of a file that is now shorter. `read_from` already
        // answers this with a tail; what is asserted here is that it is DISTINGUISHABLE.
        let short: Vec<String> = (0..3).map(|n| line(&format!("check{n}"))).collect();
        let (_dir2, short_path) = journal_file(&short);
        let stale = 900_000;
        let reset = opening_replay(&short_path, &anchor(Some(stale), None)).expect("reset");
        assert_eq!(reset.lines.len(), 3);
        assert_ne!(
            reset.start, stale,
            "a reset file must not look like a clean join"
        );
    }
}
