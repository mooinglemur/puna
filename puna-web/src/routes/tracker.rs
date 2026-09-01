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
//! reference field for field, so mirroring the paths too is what makes those tools work against a
//! Puna room with only a base URL changed. Getting this wrong would throw away the compatibility
//! that made mirroring the documents worthwhile.
//!
//! ## Three cache layers, in the order they remove work
//!
//! 1. **`ETag` and `Cache-Control`**, so a browser stops re-fetching, or at least stops
//!    re-downloading. **Which of those depends on what the response is made of**: see [`Caching`].
//!    A passthrough of pahoa's document may be reused for pahoa's own window without asking; a view
//!    Puna derives from its own rows as well must revalidate, because those rows change when
//!    somebody presses Save rather than on pahoa's schedule. The `ETag` is what makes revalidating
//!    nearly free, and it is the layer that cuts bytes rather than requests.
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
use puna_core::model::{annotation, event, member, names, slot, tracker};
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
/// Deliberately short. It exists to absorb a burst (a page load fetching both documents, twenty
/// tabs refreshing at once), not to add staleness on top of the shared cache's.
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
            // policy", which quietly assumed a small document. A 2000-slot room's is 17.6 MiB, so
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
/// They are **static per generation** (the seed cannot change), so this could be forever. It is
/// not, because an admin rebuild repairs a bad cache and a process that never re-read would ignore
/// the repair until it restarted. Ten minutes makes the fix land on its own.
const NAMES_TTL: Duration = Duration::from_secs(600);

/// Every game's names for one generation, shared between the requests that need them.
type Games = Arc<BTreeMap<String, GameNames>>;

/// Item and location names, per generation, held in this process.
///
/// **Worth having rather than querying per request**, and by a wide margin: measured on a real
/// seed, one generation's tables are ~2.7 MB across 54 games. Reading that from Postgres on every
/// poll of every tab would put the tracker tier's whole reason for existing (absorbing the most
/// public, highest-volume surface Puna has) straight onto the database instead.
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
    /// Staff of this room: an organizer, a helper, or a site admin.
    ///
    /// **Kept apart from `owns_a_slot` rather than folded into one flag**, because the enhanced
    /// tracker needs the difference: a ping preference of `no` withholds somebody's handle from
    /// other players and never from staff, who are exactly who "organizers and helpers may still
    /// choose to ping you" is about.
    is_staff: bool,
    owns_a_slot: bool,
    /// Whoever is looking, when they are signed in. Compared against a slot's owner to decide who
    /// may edit an annotation, and used as the subject of a ping preference.
    viewer: Option<i64>,
}

impl Access {
    /// One of the room's own people. The tier the roster applies to who holds a slot, and the one
    /// `may_see_spoiler` calls `players`.
    fn is_participant(&self) -> bool {
        self.is_staff || self.owns_a_slot
    }

    /// Whether this viewer gets the annotation columns at all: the room has to have opted in **and**
    /// the viewer has to be one of its people. Everybody else sees the tracker exactly as it was
    /// before the feature existed.
    fn sees_annotations(&self) -> bool {
        self.room.enhanced_tracker && self.is_participant()
    }
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
            // The tracker tier initiates no login of its own (it holds no Discord credentials),
            // but the 401 catcher redirects to the web tier's `/auth/login` on the same hostname,
            // which is why both roles share one `ROCKET_SECRET_KEY` and only one has the secrets.
            unauthorized("log in to see this tracker")
        } else {
            forbidden("this tracker is limited to the room's members")
        });
    }

    Ok(Access {
        room,
        target,
        is_staff,
        owns_a_slot,
        viewer: session.user_id,
    })
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
    // and a slot view is a projection of it, rather than one cached document per slot, which
    // would multiply both the upstream fetches and the memory by the room's slot count.
    let scope = access.target.slot_number();
    let fetched = obtain(&mut conn, &state, &access.room, which).await?;

    Ok(respond(
        project(fetched.body, scope),
        Caching::Upstream(which),
        &conditional,
    ))
}

/// One document, from whichever layer has it.
struct Fetched {
    body: String,
    /// When this was true, if it is no longer. `Some` means **the room did not answer** and this is
    /// the last thing it said, which for an async is most of its life, and is exactly what the
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
    // are mutually exclusive (the fresh one returns) and at 17.6 MiB a defensive clone is not
    // free. Were that ever reordered, the second take would answer `None` and the request would
    // degrade to "this room cannot be reached", which is a visible failure rather than a silent one.
    let mut cached = tracker::cached(conn, room.id).await?;

    // **Freshness is judged against the document being asked for, and nothing else.**
    //
    // This read the room's single `last_tracker_at`, which the STATIC document's write also moved.
    // A room whose live document has outgrown `PUNA_TRACKER_CACHE_MAX` keeps the last copy that fit
    // (`store` refuses to truncate), and the static writes kept stamping that copy as current, so
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
        // serve it **with its real age attached** if the room does not answer, which is what the
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
            // button**: a tracker's audience is not necessarily authorized to provision a pod,
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
// Puna's own shape, under its own prefix. The reference owns the whole `/api/tracker/*` subtree
// (`WebHostLib/api/__init__.py` even sets its CORS policy over the glob), so putting a
// Puna-shaped document in there would be a trap for a tool walking that namespace. The two
// reference-compatible endpoints above are untouched.

/// How current the documents behind a view are.
///
/// The **oldest** of the documents used, because a view is only as current as its stalest half.
/// `as_of` for a fresh document is *now* rather than the exact moment the shared cache was filled:
/// that is accurate to within the document's own 60-second window, which is the resolution the
/// client polls at anyway. For a **stale** one it is exact, and that is the case where precision
/// matters: a room down for three days should say so.
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
    /// Carried from [`Access`], because the roster below holds `owner_id`, a note and a progression
    /// for every slot, and the digest is what decides which of those reach the wire.
    is_staff: bool,
    is_participant: bool,
    viewer: Option<i64>,
    /// The room's handles and ping preferences, loaded **only** where they will be rendered: the
    /// room has the enhanced tracker on and this viewer is one of its people. `None` is what makes
    /// an ordinary tracker cost exactly the queries it always did.
    people: Option<digest::People>,
}

