//! The tracker tier: `/tracker/**` and the two proxied JSON endpoints.
//!
//! Mounted only under `PUNA_ROLE=tracker`, which is the same binary in a different run mode. The
//! split exists because this is the most public, highest-volume and least-authenticated surface
//! Puna has, and a spike on it should not be able to degrade room creation, the console, or the
//! OAuth callback.
//!
//! ## The URL shape is the reference's, on purpose
//!
//! Third-party tools are written against `archipelago.gg`'s paths, and pahoa's documents mirror the
//! reference field for field — so mirroring the paths too is what makes those tools work against a
//! Puna room with only a base URL changed. Getting this wrong would throw away the compatibility
//! that made mirroring the documents worthwhile.
//!
//! ## Three cache layers, in the order they remove work
//!
//! 1. **`ETag` and `Cache-Control`**, so a browser stops re-fetching at all. This cuts request
//!    volume at the source and is worth more than any amount of replica scaling.
//! 2. **The shared cache in `rooms.last_tracker_doc`**, honoring pahoa's own windows. Shared
//!    matters: with a per-process cache, adding replicas *multiplies* upstream fetches instead of
//!    amortizing them.
//! 3. **A short in-process memo** over that, so a burst does not become a burst of row reads.
//!
//! The column already existed for the torn-down-room case; using it as the shared cache is reuse
//! rather than a new mechanism.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use puna_core::ids::{RoomId, TrackerId};
use puna_core::model::room::{self, Room, TrackerPolicy};
use puna_core::model::{member, slot, tracker};
use rocket::http::{Header, Status};
use rocket::{Responder, State, get, routes};

use askama::Template;
use askama_web::WebTemplate;

use crate::auth::Session;
use crate::error::{Error, Result, forbidden, not_found, unauthorized};
use crate::params::TrackerParam;
use crate::tpl::TplContext;
use crate::upstream::{Document, Upstream, UpstreamError};

type Pool = puna_core::db::Pool;

/// How long a document is held in this process before the shared cache is consulted again.
///
/// Deliberately short. It exists to absorb a burst — a page load fetching both documents, twenty
/// tabs refreshing at once — not to add staleness on top of the shared cache's.
const MEMO_TTL: Duration = Duration::from_secs(5);

/// The in-process layer.
#[derive(Default)]
pub struct Memo {
    entries: Mutex<HashMap<(RoomId, Document), (Instant, String)>>,
}

impl Memo {
    fn get(&self, key: (RoomId, Document)) -> Option<String> {
        let entries = self.entries.lock().ok()?;
        let (at, body) = entries.get(&key)?;
        (at.elapsed() < MEMO_TTL).then(|| body.clone())
    }

    fn put(&self, key: (RoomId, Document), body: String) {
        if let Ok(mut entries) = self.entries.lock() {
            // Unbounded in principle, bounded in practice by the room count and swept by age on
            // read. A room that stops being asked about holds one entry until the process restarts;
            // at a few hundred rooms that is not worth an eviction policy.
            entries.insert(key, (Instant::now(), body));
        }
    }
}

/// The largest document that will be written to the shared cache, in bytes.
pub struct TrackerCacheMax(pub usize);

/// The three pieces of Rocket state every tracker handler needs, as one guard.
///
/// Threading them individually made every handler take eight arguments, which is both unreadable
/// and the kind of list where two same-typed parameters get swapped. A guard is what Rocket offers
/// for exactly this.
pub struct TrackerState<'r> {
    upstream: &'r Upstream,
    memo: &'r Memo,
    cache_max: usize,
}

#[rocket::async_trait]
impl<'r> rocket::request::FromRequest<'r> for TrackerState<'r> {
    type Error = Error;

    async fn from_request(
        request: &'r rocket::Request<'_>,
    ) -> rocket::request::Outcome<Self, Self::Error> {
        let missing = |what: &str| {
            Error::new(
                Status::InternalServerError,
                anyhow::anyhow!("no {what} in Rocket state"),
            )
        };

        let (Some(upstream), Some(memo), Some(cache_max)) = (
            request.guard::<&State<Upstream>>().await.succeeded(),
            request.guard::<&State<Memo>>().await.succeeded(),
            request.guard::<&State<TrackerCacheMax>>().await.succeeded(),
        ) else {
            let e = missing("tracker state");
            return rocket::outcome::Outcome::Error((e.status, e));
        };

        rocket::outcome::Outcome::Success(TrackerState {
            upstream: upstream.inner(),
            memo: memo.inner(),
            cache_max: cache_max.0,
        })
    }
}

