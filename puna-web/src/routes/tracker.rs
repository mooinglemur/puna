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
            // Unbounded in principle, bounded in practice by the room count and swept by age on
            // read. A room that stops being asked about holds one entry until the process restarts;
            // at a few hundred rooms that is not worth an eviction policy.
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
async fn names_for(
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
                version: "test",
                static_version: "test",
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
                version: "test",
                static_version: "test",
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
}