impl Digestible {
    fn viewer(&self) -> digest::Viewer<'_> {
        digest::Viewer {
            id: self.viewer,
            participant: self.is_participant,
            staff: self.is_staff,
            people: self.people.as_ref(),
        }
    }
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
    let people = if access.sees_annotations() {
        Some(digest::People {
            handles: slot::owner_names(conn, access.room.id).await?,
            preferences: annotation::preferences(conn, access.room.id)
                .await?
                .into_iter()
                .map(|p| (p.user_id, p.preference))
                .collect(),
        })
    } else {
        None
    };

    Ok(Digestible {
        scope,
        is_staff: access.is_staff,
        is_participant: access.is_participant(),
        viewer: access.viewer,
        people,
        room: access.room,
        roster,
    })
}

/// Which slot, if any, a request is about.
///
/// **`?slot=<n>` is honored only for a room's tracker id, and it discloses nothing new.** Holding
/// that id already grants the whole multiworld's data, and the reference-compatible page
/// `/tracker/<id>/0/<n>` has always rendered any single slot from it. The parameter exists because
/// that page needs the per-slot views, and those resolve their scope from the *id*, which for this
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
        &it.viewer(),
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
/// view free of any other slot's raw data: there is no endpoint that would serve it.
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
/// with empty tables rather than an error, the same fail-quiet the projection already takes.
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
    Ok(respond(body, Caching::Derived, conditional))
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
    /// Whether this page renders the enhanced tracker's columns: the room has opted in **and** this
    /// viewer is one of its people.
    ///
    /// The table skeleton is server-rendered and the bodies are the client's, so the column has to
    /// exist here for `tracker.js` to fill, and its absence is what makes an outsider's page
    /// byte-identical to the one it was before the feature existed.
    /// Whatever the two write routes had to say. **The tracker page reads one now**, which it did
    /// not before it could be written to: a save that redirects with no word for it is a page that
    /// reloads and leaves somebody squinting at the table to work out whether it took.
    notice: Option<crate::flash::Notice>,
    annotations: bool,
    /// Where the two write forms post: `/tracker/<the id already in this URL>`.
    ///
    /// **Rendered rather than reconstructed in the browser**, for the reason `api_base` gives: the
    /// two page URLs carry the id in different shapes (a slot's own, or the room's followed by
    /// `/0/<n>`), and a client parsing `location.pathname` would have to know which. The id here is
    /// the one the visitor already holds, so this discloses nothing, and it is emphatically **not**
    /// the room's id, which this page must never carry.
    write_base: String,
    /// Whether to offer the preferences form: this viewer holds a slot here.
    ///
    /// **Not `is_staff`**, deliberately. A ping preference renders as a chip beside a slot's owner,
    /// so somebody holding none has nothing to set, and offering the control anyway would be a
    /// form whose effect is invisible. Staff who also play get it, because they are players.
    owns_a_slot: bool,
    /// What they have said so far, so the form opens on their own answer rather than on the default.
    my_preference: &'static str,
    /// Every choice, as `(value, label, explanation)`. Rendered from the enum rather than written
    /// out in markup, so a value pahoa's vocabulary never sees cannot drift between the two.
    ping_choices: Vec<(&'static str, &'static str, &'static str)>,
    /// The progression values the dialog offers, as `(value, label)`.
    progression_choices: Vec<(&'static str, &'static str)>,
    note_limit: usize,
    /// The slot this page is about, if it is about one.
    slot: Option<i32>,
}

/// The multiworld's tracker, or one slot's.
#[get("/tracker/<id>")]
async fn page(
    id: TrackerParam,
    session: Session,
    flash: Option<rocket::request::FlashMessage<'_>>,
    pool: &State<Pool>,
) -> Result<TrackerTemplate> {
    let mut conn = pool.get().await?;
    let access = access(&mut conn, &session, id.0).await?;
    let scope = access.target.slot_number();
    render(&mut conn, &session, id.0, access, scope, flash).await
}

