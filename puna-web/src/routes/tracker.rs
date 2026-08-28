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

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use puna_core::artifact::names::GameNames;
use puna_core::ids::{GenerationId, RoomId, TrackerId};
use puna_core::model::room::{self, Room, TrackerPolicy};
use puna_core::model::{member, names, slot, tracker};
use rocket::http::{Header, Status};
use rocket::{Responder, State, get, routes};

use askama::Template;
use askama_web::WebTemplate;

use crate::auth::Session;
use crate::digest;
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
            // **Expired entries are dropped here, not merely ignored on read.** The comment this
            // replaces argued that a room nobody asks about any more could keep its entry until the
            // process restarts, because "at a few hundred rooms that is not worth an eviction
            // policy" -- which quietly assumed a small document. A 2000-slot room's is 17.6 MiB, so
            // ten rooms that were browsed once is 350 MiB of a 768 Mi limit held for nothing. The
            // sweep is O(rooms) over a map with one entry per room per kind, on a path that only
            // runs on a cache miss.
            entries.retain(|_, (at, _)| at.elapsed() < MEMO_TTL);
            entries.insert(key, (Instant::now(), body));
        }
    }
}

/// The largest document that will be written to the shared cache, in bytes.
pub struct TrackerCacheMax(pub usize);

/// How long a generation's name tables are held in this process.
///
/// They are **static per generation** — the seed cannot change — so this could be forever. It is
/// not, because an admin rebuild repairs a bad cache and a process that never re-read would ignore
/// the repair until it restarted. Ten minutes makes the fix land on its own.
const NAMES_TTL: Duration = Duration::from_secs(600);

/// Every game's names for one generation, shared between the requests that need them.
type Games = Arc<BTreeMap<String, GameNames>>;

/// Item and location names, per generation, held in this process.
///
/// **Worth having rather than querying per request**, and by a wide margin: measured on a real
/// seed, one generation's tables are ~2.7 MB across 54 games. Reading that from Postgres on every
/// poll of every tab would put the tracker tier's whole reason for existing — absorbing the most
/// public, highest-volume surface Puna has — straight onto the database instead.
#[derive(Default)]
pub struct NameCache {
    entries: Mutex<HashMap<GenerationId, (Instant, Games)>>,
}

impl NameCache {
    fn get(&self, generation: GenerationId) -> Option<Games> {
        let entries = self.entries.lock().ok()?;
        let (at, names) = entries.get(&generation)?;
        (at.elapsed() < NAMES_TTL).then(|| Arc::clone(names))
    }

    fn put(&self, generation: GenerationId, names: Games) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(generation, (Instant::now(), names));
        }
    }
}

/// This generation's names, from the process cache or the database.
///
/// **An empty map is a valid answer**, not an error: a generation ingested before the name cache
/// existed has no rows, and the digest renders raw ids for it. Failing here would turn a cosmetic
/// gap into a dead tracker.
pub(crate) async fn names_for(
    conn: &mut diesel_async::AsyncPgConnection,
    cache: &NameCache,
    generation: GenerationId,
) -> Result<Games> {
    if let Some(names) = cache.get(generation) {
        return Ok(names);
    }

    let loaded = Arc::new(names::all_games(conn, generation).await?);
    if loaded.is_empty() {
        tracing::warn!(
            %generation,
            "no cached names for this generation; the tracker will render raw ids. Run \
             POST /admin/generations/rebuild-names on the web tier."
        );
    }
    cache.put(generation, Arc::clone(&loaded));
    Ok(loaded)
}