/// A JSON document, with the caching headers that make the first layer work.
#[derive(Responder)]
#[response(content_type = "application/json")]
struct Cached {
    body: String,
    etag: Header<'static>,
    cache_control: Header<'static>,
}

/// A `304`, which is the whole point of having sent an `ETag`.
#[derive(Responder)]
#[response(status = 304)]
struct NotModified(());

#[derive(Responder)]
enum Json {
    Body(Box<Cached>),
    Unchanged(NotModified),
}

/// What a request resolved to: which room, which slot if any, and what it may read.
struct Access {
    room: Room,
    target: tracker::Target,
}

/// Resolve the id, then decide whether this viewer may see it.
///
/// Order matters: a `disabled` tracker and an id that names nothing must be **indistinguishable**,
/// so both answer `404`. Anything else lets someone probe which unguessable ids are real.
async fn access(
    conn: &mut diesel_async::AsyncPgConnection,
    session: &Session,
    id: TrackerId,
) -> Result<Access> {
    let target = tracker::resolve(conn, id)
        .await?
        .ok_or_else(|| not_found("no such tracker"))?;

    let room = room::get(conn, target.room_id())
        .await?
        .ok_or_else(|| not_found("no such tracker"))?;

    if room.tracker_policy == TrackerPolicy::Disabled {
        return Err(not_found("no such tracker"));
    }

    let is_staff = if session.is_admin {
        true
    } else if let Some(user_id) = session.user_id {
        member::role_of(conn, room.id, user_id).await?.is_some()
    } else {
        false
    };
    let owns_a_slot = match session.user_id {
        Some(user_id) => slot::list(conn, room.id)
            .await?
            .iter()
            .any(|s| s.owner_id == Some(user_id)),
        None => false,
    };

    if !room::may_see_tracker(room.tracker_policy, is_staff, owns_a_slot) {
        return Err(if session.user_id.is_none() {
            // The tracker tier initiates no login of its own -- it holds no Discord credentials --
            // but the 401 catcher redirects to the web tier's `/auth/login` on the same hostname,
            // which is why both roles share one `ROCKET_SECRET_KEY` and only one has the secrets.
            unauthorized("log in to see this tracker")
        } else {
            forbidden("this tracker is limited to the room's members")
        });
    }

    Ok(Access { room, target })
}

/// The live document.
#[get("/api/tracker/<id>")]
async fn live(
    id: TrackerParam,
    session: Session,
    conditional: IfNoneMatch,
    pool: &State<Pool>,
    state: TrackerState<'_>,
) -> Result<Json> {
    document(id, session, conditional, pool, state, Document::Live).await
}

/// The static document: games, location totals, datapackage checksums.
#[get("/api/static_tracker/<id>")]
async fn statics(
    id: TrackerParam,
    session: Session,
    conditional: IfNoneMatch,
    pool: &State<Pool>,
    state: TrackerState<'_>,
) -> Result<Json> {
    document(id, session, conditional, pool, state, Document::Static).await
}

async fn document(
    id: TrackerParam,
    session: Session,
    conditional: IfNoneMatch,
    pool: &State<Pool>,
    state: TrackerState<'_>,
    which: Document,
) -> Result<Json> {
    let mut conn = pool.get().await?;
    let access = access(&mut conn, &session, id.0).await?;

    // Scoping happens after every cache layer, so the caches hold ONE document per room per kind
    // and a slot view is a projection of it -- rather than one cached document per slot, which
    // would multiply both the upstream fetches and the memory by the room's slot count.
    let scope = access.target.slot_number();
    let fetched = obtain(&mut conn, &state, &access.room, which).await?;

    Ok(respond(project(fetched.body, scope), which, &conditional))
}

/// One document, from whichever layer has it.
struct Fetched {
    body: String,
    /// When this was true, if it is no longer. `Some` means **the room did not answer** and this is
    /// the last thing it said — which for an async is most of its life, and is exactly what the
    /// page's "as of" banner reports.
    stale_since: Option<chrono::DateTime<chrono::Utc>>,
}