/// **One line of plain text, for a chat bot.**
///
/// `RaysSMS: 50.0% | RaysLM: 25.4% | RaysSM64: goal`, and nothing else. Built for a Twitch bot
/// answering `!progress` while somebody streams a solo multiworld: one request, one line, no JSON
/// to walk and no HTML to scrape. `digest::summary` owns the wording; this owns who may ask.
///
/// **Served only where the tracker is open to the world, and `404` everywhere else**, including
/// to an organizer of a `members` room, which is the part worth stating because it looks like an
/// oversight. Two reasons, and the second is the one that decides it:
///
/// - The endpoint exists to be fetched by something holding no credential at all. A bot cannot log
///   in, so a summary that a room's staff could read and a bot could not would be a URL that works
///   in a browser and fails in the only place it is meant to be used.
/// - **It is what lets the answer be the same for everybody.** No viewer identity can change this
///   response, so it needs no `Session` guard, cannot leak one viewer's document to another, and is
///   `public` rather than `private` to any cache in front of it, unlike every other view here.
///
/// `404` rather than `403` for the same reason [`access`] gives: a restricted tracker and an id
/// that names nothing must be indistinguishable, or the refusal is itself an answer about which
/// unguessable ids are real.
///
/// A slot's own tracker id resolves to that slot alone, exactly as every other view scopes, so a
/// player can hand out a summary URL for their world without handing over the multiworld's.
///
/// ## The two modifiers
///
/// * **`?s=1,2,4`**: only these slots, in the roster's order rather than the order asked for. That
///   makes one set of slots one URL, which matters for a response a shared cache may hold and for a
///   bot diffing its own output.
/// * **`?o`**: append an overall line. See [`flag`] for why the value is optional *and* forgiving.
///
/// **Both live in the URL rather than in a session**, which is what keeps this response identical
/// for every reader and therefore `public`-cacheable: a cache keys on the whole URL, so two
/// selections are two entries rather than one being served for the other.
#[get("/tracker/<id>/summary.txt?<s>&<o>")]
async fn summary_text(
    id: TrackerParam,
    s: Option<String>,
    o: Option<String>,
    pool: &State<Pool>,
    state: TrackerState<'_>,
) -> Result<PlainText> {
    let overall = flag("o", o.as_deref())?;
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
    let wanted = selection(s.as_deref(), &roster, target.slot_number())?;

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
        // **The outsider's view, and it has to be**: this response is deliberately identical for
        // every reader: that is what lets it be `public`-cacheable and what makes it answerable to
        // a bot with no session at all. A viewer-dependent field here would be a shared cache
        // handing one reader another's document, which is the exact trade the comment below buys.
        //
        // `summary` renders no claim state and no annotation today, so this changes nothing now and
        // is what stops it becoming a leak the day somebody adds a column to that function.
        &digest::Viewer::outsider(),
    );

    // Filtered after the digest rather than before it, so a selected slot's row is built from the
    // same code every other view builds it from, and so `?s` cannot become a second place that
    // decides what a row contains.
    let rows: Vec<digest::SlotRow> = match &wanted {
        Some(only) => rows
            .into_iter()
            .filter(|row| only.contains(&row.slot))
            .collect(),
        None => rows,
    };

    Ok(PlainText {
        body: digest::summary(&rows, overall),
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
/// The *other* per-slot form (a slot's own id) is the one that discloses nothing about the room,
/// and both render the same page.
#[get("/tracker/<id>/<team>/<player>")]
async fn slot_page(
    id: TrackerParam,
    team: i32,
    player: i32,
    session: Session,
    flash: Option<rocket::request::FlashMessage<'_>>,
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

    render(&mut conn, &session, id.0, access, scope, flash).await
}

/// The page is a **shell**, and that is the point of Stage C.
///
/// It fetches no tracker document at all: every table is rendered in the browser from
/// `/api/puna/tracker/**`. So the HTML costs one row read and nothing upstream, and the only work
/// that touches a room is the JSON the client asks for, which is also the only thing that has to
/// be fresh.
async fn render(
    conn: &mut diesel_async::AsyncPgConnection,
    session: &Session,
    id: TrackerId,
    access: Access,
    scope: Option<i32>,
    flash: Option<rocket::request::FlashMessage<'_>>,
) -> Result<TrackerTemplate> {
    // Only to name the slot in the heading. The roster is Puna's, not the document's, which is why
    // a spectator (absent from every per-player array) still has a name here.
    let slot_name = match scope {
        Some(number) => slot::list(conn, access.room.id)
            .await?
            .into_iter()
            .find(|s| s.slot_number == number)
            .map(|s| s.player_name),
        None => None,
    };

    // Their own answer, so the form opens on what they said rather than on the default. Absent is
    // `unknown`, which is the default answer for somebody who has simply not been asked.
    let mine = match (access.sees_annotations(), access.viewer) {
        (true, Some(user)) => annotation::preferences(conn, access.room.id)
            .await?
            .into_iter()
            .find(|p| p.user_id == user)
            .map_or_else(annotation::PingPreference::default, |p| p.preference),
        _ => annotation::PingPreference::default(),
    };

    Ok(TrackerTemplate {
        base: TplContext::new(session),
        notice: crate::flash::Notice::take(flash),
        room_name: access.room.name.clone(),
        slot_name,
        api_base: format!("/api/puna/tracker/{id}"),
        annotations: access.sees_annotations(),
        write_base: format!("/tracker/{id}"),
        owns_a_slot: access.owns_a_slot,
        my_preference: mine.as_sql(),
        ping_choices: annotation::PingPreference::ALL
            .into_iter()
            .map(|p| (p.as_sql(), p.label(), p.explanation()))
            .collect(),
        progression_choices: annotation::ProgressionStatus::ALL
            .into_iter()
            .map(|p| (p.as_sql(), p.label()))
            .collect(),
        note_limit: annotation::MAX_NOTE_CHARS,
        slot: scope,
    })
}

/// Ask the room, and write what comes back into the shared cache.
///
/// Returns the document's **text**, unparsed. Nothing on this path needs its structure. See
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

    // One key, merged in SQL, so fetching one document cannot evict the other: a room with a
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
        // something that is not JSON. Served anyway (the room is what said it), but worth a line,
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
/// is a key that passes through unscoped: the failure is silent and in the wrong direction.
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
/// Takes and returns the rendered body rather than a `Value`, so the whole-multiworld case (which
/// is the common one) costs nothing at all: no parse, no re-render, just the string the cache
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
/// player can share their own tracker without handing out the multiworld's, and that promise is
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
        // Anything else (`groups`, or a key a later pahoa adds) is left out. A slot's view
        // showing the item-link groups would name other slots, which is the one thing it is for
        // not doing.
    }

    serde_json::Value::Object(out)
}