/// The three pieces of Rocket state every tracker handler needs, as one guard.
///
/// Threading them individually made every handler take eight arguments, which is both unreadable
/// and the kind of list where two same-typed parameters get swapped. A guard is what Rocket offers
/// for exactly this.
pub struct TrackerState<'r> {
    upstream: &'r Upstream,
    memo: &'r Memo,
    names: &'r NameCache,
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

        let (Some(upstream), Some(memo), Some(names), Some(cache_max)) = (
            request.guard::<&State<Upstream>>().await.succeeded(),
            request.guard::<&State<Memo>>().await.succeeded(),
            request.guard::<&State<NameCache>>().await.succeeded(),
            request.guard::<&State<TrackerCacheMax>>().await.succeeded(),
        ) else {
            let e = missing("tracker state");
            return rocket::outcome::Outcome::Error((e.status, e));
        };

        rocket::outcome::Outcome::Success(TrackerState {
            upstream: upstream.inner(),
            memo: memo.inner(),
            names: names.inner(),
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

/// The one response here that is not JSON and not per-viewer. See `summary_text`.
#[derive(Responder)]
#[response(content_type = "text/plain; charset=utf-8")]
struct PlainText {
    body: String,
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
    //
    // `mut` because the document is **taken** rather than cloned: the two `take` call sites below
    // are mutually exclusive -- the fresh one returns -- and at 17.6 MiB a defensive clone is not
    // free. Were that ever reordered, the second take would answer `None` and the request would
    // degrade to "this room cannot be reached", which is a visible failure rather than a silent one.
    let mut cached = tracker::cached(conn, room.id).await?;

    // **Freshness is judged against the document being asked for, and nothing else.**
    //
    // This read the room's single `last_tracker_at`, which the STATIC document's write also moved.
    // A room whose live document has outgrown `PUNA_TRACKER_CACHE_MAX` keeps the last copy that fit
    // -- `store` refuses to truncate -- and the static writes kept stamping that copy as current, so
    // the tier served an hours-old live document with `stale: false` for a minute out of every five.
    // Measured on a 2000-slot room reporting 233 checks against the 169,938 it actually had.
    let entry = cached.as_mut().and_then(|c| c.take(which.kind()));

    if let Some(entry) = entry {
        let age = chrono::Utc::now()
            .signed_duration_since(entry.at)
            .to_std()
            .ok();
        if age.is_some_and(|age| age < which.ttl()) {
            memo.put((room.id, which), entry.body.clone());
            return Ok(Fetched {
                body: entry.body,
                stale_since: None,
            });
        }

        // Not fresh, and it is the only copy anybody has. Put it back so the fallback below can
        // serve it **with its real age attached** if the room does not answer -- which is what the
        // column is for, and is the honest version of what the old code was doing by accident.
        if let Some(cache) = cached.as_mut() {
            match which.kind() {
                tracker::Kind::Live => cache.live = Some(entry),
                tracker::Kind::Static => cache.statics = Some(entry),
            }
        }
    }

    // Nothing fresh: ask the room.
    match fetch(conn, upstream, room, which, cache_max).await {
        Ok(body) => {
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
            if let Some(cache) = cached.as_mut()
                && let Some(stale) = cache.take(which.kind())
            {
                tracing::debug!(
                    room = %room.id,
                    document = which.as_str(),
                    stored_at = %stale.at,
                    error = %e,
                    "serving a stale tracker document"
                );
                // **This document's own timestamp, which is the point of the pair.** The banner says
                // "as of <time>", so taking the neighbor's would have told a reader a three-day-old
                // document was minutes old.
                return Ok(Fetched {
                    body: stale.body,
                    stale_since: Some(stale.at),
                });
            }
            Err(unreachable_room(e))
        }
    }
}

// ---- the digested views ------------------------------------------------------------------------
//
// Puna's own shape, under its own prefix. The reference owns the whole `/api/tracker/*` subtree --
// `WebHostLib/api/__init__.py` even sets its CORS policy over the glob -- so putting a
// Puna-shaped document in there would be a trap for a tool walking that namespace. The two
// reference-compatible endpoints above are untouched.

/// How current the documents behind a view are.
///
/// The **oldest** of the documents used, because a view is only as current as its stalest half.
/// `as_of` for a fresh document is *now* rather than the exact moment the shared cache was filled:
/// that is accurate to within the document's own 60-second window, which is the resolution the
/// client polls at anyway. For a **stale** one it is exact, and that is the case where precision
/// matters — a room down for three days should say so.
fn freshness(stale_since: &[Option<DateTime<Utc>>], now: DateTime<Utc>) -> digest::Freshness {
    let oldest = stale_since.iter().flatten().min().copied();
    digest::Freshness {
        as_of: oldest.unwrap_or(now).to_rfc3339(),
        stale: oldest.is_some(),
        // The client is told the cadence rather than choosing one: asking faster than the
        // document's own cache window cannot produce new data, and only the server knows it.
        next_poll_ms: Document::Live.ttl().as_millis() as u64,
    }
}

/// Everything a digested view needs, resolved once.
struct Digestible {
    room: Room,
    roster: Vec<slot::Slot>,
    scope: Option<i32>,
}

async fn digestible(
    conn: &mut diesel_async::AsyncPgConnection,
    session: &Session,
    id: TrackerId,
    requested: Option<i32>,
) -> Result<Digestible> {
    let access = access(conn, session, id).await?;
    let scope = scope_of(&access, requested)?;
    let roster = slot::list(conn, access.room.id).await?;
    Ok(Digestible {
        scope,
        room: access.room,
        roster,
    })
}

/// Which slot, if any, a request is about.
///
/// **`?slot=<n>` is honored only for a room's tracker id, and it discloses nothing new.** Holding
/// that id already grants the whole multiworld's data, and the reference-compatible page
/// `/tracker/<id>/0/<n>` has always rendered any single slot from it. The parameter exists because
/// that page needs the per-slot views, and those resolve their scope from the *id* — which for this
/// URL names the room.
///
/// A slot's own id already names its slot, so combining it with a different one is two answers to
/// one question and answers `404`, the same rule the page applies.
fn scope_of(access: &Access, requested: Option<i32>) -> Result<Option<i32>> {
    match (access.target.slot_number(), requested) {
        (Some(own), Some(asked)) if own != asked => Err(not_found("no such slot")),
        (Some(own), _) => Ok(Some(own)),
        (None, asked) => Ok(asked),
    }
}

/// The slot table. Whole multiworld for a room's id, one row for a slot's.
#[get("/api/puna/tracker/<id>/slots?<slot>")]
async fn view_slots(
    id: TrackerParam,
    slot: Option<i32>,
    session: Session,
    conditional: IfNoneMatch,
    pool: &State<Pool>,
    state: TrackerState<'_>,
) -> Result<Json> {
    let mut conn = pool.get().await?;
    let it = digestible(&mut conn, &session, id.0, slot).await?;

    // Both documents: progress comes from one, and the location totals from the other.
    let live = obtain(&mut conn, &state, &it.room, Document::Live).await?;
    let statics = obtain(&mut conn, &state, &it.room, Document::Static).await?;

    let view = digest::slots(
        &it.roster,
        &parsed(&live.body),
        &parsed(&statics.body),
        freshness(&[live.stale_since, statics.stale_since], Utc::now()),
        it.scope,
        Utc::now(),
    );

    json(&view, &conditional)
}

/// The hint table. Every hint for a room's id; the ones a slot is either end of for a slot's.
#[get("/api/puna/tracker/<id>/hints?<slot>")]
async fn view_hints(
    id: TrackerParam,
    slot: Option<i32>,
    session: Session,
    conditional: IfNoneMatch,
    pool: &State<Pool>,
    state: TrackerState<'_>,
) -> Result<Json> {
    let mut conn = pool.get().await?;
    let it = digestible(&mut conn, &session, id.0, slot).await?;
    let live = obtain(&mut conn, &state, &it.room, Document::Live).await?;
    let names = names_for(&mut conn, state.names, it.room.generation_id).await?;

    let view = digest::hints(
        &it.roster,
        &parsed(&live.body),
        &digest::Names { games: &names },
        freshness(&[live.stale_since], Utc::now()),
        it.scope,
    );

    json(&view, &conditional)
}

/// One slot's locations, checked and unchecked.
///
/// **`404` for a room's tracker id, by construction rather than by a check**: this is a per-slot
/// question and a multiworld has no single answer to it. That is also what keeps the multiworld
/// view free of any other slot's raw data — there is no endpoint that would serve it.
#[get("/api/puna/tracker/<id>/locations?<slot>")]
async fn view_locations(
    id: TrackerParam,
    slot: Option<i32>,
    session: Session,
    conditional: IfNoneMatch,
    pool: &State<Pool>,
    state: TrackerState<'_>,
) -> Result<Json> {
    let mut conn = pool.get().await?;
    let it = digestible(&mut conn, &session, id.0, slot).await?;
    let slot = only_slot(&it)?;

    let live = obtain(&mut conn, &state, &it.room, Document::Live).await?;
    let names = names_for(&mut conn, state.names, it.room.generation_id).await?;

    // Absent means the name cache was never built for this generation. An empty table says
    // "nothing to show" honestly; inventing rows from the checked set would silently redefine the
    // view as "checked only", which is the one thing it exists not to be.
    let all = names::slot_locations(&mut conn, it.room.generation_id, slot.slot_number)
        .await?
        .unwrap_or_default();

    let view = digest::locations(
        slot,
        &all,
        &parsed(&live.body),
        &digest::Names { games: &names },
        freshness(&[live.stale_since], Utc::now()),
    );

    json(&view, &conditional)
}

/// One slot's received items. `404` for a room's id, for the same reason as `locations`.
#[get("/api/puna/tracker/<id>/items?<slot>")]
async fn view_items(
    id: TrackerParam,
    slot: Option<i32>,
    session: Session,
    conditional: IfNoneMatch,
    pool: &State<Pool>,
    state: TrackerState<'_>,
) -> Result<Json> {
    let mut conn = pool.get().await?;
    let it = digestible(&mut conn, &session, id.0, slot).await?;
    let slot = only_slot(&it)?;

    let live = obtain(&mut conn, &state, &it.room, Document::Live).await?;
    let names = names_for(&mut conn, state.names, it.room.generation_id).await?;

    let view = digest::items(
        slot,
        &it.roster,
        &parsed(&live.body),
        &digest::Names { games: &names },
        freshness(&[live.stale_since], Utc::now()),
    );

    json(&view, &conditional)
}

/// The scoped slot, or `404`.
///
/// A slot in the scope that is not in the roster answers `404` too, rather than an empty view: the
/// id resolved against `room_slots`, so its absence here would mean the two reads disagree, and
/// guessing which is right is worse than saying nothing.
fn only_slot(it: &Digestible) -> Result<&slot::Slot> {
    let scope = it
        .scope
        .ok_or_else(|| not_found("this view is per-slot; use a slot's tracker id"))?;

    it.roster
        .iter()
        .find(|s| s.slot_number == scope)
        .ok_or_else(|| not_found("no such slot"))
}

/// Unparseable means the cache holds something this build cannot read. `null` digests to a view
/// with empty tables rather than an error — the same fail-quiet the projection already takes.
fn parsed(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_default()
}

fn json<T: serde::Serialize>(view: &T, conditional: &IfNoneMatch) -> Result<Json> {
    let body = serde_json::to_string(view).map_err(|e| {
        Error::new(
            Status::InternalServerError,
            anyhow::anyhow!("could not render the tracker view: {e}"),
        )
    })?;
    Ok(respond(body, Document::Live, conditional))
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
    /// Where the client fetches its tables from: `/api/puna/tracker/<the id already in this URL>`.
    ///
    /// **Rendered rather than reconstructed in the browser** because the two page URLs carry the id
    /// in different shapes, and a client parsing `location.pathname` would have to know which. The
    /// id here is the one the visitor already holds, so this discloses nothing.
    api_base: String,
    /// The slot this page is about, if it is about one.
    slot: Option<i32>,
}

/// The multiworld's tracker, or one slot's.
#[get("/tracker/<id>")]
async fn page(id: TrackerParam, session: Session, pool: &State<Pool>) -> Result<TrackerTemplate> {
    let mut conn = pool.get().await?;
    let access = access(&mut conn, &session, id.0).await?;
    let scope = access.target.slot_number();
    render(&mut conn, &session, id.0, access, scope).await
}

/// **One line of plain text, for a chat bot.**
///
/// `RaysSMS: 50.0% | RaysLM: 25.4% | RaysSM64: goal`, and nothing else. Built for a Twitch bot
/// answering `!progress` while somebody streams a solo multiworld: one request, one line, no JSON
/// to walk and no HTML to scrape. `digest::summary` owns the wording; this owns who may ask.
///
/// **Served only where the tracker is open to the world, and `404` everywhere else** — including
/// to an organizer of a `members` room, which is the part worth stating because it looks like an
/// oversight. Two reasons, and the second is the one that decides it:
///
/// - The endpoint exists to be fetched by something holding no credential at all. A bot cannot log
///   in, so a summary that a room's staff could read and a bot could not would be a URL that works
///   in a browser and fails in the only place it is meant to be used.
/// - **It is what lets the answer be the same for everybody.** No viewer identity can change this
///   response, so it needs no `Session` guard, cannot leak one viewer's document to another, and is
///   `public` rather than `private` to any cache in front of it — unlike every other view here.
///
/// `404` rather than `403` for the same reason [`access`] gives: a restricted tracker and an id
/// that names nothing must be indistinguishable, or the refusal is itself an answer about which
/// unguessable ids are real.
///
/// A slot's own tracker id resolves to that slot alone, exactly as every other view scopes — so a
/// player can hand out a summary URL for their world without handing over the multiworld's.
#[get("/tracker/<id>/summary.txt")]
async fn summary_text(
    id: TrackerParam,
    pool: &State<Pool>,
    state: TrackerState<'_>,
) -> Result<PlainText> {
    let mut conn = pool.get().await?;

    let target = tracker::resolve(&mut conn, id.0)
        .await?
        .ok_or_else(|| not_found("no such tracker"))?;
    let room = room::get(&mut conn, target.room_id())
        .await?
        .ok_or_else(|| not_found("no such tracker"))?;

    // The whole access rule, and it is a property of the ROOM rather than of the reader.
    if room.tracker_policy != TrackerPolicy::Link {
        return Err(not_found("no such tracker"));
    }

    let roster = slot::list(&mut conn, room.id).await?;
    // Both documents, for the same reason the slots view needs both: progress comes from one and
    // the location totals from the other.
    let live = obtain(&mut conn, &state, &room, Document::Live).await?;
    let statics = obtain(&mut conn, &state, &room, Document::Static).await?;

    let rows = digest::slot_rows(
        &roster,
        &parsed(&live.body),
        &parsed(&statics.body),
        target.slot_number(),
        Utc::now(),
    );

    Ok(PlainText {
        body: digest::summary(&rows),
        // `public`, which no other response here can be: this one is identical for every reader by
        // construction, so a shared cache in front of it is free rate limiting rather than a way to
        // hand one viewer another's document. The window is pahoa's own for the live document, so a
        // bot polled hard costs a cache hit rather than a room.
        cache_control: Header::new(
            "Cache-Control",
            format!("public, max-age={}", Document::Live.ttl().as_secs()),
        ),
    })
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
) -> Result<TrackerTemplate> {
    let mut conn = pool.get().await?;
    let access = access(&mut conn, &session, id.0).await?;

    // Pahoa rooms are single-team, as the reference's own default is. Accepting only team 0 keeps
    // the URL honest rather than silently ignoring a segment somebody meant.
    if team != 0 {
        return Err(not_found("no such team"));
    }
    let scope = scope_of(&access, Some(player))?;

    render(&mut conn, &session, id.0, access, scope).await
}

/// The page is a **shell**, and that is the point of Stage C.
///
/// It fetches no tracker document at all: every table is rendered in the browser from
/// `/api/puna/tracker/**`. So the HTML costs one row read and nothing upstream, and the only work
/// that touches a room is the JSON the client asks for -- which is also the only thing that has to
/// be fresh.
async fn render(
    conn: &mut diesel_async::AsyncPgConnection,
    session: &Session,
    id: TrackerId,
    access: Access,
    scope: Option<i32>,
) -> Result<TrackerTemplate> {
    // Only to name the slot in the heading. The roster is Puna's, not the document's, which is why
    // a spectator -- absent from every per-player array -- still has a name here.
    let slot_name = match scope {
        Some(number) => slot::list(conn, access.room.id)
            .await?
            .into_iter()
            .find(|s| s.slot_number == number)
            .map(|s| s.player_name),
        None => None,
    };

    Ok(TrackerTemplate {
        base: TplContext::new(session),
        room_name: access.room.name.clone(),
        slot_name,
        api_base: format!("/api/puna/tracker/{id}"),
        slot: scope,
    })
}

/// Ask the room, and write what comes back into the shared cache.
///
/// Returns the document's **text**, unparsed. Nothing on this path needs its structure — see
/// `upstream::Upstream::fetch`, where that decision lives.
async fn fetch(
    conn: &mut diesel_async::AsyncPgConnection,
    upstream: &Upstream,
    room: &Room,
    which: Document,
    cache_max: usize,
) -> std::result::Result<String, UpstreamError> {
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

    let body = upstream
        .fetch(room.id, base_port, &secrets.admin_token, which)
        .await?;

    // One key, merged in SQL, so fetching one document cannot evict the other -- a room with a
    // cached live document and no static one would render a table with no games. This used to be a
    // read-modify-write, which meant parsing the whole column back into this process to write one
    // half of it.
    match tracker::store(conn, room.id, which.kind(), &body, cache_max).await {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            room = %room.id,
            document = which.as_str(),
            bytes = body.len(),
            "this room's tracker document is too large for the shared cache; it will be fetched \
             from the room every time and will not survive a teardown"
        ),
        // The cast to `jsonb` is where Postgres parses it, so this is a room that answered with
        // something that is not JSON. Served anyway -- the room is what said it -- but worth a line,
        // because from a viewer's side the only symptom is a tracker that never caches.
        Err(e) => tracing::warn!(
            room = %room.id,
            document = which.as_str(),
            error = %e,
            "could not cache this room's tracker document"
        ),
    }

    Ok(body)
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
        UpstreamError::Room(puna_core::room::RoomError::Status { status: 404 }) => Error::new(
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
    routes![
        page,
        slot_page,
        summary_text,
        live,
        statics,
        view_slots,
        view_hints,
        view_locations,
        view_items
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **`summary.txt` does not collide with the reference-compatible per-slot URL**, which is the
    /// open question about putting it under `/tracker/<id>/`: that space is otherwise the slot
    /// page's.
    ///
    /// It does not, because the shapes differ in length — `/tracker/<id>/summary.txt` is three
    /// segments and `/tracker/<id>/<team>/<player>` is four — but "differ in length" is a claim
    /// about Rocket's matcher rather than about this file, and the next path added here may not be
    /// so lucky. Rocket refuses to ignite on a collision, so building the client **is** the
    /// assertion; the dispatches after it are what says each URL reaches the handler it names.
    ///
    /// Neither request can get further than its state guards here, so a `500` means "routed, and
    /// then found no database" while a `404` means the route table never matched it at all.
    #[rocket::async_test]
    async fn the_text_summary_does_not_shadow_the_slot_page() {
        use rocket::http::Status;
        use rocket::local::asynchronous::Client;

        // A pool that is never connected to. `get_database_pool` with no migrations builds the
        // deadpool and nothing else -- connections are made on `get()` -- so this is here to
        // satisfy Rocket's `&State<Pool>` sentinel, which aborts ignite for unmanaged state and
        // would otherwise be indistinguishable from the collision this test is looking for.
        let pool = puna_core::db::get_database_pool("postgres://127.0.0.1:1/none", None)
            .await
            .expect("a pool, unconnected");

        let client = Client::untracked(rocket::build().manage(pool).mount("/", routes()))
            .await
            .expect("the tracker routes ignite, which is to say none of them collide");

        let id = puna_core::ids::TrackerId::new();
        for path in [
            format!("/tracker/{id}/summary.txt"),
            format!("/tracker/{id}/0/1"),
            format!("/tracker/{id}"),
        ] {
            assert_ne!(
                client.get(&path).dispatch().await.status(),
                Status::NotFound,
                "{path} matched no route at all"
            );
        }

        // And the static segment is static: it is not a slot name, a team, or anything else the
        // shapes beside it would accept.
        assert_eq!(
            client
                .get(format!("/tracker/{id}/summary.json"))
                .dispatch()
                .await
                .status(),
            Status::NotFound,
            "something other than summary.txt is being routed to the summary"
        );
    }

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

    /// **A document the memo will no longer serve is dropped, not just ignored.**
    ///
    /// Expiring on read alone leaves the bytes resident until something overwrites that exact key,
    /// which for a room nobody opens again is until the process restarts. That was a fair trade
    /// when the entries were small; at 17.6 MiB each it is the tier's memory limit.
    #[test]
    fn the_memo_drops_what_it_will_not_serve() {
        let memo = Memo::default();
        let stale = (RoomId::new(), Document::Live);
        memo.put(stale, "{}".to_string());
        {
            let mut entries = memo.entries.lock().expect("lock");
            entries.get_mut(&stale).expect("the entry").0 =
                Instant::now() - MEMO_TTL - Duration::from_secs(1);
        }

        memo.put((RoomId::new(), Document::Live), "{}".to_string());

        let entries = memo.entries.lock().expect("lock");
        assert!(
            !entries.contains_key(&stale),
            "an expired entry outlived the write that swept it"
        );
        assert_eq!(entries.len(), 1, "the live entry must survive the sweep");
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

    /// **The property this whole tier exists for**, asserted on the shell.
    ///
    /// Stage C moved the tables into the browser, so the rows this used to check are now covered by
    /// `digest`'s own leak test over the four JSON views. What is left here is the half that JSON
    /// cannot cover: the surrounding page must still name no room, no address and no start control.
    #[test]
    fn the_rendered_page_leaks_neither_the_address_nor_the_room() {
        let room_id = RoomId::new();
        // A fixed, all-letter tracker id. A random uuid can contain a five-digit run that the
        // port-shaped check below would read as an address, which would make this test flaky for a
        // reason that has nothing to do with what it is testing.
        let tracker_id: TrackerId = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
            .parse()
            .expect("a valid uuid");

        let page = TrackerTemplate {
            base: TplContext {
                is_logged_in: false,
                is_admin: false,
                username: String::new(),
                site_name: "puna",
                version: "test",
                static_version: "test",
                view_as: None,
            },
            room_name: "Friday async".into(),
            slot_name: Some("Troy".into()),
            api_base: format!("/api/puna/tracker/{tracker_id}"),
            slot: Some(1),
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
        // No start button, however the room is doing: a tracker's audience is not necessarily
        // authorized to provision a pod, and a widely-shared link that spins up compute is exactly
        // the hazard D8 exists to prevent.
        assert!(
            !html.contains("/start"),
            "a start control reached the tracker"
        );

        // What it *does* say: the room's name, the slot it is about, and where the client fetches.
        assert!(html.contains("Friday async"));
        // The rendered form, not just the substring: `whitespace = "suppress"` once made this
        // "ShowingTroy" on every page that named somebody. See `tests/templates.rs`.
        assert!(
            html.contains("Showing Troy"),
            "the space before the name is gone again"
        );
        assert!(html.contains(&format!("/api/puna/tracker/{tracker_id}")));
        assert!(html.contains("<noscript>"), "the no-JavaScript explanation");

        // The per-slot tables exist only on a slot's page; the multiworld one would be sending
        // every slot's location list to a browser that shows one table.
        assert!(html.contains("data-view=\"locations\""));
        assert!(html.contains("data-view=\"items\""));
    }

    /// The multiworld page carries the slot table and the hints, and **not** the per-slot tables.
    #[test]
    fn the_multiworld_page_has_no_per_slot_tables() {
        let page = TrackerTemplate {
            base: TplContext {
                is_logged_in: false,
                is_admin: false,
                username: String::new(),
                site_name: "puna",
                version: "test",
                static_version: "test",
                view_as: None,
            },
            room_name: "Friday async".into(),
            slot_name: None,
            api_base: "/api/puna/tracker/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            slot: None,
        };

        let html = page.render().expect("renders");
        assert!(html.contains("data-view=\"slots\""));
        assert!(html.contains("data-view=\"hints\""));
        assert!(!html.contains("data-view=\"locations\""));
        assert!(!html.contains("data-view=\"items\""));
        assert!(
            !html.contains("data-slot="),
            "no slot scope on the multiworld page"
        );
    }

    /// A crude port-shaped-number check, so the leak test does not need a regex dependency.
    fn regex_free_port_like(html: &str) -> bool {
        html.split(|c: char| !c.is_ascii_digit())
            .filter_map(|run| run.parse::<u32>().ok())
            .any(|n| (40000..=49999).contains(&n))
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
    /// Build a tracker page for one slot, or for the whole multiworld.
    fn tracker_page(slot: Option<i32>) -> TrackerTemplate {
        TrackerTemplate {
            base: TplContext {
                is_logged_in: false,
                is_admin: false,
                username: String::new(),
                site_name: "puna",
                version: "test",
                static_version: "test",
                view_as: None,
            },
            room_name: "Friday async".into(),
            slot_name: slot.map(|_| "Troy".into()),
            api_base: "/api/puna/tracker/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            slot,
        }
    }

    /// **A filter over a table that always has one row can only hide it.** A slot's own page renders
    /// exactly one slot, so the box is offered on the multiworld view and nowhere else.
    ///
    /// Asserted by counting, because both pages carry several search boxes and "contains a search
    /// box" is true either way -- the question is how many.
    #[test]
    fn the_one_row_slot_table_offers_no_filter() {
        let multiworld = tracker_page(None).render().expect("renders");
        let one_slot = tracker_page(Some(1)).render().expect("renders");

        // Multiworld: slots + hints. One slot: locations + items + hints, and NOT slots.
        assert_eq!(
            multiworld.matches("table-search").count(),
            2,
            "the multiworld view lost a filter, or grew one"
        );
        assert_eq!(
            one_slot.matches("table-search").count(),
            3,
            "the slot view's filter count is wrong -- the one-row slot table should have none"
        );

        assert!(
            !one_slot.contains(r#"aria-label="Filter slots""#),
            "a table with one row is offering a filter that can only hide it"
        );
        assert!(
            multiworld.contains(r#"aria-label="Filter slots""#),
            "the multiworld slot table lost its filter"
        );
    }

    /// **The two unbounded tables scroll inside themselves, in ONE wrapper.**
    ///
    /// One because `position: sticky` resolves against the nearest scrollport: nest a horizontal
    /// scroller inside a vertical one and the header sticks to a box that never scrolls vertically,
    /// so it slides away with the rows. That failure is invisible in markup and only shows when
    /// somebody scrolls a long list -- which is exactly the case these two exist for.
    #[test]
    fn a_slots_locations_and_items_scroll_inside_one_bounded_wrapper() {
        let html = tracker_page(Some(1)).render().expect("renders");

        assert_eq!(
            html.matches(r#"class="table-scroll bounded""#).count(),
            2,
            "locations and items are the two tables nobody chose the length of"
        );

        // Each bounded wrapper holds a table DIRECTLY -- no second scroller in between.
        for section in html.split(r#"<div class="table-scroll bounded">"#).skip(1) {
            let head = section.split("<table").next().unwrap_or_default();
            assert!(
                !head.contains("<div"),
                "a wrapper sits between the bounded scroller and its table, so the sticky header \
                 will resolve against the inner one and scroll away:\n{head}"
            );
        }
    }

    /// **The items column is named for what it counts**, and the collapse toggle remembers itself.
    ///
    /// Three files have to agree for that toggle to work at all: the template names a key, and
    /// `toggles.js` restores it while `tracker.js` reacts to it. Every mismatch is silent — the box
    /// renders, ticks, and does nothing, or does something and forgets by the next page load.
    #[test]
    fn the_items_table_is_ordered_and_collapsible() {
        let html = tracker_page(Some(1)).render().expect("renders");

        assert!(
            html.contains(r#"<th data-key="order" data-type="number">Order</th>"#),
            "the items column is headed `#`, which does not say what it counts"
        );

        assert!(
            html.contains(r#"data-toggle="tracker.slot.items.latest""#),
            "the collapse toggle is missing from the items table"
        );

        // The store that restores it, and the file that reacts to it. Checked against the CALLS
        // rather than any mention -- a lint that matches its own prose has happened four times in
        // this codebase.
        let toggles = std::fs::read_to_string("static/toggles.js").expect("toggles.js");
        assert!(
            toggles.contains(r#"querySelectorAll("[data-toggle]")"#),
            "toggles.js no longer restores the attribute the template emits"
        );
        assert!(
            html.contains("/static/toggles.js"),
            "the page emits a toggle and never loads the file that remembers it"
        );

        let tracker = std::fs::read_to_string("static/tracker.js").expect("tracker.js");
        assert!(
            tracker.contains(r#"querySelector("[data-toggle]")"#),
            "tracker.js no longer reads the toggle, so ticking it changes nothing"
        );
        assert!(
            tracker.contains("collapse: { key: \"item\", recency: \"order\" }"),
            "the items view no longer declares how it collapses"
        );
    }

    /// **Every remembered preference is keyed by PAGE TYPE**, and the hints table is why.
    ///
    /// It is on both trackers, and its toggle asks a different question on each: on a slot's page
    /// "what am I still waiting for", on the multiworld's "what is outstanding anywhere". One shared
    /// key would make a choice on one page silently change the other — which looks like the setting
    /// not persisting, rather than like two views sharing state.
    #[test]
    fn a_remembered_preference_is_scoped_to_the_page_it_was_made_on() {
        let one_slot = tracker_page(Some(1)).render().expect("renders");
        let multiworld = tracker_page(None).render().expect("renders");

        assert!(
            one_slot.contains(r#"data-toggle="tracker.slot.hints.hidefound""#),
            "the slot page's hints toggle is not scoped to it"
        );
        assert!(
            multiworld.contains(r#"data-toggle="tracker.room.hints.hidefound""#),
            "the multiworld page's hints toggle is not scoped to it"
        );
        assert!(
            !multiworld.contains("tracker.slot."),
            "the multiworld page carries a slot-scoped key, so the two share state"
        );

        // The per-slot tables, which exist on one page only.
        assert!(one_slot.contains(r#"data-toggle="tracker.slot.locations.hidechecked""#));
        assert!(one_slot.contains(r#"data-toggle="tracker.slot.items.latest""#));

        // And the script derives the same namespace for the sorts it remembers, from the same
        // signal the template branches on -- `data-slot` being present.
        let tracker = std::fs::read_to_string("static/tracker.js").expect("tracker.js");
        assert!(
            tracker.contains(r#"root.dataset.slot ? "slot" : "room""#),
            "tracker.js no longer derives the page type, so remembered sorts would collide"
        );
        assert!(
            tracker.contains("`tracker.${pageType}.${this.view}.sort`"),
            "the remembered sort key is not scoped by page and table"
        );
    }

    /// Tables, not prose. The same opt-out the admin pages and the room page take.
    #[test]
    fn tracker_pages_are_not_held_to_the_prose_measure() {
        for slot in [None, Some(1)] {
            assert!(
                tracker_page(slot)
                    .render()
                    .expect("renders")
                    .contains("<body class=\"wide\">"),
                "a tracker page is held to a measure meant for paragraphs"
            );
        }
    }
}