async fn obtain(
    conn: &mut diesel_async::AsyncPgConnection,
    state: &TrackerState<'_>,
    room: &Room,
    which: Document,
) -> Result<Fetched> {
    let (upstream, memo, cache_max) = (state.upstream, state.memo, state.cache_max);
    // Layer 3: this process, five seconds.
    if let Some(body) = memo.get((room.id, which)) {
        return Ok(Fetched {
            body,
            stale_since: None,
        });
    }

    // Layer 2: the shared cache, honoring pahoa's own window.
    let cached = tracker::cached(conn, room.id).await?;
    let fresh = cached.as_ref().is_some_and(|c| {
        chrono::Utc::now()
            .signed_duration_since(c.at)
            .to_std()
            .ok()
            .is_some_and(|age| age < which.ttl())
    });

    if let Some(cache) = &cached
        && fresh
        && let Some(value) = pick(cache, which)
    {
        let body = value.to_string();
        memo.put((room.id, which), body.clone());
        return Ok(Fetched {
            body,
            stale_since: None,
        });
    }

    // Nothing fresh: ask the room.
    match fetch(conn, upstream, room, which, cache_max).await {
        Ok(value) => {
            let body = value.to_string();
            memo.put((room.id, which), body.clone());
            Ok(Fetched {
                body,
                stale_since: None,
            })
        }
        Err(e) => {
            // **The torn-down room.** Serving the last known document is the whole reason the
            // column exists; the page says how old it is, and deliberately offers **no start
            // button** -- a tracker's audience is not necessarily authorized to provision a pod,
            // and a widely-shared link that spins up compute is the hazard D8 exists to prevent.
            if let Some(cache) = &cached
                && let Some(stale) = pick(cache, which)
            {
                tracing::debug!(
                    room = %room.id,
                    document = which.as_str(),
                    error = %e,
                    "serving a stale tracker document"
                );
                return Ok(Fetched {
                    body: stale.to_string(),
                    stale_since: Some(cache.at),
                });
            }
            Err(unreachable_room(e))
        }
    }
}

// ---- the page ---------------------------------------------------------------------------------

#[derive(Template, WebTemplate)]
#[template(path = "tracker/show.html")]
pub struct TrackerTemplate {
    base: TplContext,
    /// The room's name. **Not its id**, and not its address: the page identifies the multiworld to
    /// somebody who was given the link, and identifies it to nobody else.
    room_name: String,
    /// Set when this is one slot's view, whether reached by the slot's own id or by the
    /// reference-compatible `/<team>/<player>` path.
    slot_name: Option<String>,
    rows: Vec<TrackerRow>,
    /// `Some` when the room did not answer and this is the last thing it said.
    as_of: Option<String>,
    /// How often the page reloads itself, in seconds. Matched to the document's own cache window:
    /// refreshing faster than the upstream can change is work that buys nothing.
    refresh_secs: u64,
}

/// One row of the slot table.
pub struct TrackerRow {
    pub slot_number: i32,
    pub player_name: String,
    pub game: String,
    pub is_spectator: bool,
    pub checks_done: usize,
    pub checks_total: i64,
    pub percent: i64,
    pub status: &'static str,
    pub last_activity: String,
    pub hints: usize,
    /// Whether somebody has claimed this slot in Puna. **The reference cannot show this**, because
    /// it does not know who is playing — only that a slot exists.
    pub claimed: bool,
}

/// The multiworld's tracker, or one slot's.
#[get("/tracker/<id>")]
async fn page(
    id: TrackerParam,
    session: Session,
    pool: &State<Pool>,
    state: TrackerState<'_>,
) -> Result<TrackerTemplate> {
    let mut conn = pool.get().await?;
    let access = access(&mut conn, &session, id.0).await?;
    let scope = access.target.slot_number();
    render(&mut conn, &session, &state, access, scope).await
}

/// The reference implementation's per-slot URL, so tools that construct it keep working.
///
/// It leaks nothing new: anyone who can build this path already holds the multiworld's tracker id.
/// The *other* per-slot form -- a slot's own id -- is the one that discloses nothing about the room,
/// and both render the same page.
#[get("/tracker/<id>/<team>/<player>")]
async fn slot_page(
    id: TrackerParam,
    team: i32,
    player: i32,
    session: Session,
    pool: &State<Pool>,
    state: TrackerState<'_>,
) -> Result<TrackerTemplate> {
    let mut conn = pool.get().await?;
    let access = access(&mut conn, &session, id.0).await?;

    // Pahoa rooms are single-team, as the reference's own default is. Accepting only team 0 keeps
    // the URL honest rather than silently ignoring a segment somebody meant.
    if team != 0 {
        return Err(not_found("no such team"));
    }
    // A slot's own id already names its slot; combining it with a different one would be two
    // answers to one question.
    if let Some(own) = access.target.slot_number()
        && own != player
    {
        return Err(not_found("no such slot"));
    }

    render(&mut conn, &session, &state, access, Some(player)).await
}