/// A room that cannot be reached, mapped to something a caller can act on.
fn unreachable_room(e: UpstreamError) -> Error {
    match e {
        // A `404` from a room means no admin token is configured there (pahoa answers `404`
        // rather than `401` precisely so this is distinguishable), which means the Secret did not
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

/// A query-string flag whose **presence is the point**, spelled forgivingly.
///
/// `?o` on its own is on, which is what somebody writing a bot's URL by hand reaches for. So is
/// `?o=1`, and that is the whole reason this exists rather than a plain `bool` parameter: Rocket's
/// `FromFormField for bool` accepts an empty value, `on`, `yes` and `true`, and **refuses `1`**,
/// with a 422 for the whole request. `1` is the most natural thing to type after `=`, and a
/// hand-written URL failing with no explanation is a bad trade for four characters of parsing.
///
/// It refuses an unrecognized value rather than treating presence alone as on. `?o=false` meaning
/// *on* would be the same trap pointing the other way, and this endpoint is read by bots whose
/// config somebody wrote once and will not revisit.
fn flag(name: &str, raw: Option<&str>) -> Result<bool> {
    match raw.map(str::trim) {
        None => Ok(false),
        // Bare, which is the spelling this exists for.
        Some("") => Ok(true),
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "1" | "on" | "yes" | "true" => Ok(true),
            "0" | "off" | "no" | "false" => Ok(false),
            other => Err(Error::new(
                Status::BadRequest,
                anyhow::anyhow!(
                    "`{name}={other}` is not a yes or a no; use `{name}` on its own, or one of \
                     1/0, on/off, yes/no, true/false"
                ),
            )),
        },
    }
}

/// Which slots `?s=1,2,4` asked for, or `None` for all of them.
///
/// **Refuses rather than ignores**, in both directions. A token that is not a number and a number
/// that is not a slot of this room are both configuration mistakes in a URL somebody pasted into a
/// bot once, and the alternative is a summary quietly listing fewer slots than were asked for,
/// which reads as the room having changed rather than as the URL being wrong.
///
/// Blank (`?s=` or `?s=,,`) is treated as absent rather than as an empty selection: an empty one
/// could only ever produce `no slots`, so reading it as "everything" is the interpretation that
/// might be what somebody meant.
///
/// **Order comes from the roster, never from the query.** `?s=4,1` and `?s=1,4` are the same
/// request and must produce the same bytes: this response is `public`-cacheable and is diffed by
/// bots, and two spellings of one selection producing two answers would be a needless difference.
/// The filtering itself preserves roster order because it walks the digested rows.
fn selection(
    raw: Option<&str>,
    roster: &[slot::Slot],
    scope: Option<i32>,
) -> Result<Option<Vec<i32>>> {
    let Some(raw) = raw else { return Ok(None) };

    let mut wanted = Vec::new();
    for token in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let number: i32 = token.parse().map_err(|_| {
            Error::new(
                Status::BadRequest,
                anyhow::anyhow!("`s={token}` is not a slot number"),
            )
        })?;
        if !roster.iter().any(|slot| slot.slot_number == number) {
            return Err(Error::new(
                Status::BadRequest,
                anyhow::anyhow!("this room has no slot {number}"),
            ));
        }
        // Deduplicated, so `?s=1,1` is one entry rather than a slot listed twice.
        if !wanted.contains(&number) {
            wanted.push(number);
        }
    }

    if wanted.is_empty() {
        return Ok(None);
    }

    // **A slot's own tracker id already names its slot**, so combining it with a different one is
    // two answers to one question, the rule `scope_of` applies to the digested views, applied
    // here for the same reason. `404` rather than a refusal, matching it.
    if let Some(own) = scope
        && wanted != [own]
    {
        return Err(not_found("no such slot"));
    }

    Ok(Some(wanted))
}

/// How long a response may be reused **without asking**, which is a different question from how
/// long it stays accurate.
///
/// ## The distinction this exists to draw, and getting it wrong was a real bug
///
/// Every response here once carried `max-age` from pahoa's own document window, which was exactly
/// right while every response *was* pahoa's document. It is not right for the digested views: those
/// are a function of the room's documents **and of Puna's own rows** (a slot's owner, its
/// progression, its note, its holder's ping preference), and those change the instant somebody
/// presses Save rather than on pahoa's schedule.
///
/// Under `max-age` the browser serves its own copy without a request, so saving an annotation and
/// landing back on the tracker showed the *previous* body for the rest of the window. The `ETag`
/// was already correct and never got a chance to run: a browser with a fresh entry does not ask.
///
/// **`claimed` had the same defect and nobody noticed**, because a claim lands rarely enough that
/// the stale window closed before anybody looked twice. The annotations only made a latent
/// wrongness visible.
enum Caching {
    /// A passthrough of pahoa's own document, where pahoa's window is precisely the right answer:
    /// asking again sooner cannot produce different data.
    Upstream(Document),
    /// A view Puna derives, which may change between one request and the next for reasons the
    /// upstream window knows nothing about.
    ///
    /// `no-cache` rather than `no-store`: the browser may keep it and **must revalidate**, so the
    /// `ETag` still does the work it was added for. The common answer is a 304 with no body, which
    /// is what keeps this cheap on the tier that exists to absorb polling.
    Derived,
}