async fn render(
    conn: &mut diesel_async::AsyncPgConnection,
    session: &Session,
    state: &TrackerState<'_>,
    access: Access,
    scope: Option<i32>,
) -> Result<TrackerTemplate> {
    // Both documents, because a slot table needs progress from one and games and totals from the
    // other. Each goes through the same three cache layers, so the common case costs no upstream
    // call at all.
    let live = obtain(conn, state, &access.room, Document::Live).await?;
    let statics = obtain(conn, state, &access.room, Document::Static).await?;

    let slots = slot::list(conn, access.room.id).await?;
    let live_doc: serde_json::Value = serde_json::from_str(&live.body).unwrap_or_default();
    let static_doc: serde_json::Value = serde_json::from_str(&statics.body).unwrap_or_default();

    let mut rows = rows(&slots, &live_doc, &static_doc);
    let mut slot_name = None;
    if let Some(slot_number) = scope {
        rows.retain(|row| row.slot_number == slot_number);
        slot_name = rows.first().map(|row| row.player_name.clone());
    }

    Ok(TrackerTemplate {
        base: TplContext::new(session),
        room_name: access.room.name.clone(),
        slot_name,
        rows,
        // The older of the two, because a page is only as current as its stalest half.
        as_of: live
            .stale_since
            .into_iter()
            .chain(statics.stale_since)
            .min()
            .map(|at| format!("{}", at.format("%Y-%m-%d %H:%M UTC"))),
        refresh_secs: Document::Live.ttl().as_secs(),
    })
}

/// Merge Puna's slot list with the room's two documents.
///
/// **Puna's list leads.** The documents describe only what pahoa knows about — and a spectator has
/// no progress to report, so it appears in neither per-player array — but a spectator is still a
/// slot somebody claimed, and a tracker that silently omitted it would be describing a different
/// room from the one on the room page.
fn rows(
    slots: &[puna_core::model::slot::Slot],
    live: &serde_json::Value,
    statics: &serde_json::Value,
) -> Vec<TrackerRow> {
    slots
        .iter()
        .map(|slot| {
            let n = i64::from(slot.slot_number);
            let checks_done = array_for(live, "player_checks_done", n)
                .and_then(|entry| entry.get("locations").and_then(|l| l.as_array()))
                .map_or(0, Vec::len);
            let checks_total = array_for(statics, "player_locations_total", n)
                .and_then(|entry| {
                    entry
                        .get("total_locations")
                        .and_then(serde_json::Value::as_i64)
                })
                .unwrap_or(0);
            let hints = array_for(live, "hints", n)
                .and_then(|entry| entry.get("hints").and_then(|h| h.as_array()))
                .map_or(0, Vec::len);

            TrackerRow {
                slot_number: slot.slot_number,
                // From Puna's row, not the document: the document's `alias` is whatever the client
                // last called itself, and the roster is what the room page shows.
                player_name: slot.player_name.clone(),
                game: array_for(statics, "player_game", n)
                    .and_then(|entry| entry.get("game").and_then(|g| g.as_str()))
                    .unwrap_or(&slot.game)
                    .to_string(),
                is_spectator: slot.kind == puna_core::artifact::SlotKind::Spectator,
                checks_done,
                checks_total,
                percent: if checks_total > 0 {
                    (checks_done as i64 * 100 / checks_total).clamp(0, 100)
                } else {
                    0
                },
                status: status_word(
                    array_for(live, "player_status", n)
                        .and_then(|entry| entry.get("status").and_then(serde_json::Value::as_i64)),
                ),
                last_activity: activity(
                    array_for(live, "activity_timers", n)
                        .and_then(|entry| entry.get("time").and_then(|t| t.as_str())),
                ),
                hints,
                claimed: slot.owner_id.is_some(),
            }
        })
        .collect()
}

/// The entry for one slot in one of the document's per-player arrays.
fn array_for<'a>(
    document: &'a serde_json::Value,
    key: &str,
    slot_number: i64,
) -> Option<&'a serde_json::Value> {
    document
        .get(key)?
        .as_array()?
        .iter()
        .find(|entry| entry.get("player").and_then(serde_json::Value::as_i64) == Some(slot_number))
}

/// Archipelago's `ClientStatus`, in words.
///
/// The numbers are the protocol's and are sparse (0, 5, 10, 20, 30) because the reference leaves
/// room between them. An unknown value renders as "unknown" rather than as itself: a number in this
/// column would mean nothing to the person reading it.
fn status_word(status: Option<i64>) -> &'static str {
    match status {
        Some(5) => "connected",
        Some(10) => "ready",
        Some(20) => "playing",
        Some(30) => "goal",
        _ => "unknown",
    }
}