/// `ETag` over the body, plus how long it may be reused without asking.
fn respond(body: String, caching: Caching, conditional: &IfNoneMatch) -> Json {
    let etag = format!("\"{}\"", puna_core::hash::sha256_hex(body.as_bytes()));

    if conditional.0.as_deref() == Some(etag.as_str()) {
        return Json::Unchanged(NotModified(()));
    }

    // `private` throughout, because a `members`-policy tracker is per-viewer and the digested views
    // are per-viewer on EVERY policy: a shared cache in front of this must never hand one reader
    // another's document. `summary.txt` is the one `public` response here, and the one that is
    // identical for every reader by construction.
    let cache_control = match caching {
        Caching::Upstream(which) => format!("private, max-age={}", which.ttl().as_secs()),
        Caching::Derived => "private, no-cache".to_string(),
    };

    Json::Body(Box::new(Cached {
        body,
        etag: Header::new("ETag", etag),
        cache_control: Header::new("Cache-Control", cache_control),
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

// --- the two writes ------------------------------------------------------------------------------
//
// **The first mutations this tier has ever accepted**, and the reason they live here rather than on
// the web tier is the tracker page's own leak rule: it must never carry the room's id, so a write
// route has to be keyed by the tracker id, and `/tracker/**` is routed to `puna-tracker`. Putting
// them on the web tier would mean either publishing the room id onto this page or inventing a
// web-tier path that carries a tracker id.
//
// What that costs is stated rather than hidden: this tier's character moves from "reads and caches"
// to "reads, caches, and accepts authenticated per-slot edits". What it does *not* cost is any new
// reach: it already holds a database connection and already writes `last_tracker_doc`, and its
// NetworkPolicy needs no change. It still has no ServiceAccount token, no Discord credentials and no
// artifact volume.
//
// CSRF is covered the way every other POST in this project is: the session is a Rocket private
// cookie with `SameSite=Lax`, which a cross-site POST does not carry.

/// One slot's annotations.
#[derive(rocket::FromForm)]
struct AnnotationForm {
    progression: String,
    /// **Not filtered for blankness by the form**: empty is how a note is deleted, and
    /// `annotation::set_slot_annotation` is where that becomes an absent value.
    note: String,
}

/// Set a slot's progression and note.
///
/// **The slot's holder, or the room's staff**, which is the rule the feature was asked for with:
/// organizers and helpers may change anything a player can, because a note is the sort of thing an
/// organizer occasionally has to correct or remove.
#[rocket::post("/tracker/<id>/slot/<number>/annotation", data = "<form>")]
async fn set_annotation(
    id: TrackerParam,
    number: i32,
    session: Session,
    form: rocket::form::Form<AnnotationForm>,
    pool: &State<Pool>,
) -> Result<rocket::response::Flash<rocket::response::Redirect>> {
    let mut conn = pool.get().await?;
    let access = access(&mut conn, &session, id.0).await?;

    // The room has to have opted in. Without this the feature would be reachable by POST on a room
    // that never turned it on: a control nobody can see is still a route anybody can construct.
    if !access.sees_annotations() {
        return Err(not_found("this room does not use the enhanced tracker"));
    }
    let Some(actor) = access.viewer else {
        return Err(unauthorized("sign in to annotate a slot"));
    };

    let slot = slot::list(&mut conn, access.room.id)
        .await?
        .into_iter()
        .find(|s| s.slot_number == number)
        .ok_or_else(|| not_found("no such slot"))?;

    if !(access.is_staff || slot.owner_id == Some(actor)) {
        return Err(forbidden("that is not your slot"));
    }

    let progression = annotation::ProgressionStatus::parse(&form.progression)
        .ok_or_else(|| Error::new(Status::BadRequest, anyhow::anyhow!("unknown progression")))?;

    // Bounded here as well as by the column, so an over-long note is a sentence rather than a
    // database error. **Characters, not bytes**, so the limit does not depend on the alphabet.
    if form.note.chars().count() > annotation::MAX_NOTE_CHARS {
        return Err(Error::new(
            Status::BadRequest,
            anyhow::anyhow!(
                "that note is longer than {} characters",
                annotation::MAX_NOTE_CHARS
            ),
        ));
    }

    annotation::set_slot_annotation(
        &mut conn,
        access.room.id,
        number,
        progression,
        Some(&form.note),
        actor,
    )
    .await?;

    // **Only when somebody edits a slot that is not theirs.** A player writing their own note is
    // ordinary use and would bury the room's history; staff changing somebody else's is the case
    // where "who did this" gets asked, and it is the one the column alone cannot answer after the
    // next edit overwrites `annotated_by`.
    if slot.owner_id != Some(actor) {
        event::record(
            &mut conn,
            access.room.id,
            event::Actor::User(actor),
            "annotated_slot",
            serde_json::json!({ "slot": number, "owner": slot.owner_id }),
        )
        .await?;
    }

    Ok(rocket::response::Flash::success(
        rocket::response::Redirect::to(format!("/tracker/{}", id.0)),
        "Saved.",
    ))
}

#[derive(rocket::FromForm)]
struct PreferenceForm {
    preference: String,
}

/// Record how the viewer wants to be pinged about this room.
///
/// **Their own, and only their own.** Staff may edit any note and no ping preference: it is the one
/// field here that records what a person agreed to rather than a fact about a world, and
/// `annotation::set_preference` takes no actor for that reason: there is no caller who could set
/// somebody else's.
///
/// Holding a slot is required, not merely being staff: the chip renders beside a slot's owner, so a
/// preference from somebody who holds none would be a row nothing reads.
#[rocket::post("/tracker/<id>/preference", data = "<form>")]
async fn set_ping_preference(
    id: TrackerParam,
    session: Session,
    form: rocket::form::Form<PreferenceForm>,
    pool: &State<Pool>,
) -> Result<rocket::response::Flash<rocket::response::Redirect>> {
    let mut conn = pool.get().await?;
    let access = access(&mut conn, &session, id.0).await?;

    if !access.sees_annotations() {
        return Err(not_found("this room does not use the enhanced tracker"));
    }
    let Some(actor) = access.viewer else {
        return Err(unauthorized("sign in to set a preference"));
    };
    if !access.owns_a_slot {
        return Err(forbidden(
            "only somebody playing in this room has one to set",
        ));
    }

    let preference = annotation::PingPreference::parse(&form.preference)
        .ok_or_else(|| Error::new(Status::BadRequest, anyhow::anyhow!("unknown preference")))?;
    annotation::set_preference(&mut conn, access.room.id, actor, preference).await?;

    Ok(rocket::response::Flash::success(
        rocket::response::Redirect::to(format!("/tracker/{}", id.0)),
        "Saved. Every slot you hold in this room shows it.",
    ))
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
        view_items,
        set_annotation,
        set_ping_preference
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **`summary.txt` does not collide with the reference-compatible per-slot URL**, which is the
    /// open question about putting it under `/tracker/<id>/`: that space is otherwise the slot
    /// page's.
    ///
    /// It does not, because the shapes differ in length (`/tracker/<id>/summary.txt` is three
    /// segments and `/tracker/<id>/<team>/<player>` is four), but "differ in length" is a claim
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
        // deadpool and nothing else (connections are made on `get()`), so this is here to
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

        // Nobody else's name is anywhere in the rendered document, which is the assertion that
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

    /// Item-link groups name their members, so a scoped view drops them, along with any key a
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
            notice: None,
            annotations: false,
            write_base: "/tracker/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            owns_a_slot: false,
            my_preference: "unknown",
            ping_choices: Vec::new(),
            progression_choices: Vec::new(),
            note_limit: 1000,
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

    /// **The "Held by" column and the header count move together, or the table shifts.**
    ///
    /// The skeleton is server-rendered and the bodies are the client's, so the `<th>` and
    /// `tracker.js`'s cell array have to agree about how many columns there are. They agree because
    /// both read one flag (the `<th>` from `{% if annotations %}` and the script from
    /// `data-annotations`), and this asserts the two appear and disappear together.
    ///
    /// Getting it wrong does not fail: every cell after the missing one renders under the heading to
    /// its left, so checks appear under Game and the page looks like data rather than like a bug.
    #[test]
    fn the_owner_column_and_the_flag_the_client_reads_appear_together() {
        use askama::Template;

        let render = |annotations| {
            let mut page = tracker_page(None);
            page.annotations = annotations;
            page.render().expect("renders")
        };

        let off = render(false);
        assert!(
            !off.contains(r#"data-key="held_by""#),
            "an outsider's tracker grew a \"Held by\" column"
        );
        assert!(
            !off.contains("data-annotations"),
            "the client would build a cell the header has no column for"
        );

        let on = render(true);
        // **The room page's word, and a key that matches it.** The key is `tracker.js`'s handle on
        // this column and is no longer the name of a field on the row, so what makes it sort is an
        // explicit `sortValues` entry, pinned separately, since a key with no entry falls back to
        // a lookup that finds nothing and orders by nothing.
        assert!(on.contains(r#"<th data-key="held_by">Held by</th>"#));
        assert!(on.contains(r#"data-annotations="1""#));

        // The summary spans the table it sits under. Counted the way the standing lint counts it,
        // so the two cannot disagree about what "one column" means.
        for html in [&off, &on] {
            let slots = html
                .split_once(r#"data-view="slots""#)
                .expect("the slots table")
                .1
                .split_once("</section>")
                .expect("unterminated")
                .0;
            let headings = slots.matches("<th data-key=").count();
            let foot = slots
                .split_once("<tfoot")
                .expect("the summary")
                .1
                .split_once("</tfoot>")
                .expect("unterminated")
                .0;
            let spanned = foot.matches("<td").count() + foot.matches(r#"colspan="2""#).count();
            assert_eq!(
                spanned, headings,
                "the summary spans {spanned} columns and the table declares {headings}"
            );
        }
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
            notice: None,
            annotations: false,
            write_base: "/tracker/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            owns_a_slot: false,
            my_preference: "unknown",
            ping_choices: Vec::new(),
            progression_choices: Vec::new(),
            note_limit: 1000,
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

    /// **A bare `?o` is on, and so is `?o=1`**, which is the whole reason this is not a `bool`.
    ///
    /// Rocket's `FromFormField for bool` accepts an empty value, `on`, `yes` and `true`, and
    /// **refuses `1`** by falling through to a `ParseBoolError`, which fails the entire request with
    /// a 422 and no explanation. `1` is the first thing anybody types after `=`, and this endpoint's
    /// URL is written by hand into a bot's config and then not looked at again.
    ///
    /// It refuses an unknown value rather than reading presence alone as on, because `?o=false`
    /// meaning *on* is the same trap facing the other way.
    #[test]
    fn a_flag_is_on_when_bare_and_refuses_what_it_cannot_read() {
        for on in ["", "1", "on", "yes", "true", "TRUE", " 1 "] {
            assert_eq!(flag("o", Some(on)).ok(), Some(true), "`o={on:?}` is not on");
        }
        for off in ["0", "off", "no", "false", "False"] {
            assert_eq!(
                flag("o", Some(off)).ok(),
                Some(false),
                "`o={off:?}` is not off"
            );
        }
        assert_eq!(flag("o", None).ok(), Some(false), "absent is off");

        // Named rather than swallowed: a value nobody can read is a URL somebody got wrong, and
        // choosing a meaning for it is how a bot reports the wrong thing indefinitely.
        assert!(flag("o", Some("maybe")).is_err());
        assert!(flag("o", Some("2")).is_err());
    }

    /// `?s=1,2,4` picks slots, and **anything it cannot honor is refused rather than dropped**.
    ///
    /// Silently omitting an unknown slot is the failure worth avoiding: the summary would list
    /// fewer entries than were asked for, which reads as the room having changed rather than as the
    /// URL being wrong, and this URL lives in a config somebody wrote once.
    #[test]
    fn a_slot_selection_is_deduplicated_ordered_and_strict() {
        // Only the slot numbers matter here; `selection` reads nothing else off a roster.
        let numbered = |slot_number: i32| slot::Slot {
            room_id: puna_core::ids::RoomId::new(),
            slot_number,
            player_name: format!("p{slot_number}"),
            game: "A Link to the Past".into(),
            kind: puna_core::artifact::SlotKind::Player,
            password: None,
            owner_id: None,
            claim_token: None,
            claimed_at: None,
            tracker_id: puna_core::ids::TrackerId::new(),
            locked_at: None,
            locked_by: None,
            progression: puna_core::model::annotation::ProgressionStatus::Unknown,
            note: None,
            annotated_at: None,
            annotated_by: None,
        };
        let roster: Vec<slot::Slot> = [1, 2, 4].into_iter().map(numbered).collect();

        assert_eq!(selection(None, &roster, None).unwrap(), None, "no filter");
        assert_eq!(
            selection(Some("1,2,4"), &roster, None).unwrap(),
            Some(vec![1, 2, 4])
        );
        // Deduplicated, so a slot cannot be listed twice by asking twice.
        assert_eq!(
            selection(Some("2,2, 1 "), &roster, None).unwrap(),
            Some(vec![2, 1])
        );

        // Blank reads as absent rather than as an empty selection: an empty one could only produce
        // `no slots`, so "everything" is the reading that might be what somebody meant.
        for blank in ["", "  ", ",,", " , "] {
            assert_eq!(selection(Some(blank), &roster, None).unwrap(), None);
        }

        assert!(
            selection(Some("1,x"), &roster, None).is_err(),
            "not a number"
        );
        assert!(selection(Some("1,9"), &roster, None).is_err(), "not a slot");

        // A slot's own tracker id already names its slot, so combining it with a different one is
        // two answers to one question: `scope_of`'s rule, and its `404`.
        assert!(selection(Some("2"), &roster, Some(1)).is_err());
        assert_eq!(
            selection(Some("1"), &roster, Some(1)).unwrap(),
            Some(vec![1]),
            "naming the slot the id already names is agreement, not conflict"
        );
        assert_eq!(selection(None, &roster, Some(1)).unwrap(), None);
    }

    /// **And the digested views have to ask for that**, which the responder above cannot enforce.
    ///
    /// `json` is the one funnel every `/api/puna/tracker/**` view goes through, so pinning its
    /// argument pins all four. Without this the header test passes against the original bug intact:
    /// `respond` would still map `Derived` to `no-cache` correctly, and nothing would be asking it
    /// for `Derived`.
    ///
    /// The same call-site shape this project has now been bitten by three times: a good test on a
    /// rule, and nothing checking that the rule is invoked.
    #[test]
    fn the_digested_views_ask_to_be_revalidated() {
        let source = include_str!("tracker.rs");
        let at = source
            .find("fn json<T: serde::Serialize>")
            .expect("the digest responder is gone, so this lint checks nothing");
        let body = &source[at..];
        let body = &body[..body.find("\n}\n").expect("unterminated")];

        assert!(
            body.contains("Caching::Derived"),
            "the digested views are served with a window that describes pahoa's document rather \
             than Puna's rows, so an annotation saved now will not be visible until it expires"
        );
        assert!(
            !body.contains("Caching::Upstream"),
            "a digested view is being served as though it were a passthrough"
        );
    }

    /// **A view Puna derives must be revalidated; a passthrough of pahoa's document need not be.**
    ///
    /// This is a fix rather than a preference. Every response here used to carry `max-age` from
    /// pahoa's window, which is right only while the response *is* pahoa's document. The digested
    /// views also read Puna's own rows (a slot's owner, its note, its progression, its holder's
    /// ping preference), and those change when somebody presses Save. Under `max-age` the browser
    /// answered its own fetch without asking, so saving an annotation and landing back on the
    /// tracker showed the previous body for the rest of the window.
    ///
    /// **Nothing server-side can see that failure**, which is why it is asserted on the header: the
    /// route runs correctly, returns correct data, and the browser never calls it. The only evidence
    /// is one string in one response.
    #[test]
    fn a_derived_view_is_revalidated_and_a_passthrough_is_not() {
        let header = |caching| match respond("{}".to_string(), caching, &IfNoneMatch(None)) {
            Json::Body(cached) => cached.cache_control.value().to_string(),
            Json::Unchanged(_) => panic!("a fresh request answered 304"),
        };

        assert_eq!(
            header(Caching::Derived),
            "private, no-cache",
            "a derived view may be reused without asking, so a save will not show up"
        );

        // The passthroughs keep pahoa's own window, where it is exactly the right answer: asking
        // again sooner cannot produce different data.
        assert_eq!(
            header(Caching::Upstream(Document::Live)),
            "private, max-age=60"
        );
        assert_eq!(
            header(Caching::Upstream(Document::Static)),
            "private, max-age=300"
        );

        // `private` on every one of them: the digested views are per-viewer on every policy, so a
        // shared cache in front of this must never hand one reader another's document.
        for caching in [
            Caching::Derived,
            Caching::Upstream(Document::Live),
            Caching::Upstream(Document::Static),
        ] {
            assert!(header(caching).starts_with("private"));
        }
    }

    /// A caller presenting the current ETag gets a 304 and no body, which is the layer that removes
    /// the most work.
    #[test]
    fn a_matching_etag_is_answered_with_304() {
        let body = r#"{"hints":[]}"#.to_string();
        let etag = format!("\"{}\"", puna_core::hash::sha256_hex(body.as_bytes()));

        assert!(matches!(
            respond(body.clone(), Caching::Derived, &IfNoneMatch(Some(etag))),
            Json::Unchanged(_)
        ));
        assert!(matches!(
            respond(body.clone(), Caching::Derived, &IfNoneMatch(None)),
            Json::Body(_)
        ));
        // A stale ETag is a full response, not a 304.
        assert!(matches!(
            respond(body, Caching::Derived, &IfNoneMatch(Some("\"old\"".into()))),
            Json::Body(_)
        ));
    }
    /// Build a tracker page for one slot, or for the whole multiworld.
    /// **A slot's tracker and the multiworld's are two tabs, and were titled identically.**
    ///
    /// A player watching their own world beside the room's had no way to tell them apart, the same
    /// complaint that put the page name in front of the room name everywhere on 2026-08-31, and the
    /// one case where the page name alone still would not have been enough.
    ///
    /// The player is named on the page and in the document it fetches, so the tab discloses nothing
    /// to somebody who is already holding the id that renders it.
    #[test]
    fn a_slots_tracker_says_whose_it_is_and_the_multiworlds_does_not() {
        use askama::Template;

        let title = |slot| {
            let html = tracker_page(slot).render().expect("renders");
            html.split_once("<title>")
                .and_then(|(_, rest)| rest.split_once("</title>"))
                .map(|(title, _)| title.to_string())
                .expect("a title")
        };

        assert_eq!(title(None), "Tracker: Friday async");
        assert_eq!(title(Some(3)), "Tracker (Troy): Friday async");
    }

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
            notice: None,
            annotations: false,
            write_base: "/tracker/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            owns_a_slot: false,
            my_preference: "unknown",
            ping_choices: Vec::new(),
            progression_choices: Vec::new(),
            note_limit: 1000,
            slot,
        }
    }

    /// **A filter over a table that always has one row can only hide it.** A slot's own page renders
    /// exactly one slot, so the box is offered on the multiworld view and nowhere else.
    ///
    /// Asserted by counting, because both pages carry several search boxes and "contains a search
    /// box" is true either way: the question is how many.
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
            "the slot view's filter count is wrong: the one-row slot table should have none"
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
    /// somebody scrolls a long list, which is exactly the case these two exist for.
    #[test]
    fn a_slots_locations_and_items_scroll_inside_one_bounded_wrapper() {
        let html = tracker_page(Some(1)).render().expect("renders");

        assert_eq!(
            html.matches(r#"class="table-scroll bounded""#).count(),
            2,
            "locations and items are the two tables nobody chose the length of"
        );

        // Each bounded wrapper holds a table DIRECTLY: no second scroller in between.
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
    /// `toggles.js` restores it while `tracker.js` reacts to it. Every mismatch is silent: the box
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
        // rather than any mention: a lint that matches its own prose has happened four times in
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
    /// key would make a choice on one page silently change the other, which looks like the setting
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
        // signal the template branches on: `data-slot` being present.
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

    /// **Both writes check the same four things, and each is a different way in.**
    ///
    /// These are the first mutations this tier has ever accepted, and every guard on them is a
    /// separate refusal with no other symptom if it goes missing:
    ///
    /// * `sees_annotations()`: the room opted in, and the caller is one of its people. Without it
    ///   the feature is reachable by POST on a room that never turned it on: a control nobody can
    ///   see is still a route anybody can construct.
    /// * a signed-in caller: there is no annotation without somebody to attribute it to.
    /// * for a slot, `is_staff || slot.owner_id == Some(actor)`: otherwise any participant edits
    ///   any slot, which is the quiet one, because the page would look correct to whoever did it.
    /// * for a preference, `owns_a_slot` and **no actor parameter at all**: the writer takes only
    ///   the caller's own id, so setting somebody else's is unspellable rather than merely refused.
    ///
    /// A source lint because there is no unit test that reaches a Rocket route's body, and the
    /// alternative (a full router harness per guard) is what M21 built once and is far more than
    /// this needs.
    #[test]
    fn both_writes_check_the_room_the_caller_and_the_slot() {
        let source = include_str!("tracker.rs");
        let body_of = |name: &str| {
            let at = source
                .find(name)
                .unwrap_or_else(|| panic!("{name} is gone, so this lint checks nothing"));
            let rest = &source[at..];
            rest[..rest.find("\n}\n").expect("unterminated")].to_string()
        };

        let annotation = body_of("async fn set_annotation(");
        for guard in [
            "access.sees_annotations()",
            "access.viewer",
            "access.is_staff || slot.owner_id == Some(actor)",
        ] {
            assert!(
                annotation.contains(guard),
                "the annotation route no longer checks `{guard}`"
            );
        }

        let preference = body_of("async fn set_ping_preference(");
        for guard in [
            "access.sees_annotations()",
            "access.viewer",
            "access.owns_a_slot",
        ] {
            assert!(
                preference.contains(guard),
                "the preference route no longer checks `{guard}`"
            );
        }
        // The subject is the caller, always. Anything else appearing here would be this route
        // learning how to write somebody else's answer.
        assert!(
            preference.contains("set_preference(&mut conn, access.room.id, actor, preference)"),
            "the preference route writes something other than the caller's own answer"
        );
    }

    /// **The one response here that a shared cache may hold must not depend on who asked.**
    ///
    /// `summary.txt` is served `public`-cacheable precisely because it is identical for every
    /// reader: that is what lets a bot with no session ask for it and what makes a cache in front
    /// of it free rate limiting rather than a way to hand one viewer another's document. Every JSON
    /// view beside it is per-viewer and is not cacheable that way.
    ///
    /// So this path passes `sees_claims: false` unconditionally. `summary` renders no claim state
    /// today, so flipping it would leak nothing *now*, which is exactly why it needs a lint rather
    /// than a behavioral test: the mutation compiles, changes no output, and arms the leak for
    /// whoever next adds a column to `digest::summary`. Two edits in different files, neither
    /// wrong on its own.
    #[test]
    fn the_cacheable_summary_never_asks_for_a_viewer_dependent_field() {
        let source = include_str!("tracker.rs");
        let at = source
            .find("async fn summary_text(")
            .expect("the summary route is gone");
        // Bounded to this function: the next line that starts a new item at column zero.
        let body = &source[at..];
        let body = &body[..body.find("\n}\n").expect("an unterminated function")];

        assert!(
            body.contains("digest::slot_rows("),
            "this lint is no longer looking at the call it exists to pin"
        );
        // Every way this response could become viewer-dependent, named. The tier lives on `Access`
        // and reaches the digest through `Digestible::viewer`, so a path that reads any of these is
        // a path that has started answering differently for different readers.
        for viewer_shaped in ["is_participant", "is_staff", "it.viewer()", "people"] {
            assert!(
                !body.contains(viewer_shaped),
                "summary.txt reads {viewer_shaped:?} while still being served `public`-cacheable, \
                 so a shared cache can hand one reader another's document"
            );
        }
        assert!(
            body.contains("Viewer::outsider()"),
            "summary.txt no longer builds the outsider's view, so whatever it does build is worth \
             checking against the `public` cache-control below it"
        );
    }
}