/// An RFC 1123 timestamp, as an age.
///
/// **`null` means never, and never is not 1970.** A slot that has genuinely not acted reports null,
/// which the reference renders as nothing at all — and rendering it as an epoch date is the classic
/// way to make an untouched slot look like an abandoned one.
fn activity(time: Option<&str>) -> String {
    let Some(time) = time else {
        return "never".to_string();
    };
    let Ok(at) = chrono::DateTime::parse_from_rfc2822(time) else {
        return "unknown".to_string();
    };

    let age = chrono::Utc::now().signed_duration_since(at.with_timezone(&chrono::Utc));
    let minutes = age.num_minutes();
    match minutes {
        ..1 => "just now".to_string(),
        1..60 => format!("{minutes}m ago"),
        60..2880 => format!("{}h ago", age.num_hours()),
        _ => format!("{}d ago", age.num_days()),
    }
}

/// Ask the room, and write what comes back into the shared cache.
async fn fetch(
    conn: &mut diesel_async::AsyncPgConnection,
    upstream: &Upstream,
    room: &Room,
    which: Document,
    cache_max: usize,
) -> std::result::Result<serde_json::Value, UpstreamError> {
    let base_port = puna_core::model::port::reserved_pair(conn, room.id)
        .await
        .ok()
        .flatten()
        .ok_or(UpstreamError::NoAddress)?;

    let secrets = room::secrets(conn, room.id)
        .await
        .ok()
        .flatten()
        .ok_or(UpstreamError::NoAddress)?;

    let value = upstream
        .fetch(room.id, base_port, &secrets.admin_token, which)
        .await?;

    let (live, statics) = match which {
        Document::Live => (Some(&value), None),
        Document::Static => (None, Some(&value)),
    };
    // Merged with whatever the other key already held, so fetching one document does not evict the
    // other -- a room with a cached live document and no static one renders a table with no games.
    let existing = tracker::cached(conn, room.id).await.ok().flatten();
    let live = live.or(existing.as_ref().and_then(|c| c.live.as_ref()));
    let statics = statics.or(existing.as_ref().and_then(|c| c.statics.as_ref()));

    if let Ok(false) = tracker::store(conn, room.id, live, statics, cache_max).await {
        tracing::warn!(
            room = %room.id,
            "this room's tracker document is too large for the shared cache; it will be fetched \
             from the room every time and will not survive a teardown"
        );
    }

    Ok(value)
}

fn pick(cache: &tracker::CachedDocuments, which: Document) -> Option<serde_json::Value> {
    match which {
        Document::Live => cache.live.clone(),
        Document::Static => cache.statics.clone(),
    }
}

/// Every per-player array in either document, keyed by `player`.
///
/// Transcribed from pahoa's renderer rather than discovered by inspection, because a key this misses
/// is a key that passes through unscoped — the failure is silent and in the wrong direction.
const PER_PLAYER_KEYS: &[&str] = &[
    // live
    "aliases",
    "player_items_received",
    "player_checks_done",
    "hints",
    "activity_timers",
    "connection_timers",
    "player_status",
    // static
    "player_locations_total",
    "player_game",
];

/// Keys that describe the multiworld rather than any player, and are kept as they are.
///
/// `total_checks_done` is an aggregate per team, and `datapackage` is a checksum manifest a client
/// needs to render item names at all. Neither says anything about who else is in the room.
const AGGREGATE_KEYS: &[&str] = &["total_checks_done", "datapackage"];

/// Apply the slot scope, if there is one.
///
/// Takes and returns the rendered body rather than a `Value`, so the whole-multiworld case — which
/// is the common one — costs nothing at all: no parse, no re-render, just the string the cache
/// already holds.
fn project(body: String, scope: Option<i32>) -> String {
    let Some(slot_number) = scope else {
        return body;
    };
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(document) => scope_to_slot(&document, slot_number).to_string(),
        // Unparseable means the cache holds something this build cannot read. Serving `{}` is the
        // fail-closed answer: a slot link must never widen into the multiworld's document because
        // the projection could not be applied.
        Err(_) => "{}".to_string(),
    }
}

/// Narrow a document to one slot.
///
/// **This is what makes a slot's tracker id worth having.** The id is independent of the room's so a
/// player can share their own tracker without handing out the multiworld's — and that promise is
/// only kept if the document behind it is theirs too. Everything keyed by player is filtered to that
/// player; aggregates stay; anything unrecognized is **dropped**, because a key added upstream that
/// this code has never seen is a key it cannot know is safe to forward.
fn scope_to_slot(document: &serde_json::Value, slot_number: i32) -> serde_json::Value {
    let Some(object) = document.as_object() else {
        return serde_json::Value::Object(serde_json::Map::new());
    };

    let mut out = serde_json::Map::new();
    for (key, value) in object {
        if AGGREGATE_KEYS.contains(&key.as_str()) {
            out.insert(key.clone(), value.clone());
        } else if PER_PLAYER_KEYS.contains(&key.as_str()) {
            let kept: Vec<serde_json::Value> = value
                .as_array()
                .map(|entries| {
                    entries
                        .iter()
                        .filter(|entry| {
                            entry.get("player").and_then(serde_json::Value::as_i64)
                                == Some(i64::from(slot_number))
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            out.insert(key.clone(), serde_json::Value::Array(kept));
        }
        // Anything else -- `groups`, or a key a later pahoa adds -- is left out. A slot's view
        // showing the item-link groups would name other slots, which is the one thing it is for
        // not doing.
    }

    serde_json::Value::Object(out)
}

/// A room that cannot be reached, mapped to something a caller can act on.
fn unreachable_room(e: UpstreamError) -> Error {
    match e {
        // A `404` from a room means no admin token is configured there -- pahoa answers `404`
        // rather than `401` precisely so this is distinguishable -- which means the Secret did not
        // arrive. That is a Puna fault, not a caller's.
        UpstreamError::Status { status: 404 } => Error::new(
            Status::BadGateway,
            anyhow::anyhow!(
                "the room has no admin token configured; its Secret may not have arrived"
            ),
        ),
        UpstreamError::NoAddress => Error::new(
            Status::ServiceUnavailable,
            anyhow::anyhow!("this room has never been started, so there is nothing to track yet"),
        ),
        other => Error::new(Status::ServiceUnavailable, other.into()),
    }
}

/// `ETag` over the body, plus the window pahoa itself would have served.
fn respond(body: String, which: Document, conditional: &IfNoneMatch) -> Json {
    let etag = format!("\"{}\"", puna_core::hash::sha256_hex(body.as_bytes()));

    if conditional.0.as_deref() == Some(etag.as_str()) {
        return Json::Unchanged(NotModified(()));
    }

    Json::Body(Box::new(Cached {
        body,
        etag: Header::new("ETag", etag),
        // `private` because a `members`-policy tracker is per-viewer, and a shared cache in front of
        // this must not hand one viewer's document to another.
        cache_control: Header::new(
            "Cache-Control",
            format!("private, max-age={}", which.ttl().as_secs()),
        ),
    }))
}

/// The `If-None-Match` header, as a guard so a handler cannot forget to read it.
pub struct IfNoneMatch(pub Option<String>);

#[rocket::async_trait]
impl<'r> rocket::request::FromRequest<'r> for IfNoneMatch {
    type Error = std::convert::Infallible;

    async fn from_request(
        request: &'r rocket::Request<'_>,
    ) -> rocket::request::Outcome<Self, Self::Error> {
        rocket::outcome::Outcome::Success(IfNoneMatch(
            request
                .headers()
                .get_one("If-None-Match")
                .map(str::to_string),
        ))
    }
}

pub fn routes() -> Vec<rocket::Route> {
    routes![page, slot_page, live, statics]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memo_expires() {
        let memo = Memo::default();
        let key = (RoomId::new(), Document::Live);

        assert_eq!(memo.get(key), None);
        memo.put(key, "{}".to_string());
        assert_eq!(memo.get(key).as_deref(), Some("{}"));

        // Expiry is by age, so a stale entry is dropped on read rather than by a sweep.
        let mut entries = memo.entries.lock().expect("lock");
        let entry = entries.get_mut(&key).expect("the entry");
        entry.0 = Instant::now() - MEMO_TTL - Duration::from_secs(1);
        drop(entries);
        assert_eq!(memo.get(key), None);
    }

    /// The two documents are memoized separately: fetching one must not serve it as the other.
    #[test]
    fn the_two_documents_do_not_share_a_memo_entry() {
        let memo = Memo::default();
        let room = RoomId::new();
        memo.put((room, Document::Live), "live".to_string());

        assert_eq!(memo.get((room, Document::Live)).as_deref(), Some("live"));
        assert_eq!(memo.get((room, Document::Static)), None);
    }

    /// A whole multiworld document, in pahoa's shape.
    fn multiworld() -> serde_json::Value {
        serde_json::json!({
            "aliases": [
                {"team": 0, "player": 1, "alias": "Troy"},
                {"team": 0, "player": 2, "alias": "Someone Else"},
            ],
            "player_checks_done": [
                {"team": 0, "player": 1, "locations": [1, 2, 3]},
                {"team": 0, "player": 2, "locations": [4]},
            ],
            "hints": [
                {"team": 0, "player": 1, "hints": [[1, 2, 100, 200, false, "", 0, 0]]},
                {"team": 0, "player": 2, "hints": []},
            ],
            "activity_timers": [
                {"team": 0, "player": 1, "time": "Mon, 17 Aug 2026 18:22:09 GMT"},
                {"team": 0, "player": 2, "time": null},
            ],
            "total_checks_done": [{"team": 0, "checks_done": 4}],
            "groups": [{"slot": 3, "name": "Swords", "members": [1, 2]}],
        })
    }

    /// The promise a slot's tracker id makes: share your own progress without handing over the
    /// multiworld's.
    #[test]
    fn a_slot_view_shows_that_slot_and_no_other() {
        let scoped = scope_to_slot(&multiworld(), 1);

        assert_eq!(scoped["aliases"].as_array().unwrap().len(), 1);
        assert_eq!(scoped["aliases"][0]["alias"], "Troy");
        assert_eq!(
            scoped["player_checks_done"][0]["locations"],
            serde_json::json!([1, 2, 3])
        );
        assert_eq!(scoped["hints"].as_array().unwrap().len(), 1);
        assert_eq!(scoped["activity_timers"].as_array().unwrap().len(), 1);

        // Nobody else's name is anywhere in the rendered document -- which is the assertion that
        // actually matters, because it holds however the filtering is implemented.
        let rendered = scoped.to_string();
        assert!(!rendered.contains("Someone Else"), "{rendered}");
    }

    /// Aggregates stay: a number about the whole room says nothing about who is in it.
    #[test]
    fn aggregates_survive_scoping() {
        let scoped = scope_to_slot(&multiworld(), 1);
        assert_eq!(scoped["total_checks_done"][0]["checks_done"], 4);
    }

    /// Item-link groups name their members, so a scoped view drops them -- along with any key a
    /// later pahoa adds that this build has never seen.
    #[test]
    fn unrecognized_and_membership_keys_are_dropped_rather_than_forwarded() {
        let mut document = multiworld();
        document["something_added_later"] = serde_json::json!([{"team": 0, "player": 2, "x": 1}]);

        let scoped = scope_to_slot(&document, 1);
        assert!(scoped.get("groups").is_none(), "groups name other slots");
        assert!(
            scoped.get("something_added_later").is_none(),
            "a key this build has never seen cannot be known to be safe to forward"
        );
    }

    /// The whole-room case must not pay for the projection, and the slot case must not be able to
    /// fall back to the whole room.
    #[test]
    fn projection_is_a_no_op_without_a_scope_and_fails_closed_with_one() {
        let body = multiworld().to_string();
        assert_eq!(project(body.clone(), None), body);

        let scoped = project(body, Some(1));
        assert!(!scoped.contains("Someone Else"));

        // A cached document this build cannot parse must not widen into the multiworld's.
        assert_eq!(project("not json".to_string(), Some(1)), "{}");
        assert_eq!(project("not json".to_string(), None), "not json");
    }

    fn slots() -> Vec<puna_core::model::slot::Slot> {
        use puna_core::artifact::SlotKind;
        use puna_core::ids::TrackerId;

        let room_id = RoomId::new();
        vec![
            puna_core::model::slot::Slot {
                room_id,
                slot_number: 1,
                player_name: "Troy".into(),
                game: "A Link to the Past".into(),
                kind: SlotKind::Player,
                password: Some("a-secret".into()),
                owner_id: Some(7),
                claim_token: Some("a-claim-token".into()),
                claimed_at: None,
                tracker_id: TrackerId::new(),
            },
            puna_core::model::slot::Slot {
                room_id,
                slot_number: 4,
                player_name: "Watcher".into(),
                game: "Archipelago".into(),
                kind: SlotKind::Spectator,
                password: None,
                owner_id: None,
                claim_token: Some("another-token".into()),
                claimed_at: None,
                tracker_id: TrackerId::new(),
            },
        ]
    }

    fn statics() -> serde_json::Value {
        serde_json::json!({
            "player_game": [{"team": 0, "player": 1, "game": "A Link to the Past"}],
            "player_locations_total": [{"team": 0, "player": 1, "total_locations": 216}],
        })
    }

    #[test]
    fn a_row_is_built_from_both_documents_and_punas_own_roster() {
        let rows = rows(&slots(), &multiworld(), &statics());

        assert_eq!(
            rows.len(),
            2,
            "Puna's roster leads, so the spectator is here"
        );

        let player = &rows[0];
        assert_eq!(player.player_name, "Troy");
        assert_eq!((player.checks_done, player.checks_total), (3, 216));
        assert_eq!(player.percent, 1);
        assert_eq!(player.hints, 1);
        assert!(
            player.claimed,
            "claim state is what the reference cannot show"
        );

        // A spectator appears in neither per-player array, and must not therefore read as a player
        // who has done nothing.
        let spectator = &rows[1];
        assert!(spectator.is_spectator);
        assert_eq!(spectator.checks_total, 0);
        assert!(!spectator.claimed);
    }

    /// **The property this whole tier exists for.** A tracker link is the one meant for broad
    /// sharing, so the page must give away neither the multiworld's address nor the room's URL.
    #[test]
    fn the_rendered_page_leaks_neither_the_address_nor_the_room() {
        let room_id = RoomId::new();
        let page = TrackerTemplate {
            base: TplContext {
                is_logged_in: false,
                is_admin: false,
                username: String::new(),
                version: "test",
                static_version: "test",
            },
            room_name: "Friday async".into(),
            slot_name: None,
            rows: rows(&slots(), &multiworld(), &statics()),
            as_of: Some("2026-08-19 16:00 UTC".into()),
            refresh_secs: 60,
        };

        let html = page.render().expect("renders");

        assert!(
            !html.contains(&room_id.to_string()),
            "the room id is in the page"
        );
        assert!(!html.contains("/room/"), "a link back to the room page");
        assert!(!html.contains("mw."), "the advertised hostname");
        // No `host:port` in any shape: the ports are five digits in the 40000-49998 range.
        assert!(
            !regex_free_port_like(&html),
            "something that reads as an address: {html}"
        );

        // And no credential from the slot rows, which carry a password and a claim token.
        assert!(!html.contains("a-secret"));
        assert!(!html.contains("a-claim-token"));

        // What it *does* say.
        assert!(html.contains("Friday async"));
        assert!(html.contains("Troy"));
        assert!(html.contains("2026-08-19 16:00 UTC"), "the as-of banner");
        // No start button, however the room is doing: a tracker's audience is not necessarily
        // authorized to provision a pod, and a widely-shared link that spins up compute is exactly
        // the hazard D8 exists to prevent.
        assert!(
            !html.contains("/start"),
            "a start control reached the tracker"
        );
    }

    /// A crude port-shaped-number check, so the leak test does not need a regex dependency.
    fn regex_free_port_like(html: &str) -> bool {
        html.split(|c: char| !c.is_ascii_digit())
            .filter_map(|run| run.parse::<u32>().ok())
            .any(|n| (40000..=49999).contains(&n))
    }

    #[test]
    fn statuses_and_activity_read_as_words() {
        assert_eq!(status_word(Some(30)), "goal");
        assert_eq!(status_word(Some(20)), "playing");
        // A value from a newer protocol renders as a word, never as itself: a number in that column
        // would mean nothing to the person reading it.
        assert_eq!(status_word(Some(99)), "unknown");
        assert_eq!(status_word(None), "unknown");

        // `null` is never, and never is not 1970 -- rendering an epoch date would make an untouched
        // slot look like an abandoned one.
        assert_eq!(activity(None), "never");
        assert_eq!(activity(Some("not a date")), "unknown");

        // pahoa emits RFC 1123 with a `GMT` zone, which is what this has to parse.
        let hour_ago = chrono::Utc::now() - chrono::TimeDelta::hours(1);
        let stamp = hour_ago.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        assert_eq!(activity(Some(&stamp)), "1h ago");
    }

    /// A caller presenting the current ETag gets a 304 and no body, which is the layer that removes
    /// the most work.
    #[test]
    fn a_matching_etag_is_answered_with_304() {
        let body = r#"{"hints":[]}"#.to_string();
        let etag = format!("\"{}\"", puna_core::hash::sha256_hex(body.as_bytes()));

        assert!(matches!(
            respond(body.clone(), Document::Live, &IfNoneMatch(Some(etag))),
            Json::Unchanged(_)
        ));
        assert!(matches!(
            respond(body.clone(), Document::Live, &IfNoneMatch(None)),
            Json::Body(_)
        ));
        // A stale ETag is a full response, not a 304.
        assert!(matches!(
            respond(body, Document::Live, &IfNoneMatch(Some("\"old\"".into()))),
            Json::Body(_)
        ));
    }
}
