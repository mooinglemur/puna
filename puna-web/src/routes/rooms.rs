//! Rooms: the public page, the lifecycle buttons, the roster, invites and claims.
//!
//! Every room-scoped route spells the room id as its **first** dynamic segment, because that is
//! where [`RoomAccess`](crate::guards::RoomAccess) and [`SlotAccess`](crate::guards::SlotAccess)
//! look for it. See the note at the top of `guards.rs`.
//!
//! `GET /room/<id>` is deliberately **public**: players arrive from a shared link, and the
//! unguessable id is the authorization, exactly as the reference implementation does it. What that
//! page shows varies by who is looking -- credentials only ever through `SlotAccess`.

use puna_core::model::event;
use puna_core::model::generation;
use puna_core::model::member::{self, MemberError, RoomRole};
use puna_core::model::room::{self, DesiredState, MyRoom, Room, SlotAuth};
use puna_core::model::slot::{self, Slot};
use puna_core::model::user;
use rocket::form::Form;
use rocket::http::Status;
use rocket::response::Redirect;
use rocket::serde::json::Json;
use rocket::{FromForm, State, get, post, routes};

use askama::Template;
use askama_web::WebTemplate;

use crate::auth::{LoggedInSession, Session};
use crate::error::{Error, Result, not_found};
use crate::gate::{CanCreateRoom, Direct};
use crate::guards::{Helper, Navigation, Organizer, RoomAccess, SlotAccess};
use crate::params::RoomParam;
use crate::tpl::TplContext;

type Pool = puna_core::db::Pool;

#[derive(Template, WebTemplate)]
#[template(path = "rooms/list.html")]
pub struct MyRoomsTemplate {
    base: TplContext,
    rooms: Vec<MyRoom>,
}

#[derive(Template, WebTemplate)]
#[template(path = "rooms/show.html")]
pub struct RoomTemplate {
    base: TplContext,
    room: Room,
    slots: Vec<SlotView>,
    /// Precomputed rather than left as an `Option<RoomRole>` for the template to compare against.
    /// Askama binds pattern captures by reference, so `role >= Organizer` in markup is a type
    /// error waiting to be papered over with a deref -- and a template is the worst place to put
    /// an authorization comparison anyway.
    is_staff: bool,
    is_organizer: bool,
    siblings: Vec<Room>,
    /// From `room::may_see_spoiler`, so the link and the download route answer the same question.
    /// A page that offers a link the route refuses is a bug report; one that hides a link the route
    /// would serve teaches people to guess URLs.
    can_see_spoiler: bool,
    /// From `room::may_see_tracker`, for the same reason as `can_see_spoiler`: the page and the
    /// tracker tier answer one question in one place.
    ///
    /// **Deliberately not gated on the room being `running`.** The tracker serves
    /// `last_tracker_doc` behind an "as of <time>" banner when the room is down, which for an async
    /// is most of its life -- that fallback is a designed feature, and hiding the link while it
    /// applies would conceal the page exactly when it is most useful.
    can_see_tracker: bool,
    /// The latest event in words, for the transient states. `None` renders the state itself, which
    /// is worse but never wrong.
    message: Option<&'static str>,
    /// Already formatted, because a template is not where a duration should be turned into English
    /// -- and because the same string is what `room.js` overwrites on its first poll.
    elapsed: String,
    /// Whether this room is closed, and therefore whether the page offers a start control at all.
    ///
    /// Computed here rather than compared in markup, for the same reason `is_staff` is: this is an
    /// authorization decision, and the route that would refuse the start has to be answering the
    /// same question the page answers. A page offering a button the route rejects is worse than one
    /// that hides it.
    is_closed: bool,
    /// Whether the viewer may start this room *right now* -- which is everybody for an idle room,
    /// and staff only for a closed one.
    may_start: bool,
    /// Whether the room is mid-transition, INCLUDING a request the orchestrator has not reached
    /// yet. See [`is_working`] -- rendering from `state` alone is what made a click bounce back to
    /// the display it had just replaced.
    is_working: bool,
}

/// One row of the room page's slot table.
///
/// Deliberately NOT `slot::Slot`: that struct carries the password and the claim token, and a
/// template has no way to prove it did not render them. This carries only what the page shows,
/// so a credential cannot reach the markup by a template author's oversight.
pub struct SlotView {
    pub slot_number: i32,
    pub player_name: String,
    pub game: String,
    pub is_spectator: bool,
    pub owner_id: Option<i64>,
    pub is_mine: bool,
    /// Present only when the viewer may see it -- staff, or the unclaimed-slot case.
    pub claim_token: Option<String>,
    /// Whether this slot has a patch file at all. **Most games do not** -- they are played with a
    /// client rather than a patched ROM -- so offering the link unconditionally would promise a
    /// download that answers 404 with an explanation nobody asked for.
    pub has_patch: bool,
    /// Whether this viewer would get past `SlotAccess`: its owner, the room's staff, or an admin.
    pub can_download: bool,
    /// This slot's own tracker id, and **only when the viewer owns the slot**.
    ///
    /// The reason is capability tiers, not confidentiality. `GET /room/<id>` is a PUBLIC page under
    /// the default `link` policy -- the unguessable room id is the whole authorization -- so
    /// rendering every slot's tracker id here would mean holding the room URL yields all of them.
    /// That collapses two deliberately separate capabilities into one and makes the slot id's
    /// independence pointless: it exists so a player can share their own progress *without* handing
    /// over the multiworld's.
    ///
    /// **Staff are not a special case, and the narrow choice here is not a strong one.** An
    /// organizer already sees every slot's progress through the room-level tracker, so withholding
    /// the per-slot link discloses nothing they cannot reach -- and anyone minded to leak would
    /// share the room tracker, which shows strictly more. Widening this to staff would cost no
    /// confidentiality; it is kept to owners because that is the smallest rule that satisfies the
    /// public-page constraint above, not because staff seeing it would be unsafe.
    ///
    /// Also gated on `can_see_tracker` at the template, since `disabled` policy 404s every tracker
    /// id including a slot's own.
    pub tracker_id: Option<puna_core::ids::TrackerId>,
}

fn slot_views(
    slots: Vec<Slot>,
    viewer: Option<i64>,
    role: Option<RoomRole>,
    patched: &std::collections::HashSet<i32>,
) -> Vec<SlotView> {
    slots
        .into_iter()
        .map(|s| SlotView {
            is_mine: matches!((viewer, s.owner_id), (Some(v), Some(o)) if v == o),
            has_patch: patched.contains(&s.slot_number),
            // Owner only, and NOT widened to staff -- see the field's note. Computed from the same
            // comparison as `is_mine` rather than from it, so the two cannot drift apart.
            tracker_id: match (viewer, s.owner_id) {
                (Some(v), Some(o)) if v == o => Some(s.tracker_id),
                _ => None,
            },
            // The same three-way rule `SlotAccess` applies, and it deliberately does NOT include
            // "holds some other slot in this room".
            can_download: role.is_some()
                || matches!((viewer, s.owner_id), (Some(v), Some(o)) if v == o),
            // A claim link is offered to staff, who hand them out. A player who already holds the
            // link does not need the page to show it, and showing it to everyone would let any
            // visitor claim every unclaimed slot in a room whose URL they were given.
            claim_token: if role.is_some() { s.claim_token } else { None },
            slot_number: s.slot_number,
            player_name: s.player_name,
            game: s.game,
            is_spectator: s.kind == puna_core::artifact::SlotKind::Spectator,
            owner_id: s.owner_id,
        })
        .collect()
}

/// Take the room down **and stop anyone but staff bringing it back.**
///
/// Organizer-guarded like [`stop`], and it is the same instruction to the orchestrator: the room
/// comes down keeping its port reservation and its state directory, so reopening it returns it on
/// the address its players have bookmarked. What differs is only who may ask for it to run.
///
/// Reopening is [`start`], which is why there is no separate route for it: a closed room that an
/// organizer starts is simply a room somebody wants running again, and giving that its own verb
/// would mean two ways to spell one transition.
#[post("/room/<id>/close")]
async fn close(
    id: RoomParam,
    access: RoomAccess<Organizer>,
    pool: &State<Pool>,
) -> Result<Redirect> {
    let mut conn = pool.get().await?;
    if room::request_state(&mut conn, id.0, DesiredState::Closed).await? {
        event::record(
            &mut conn,
            id.0,
            event::Actor::User(access.user_id()),
            "requested_close",
            serde_json::json!({}),
        )
        .await?;
    }
    tracing::info!(
        room = %id,
        user_id = access.user_id(),
        role = access.role().as_sql(),
        "close requested"
    );
    Ok(Redirect::to(format!("/room/{id}")))
}

#[derive(FromForm)]
struct CreateRoomForm {
    generation_id: String,
    name: String,
    slot_auth: String,
}

/// Open a room from a generation you have already uploaded.
///
/// The upload and the room are separate steps on purpose: one generation can back any number of
/// rooms, which is the same affordance the reference implementation offers as "create a new room
/// from this seed", and it costs no storage because generations are content-addressed and shared.
#[post("/rooms", data = "<form>")]
async fn create(
    gate: CanCreateRoom<Direct>,
    form: Form<CreateRoomForm>,
    pool: &State<Pool>,
    environment: &State<puna_core::Environment>,
) -> Result<Redirect> {
    let generation_id = form
        .generation_id
        .parse()
        .map_err(|_| Error::new(Status::BadRequest, anyhow::anyhow!("not a generation id")))?;
    let name = form.name.trim();
    if name.is_empty() {
        return Err(Error::new(
            Status::BadRequest,
            anyhow::anyhow!("a room needs a name"),
        ));
    }
    let slot_auth = SlotAuth::parse(&form.slot_auth)
        .ok_or_else(|| Error::new(Status::BadRequest, anyhow::anyhow!("unknown password mode")))?;

    let mut conn = pool.get().await?;

    // The generation must exist before a room can reference it, and saying so here beats a
    // foreign-key violation surfacing as a 500.
    if generation::get(&mut conn, generation_id).await?.is_none() {
        return Err(not_found("no such generation"));
    }

    let mut new = room::NewRoom::direct(**environment, name, generation_id, gate.user_id());
    new.slot_auth = slot_auth;
    let id = room::create(&mut conn, &new).await?;

    tracing::info!(
        room = %id,
        generation = %generation_id,
        user_id = gate.user_id(),
        grant = ?gate.grant(),
        slot_auth = slot_auth.as_sql(),
        "room created"
    );
    Ok(Redirect::to(format!("/room/{id}")))
}

/// Your rooms, by either route into them.
#[get("/rooms")]
async fn my_rooms(session: LoggedInSession, pool: &State<Pool>) -> Result<MyRoomsTemplate> {
    let mut conn = pool.get().await?;
    let rooms = room::mine(&mut conn, session.user_id()).await?;
    Ok(MyRoomsTemplate {
        base: TplContext::new(session.session()),
        rooms,
    })
}

/// The public room page.
#[get("/room/<id>")]
async fn show(
    id: RoomParam,
    session: Session,
    navigation: Navigation,
    pool: &State<Pool>,
) -> Result<RoomTemplate> {
    let mut conn = pool.get().await?;
    let room = room::get(&mut conn, id.0)
        .await?
        .ok_or_else(|| not_found("no such room"))?;

    // D8: a person arriving at an idle room's URL wants it back, and making them click a button
    // first is friction for the common case. A link preview is not a person, which is what
    // `Navigation` sorts out -- and the write is idempotent either way, so a room already coming up
    // is untouched.
    // A closed room is never started by arriving at it, whoever arrives. Staff get a button; the
    // implicit trigger is for the case where wanting the page IS wanting the room, and a closed
    // room is precisely where that stops being true.
    let room = if navigation.0
        && room.state == "idle"
        && room.desired_state != "running"
        && room.desired_state != DesiredState::Closed.as_sql()
    {
        room::request_state(&mut conn, room.id, DesiredState::Running).await?;
        event::record(
            &mut conn,
            room.id,
            event::Actor::web(session.user_id),
            "requested_start",
            serde_json::json!({ "implicit": true }),
        )
        .await?;
        tracing::info!(room = %room.id, user_id = ?session.user_id, "implicit start on navigation");
        // Re-read rather than patching the copy in hand: the page renders from the row, and a row
        // that disagrees with the database is how a spinner ends up showing the wrong thing.
        room::get(&mut conn, id.0)
            .await?
            .ok_or_else(|| not_found("no such room"))?
    } else {
        room
    };

    let role = resolve_role(&mut conn, &session, room.id).await?;

    // Which slots have a patch is a property of the *generation*, not of the room's copy: the room
    // owns who holds a slot, the generation owns what a slot's file is.
    let patched: std::collections::HashSet<i32> = generation::slots(&mut conn, room.generation_id)
        .await?
        .into_iter()
        .filter(|entry| entry.patch_member.is_some())
        .map(|entry| entry.slot_number)
        .collect();

    let room_slots = slot::list(&mut conn, room.id).await?;
    let owns_a_slot = session
        .user_id
        .is_some_and(|user_id| room_slots.iter().any(|s| s.owner_id == Some(user_id)));
    let can_see_spoiler = room::may_see_spoiler(room.spoiler_policy, role.is_some(), owns_a_slot);
    let can_see_tracker = room::may_see_tracker(room.tracker_policy, role.is_some(), owns_a_slot);

    let slots = slot_views(room_slots, session.user_id, role, &patched);
    let siblings = room::siblings(&mut conn, room.id, room.generation_id).await?;
    let message = event::latest(&mut conn, room.id)
        .await?
        .and_then(|e| phrase(&e.kind));
    let elapsed = human_duration(since_ms(room.state_changed_at));

    let is_closed = room.desired_state == DesiredState::Closed.as_sql();
    Ok(RoomTemplate {
        base: TplContext::new(&session),
        // Both from `may_start`, so the page and the route cannot disagree about who gets a door.
        is_closed,
        may_start: may_start(&room, role),
        is_working: is_working(&room),
        room,
        slots,
        is_staff: role.is_some(),
        is_organizer: role.is_some_and(|r| r >= RoomRole::Organizer),
        siblings,
        can_see_spoiler,
        can_see_tracker,
        message,
        elapsed,
    })
}

/// **The one place that decides who may bring a room up**, used by the page, the explicit start
/// route and D8's implicit start alike.
///
/// An ordinary room is startable by anyone holding its URL, and that is the design rather than an
/// oversight: a room that idles out and comes back when somebody visits it is the whole point of
/// the sticky port reservation, and requiring membership would strand every player whose async went
/// quiet. A **closed** room inverts exactly that one rule and nothing else — the page still renders
/// for everybody, with their patches, their tracker and the roster.
///
/// `role` is `Some(Organizer)` for a global admin, resolved by the caller.
fn may_start(room: &Room, role: Option<RoomRole>) -> bool {
    if room.desired_state != DesiredState::Closed.as_sql() {
        return true;
    }
    role.is_some_and(|r| r >= RoomRole::Organizer)
}

/// Whether the room is between states **as the person looking at it experiences that**.
///
/// Not the same question as "is `state` a transient value", and the difference is a window that can
/// last a full reconcile interval: a request writes `desired_state` and returns, and the observed
/// state does not move until the orchestrator reaches the room. A panel rendered from `state` alone
/// during that window shows the room exactly as it was — so somebody clicks Stop on a running room
/// and is handed back the address table, then watches it change on its own some seconds later.
///
/// That is a server-side fault rather than a scripting one: without JavaScript the same click
/// redirects to a page that says "This room is not running" **with a Start button**, having just
/// been asked to start it.
///
/// So a room is working when the orchestrator is acting **or** when it has been asked to and has
/// not got there yet.
///
/// Two deliberate exclusions:
///
///   * **`failed`** is at rest with an error and a retry time, even though its `desired_state` is
///     usually still `running`. A spinner there would hide the one thing worth reading — why it
///     failed — behind an animation, for up to the ten-minute backoff cap.
///   * **An idle room asked to close** is already where it is going. Nothing has to happen, so
///     showing it as in-flight would be waiting for an event that is never coming.
fn is_working(room: &Room) -> bool {
    match room.state.as_str() {
        // The orchestrator is mid-flight. `degraded` belongs here rather than with the settled
        // states: it is a room that is not answering, which is unsettled by definition.
        "provisioning" | "starting" | "stopping" | "deleting" | "degraded" => true,
        // Down and asked to come up.
        "idle" => room.desired_state == DesiredState::Running.as_sql(),
        // Up and asked to come down -- stopped or closed, which are the same instruction.
        "running" => room.desired_state != DesiredState::Running.as_sql(),
        _ => false,
    }
}

/// This session's role in a room, with a global admin resolving to the top of the ladder.
///
/// Factored out because `show` and `start` must answer it identically — the page decides whether to
/// render a control from this, and the route decides whether to honor one.
async fn resolve_role(
    conn: &mut diesel_async::AsyncPgConnection,
    session: &Session,
    room: puna_core::ids::RoomId,
) -> Result<Option<RoomRole>> {
    if session.is_admin {
        return Ok(Some(RoomRole::Organizer));
    }
    match session.user_id {
        Some(user_id) => Ok(member::role_of(conn, room, user_id).await?),
        None => Ok(None),
    }
}

/// The lifecycle panel on its own, for the poller to swap in.
///
/// **The same template file `show.html` includes**, which is the whole point: the page has one set
/// of branches deciding what a room's state looks like and who is offered a control, and a second
/// set written in JavaScript would be two things to keep in step — with the drifting one being the
/// half nobody reviews. The page would go on working while telling somebody the wrong thing about
/// their room.
///
/// Public, like the page it is part of, and it re-resolves the viewer's role rather than trusting
/// anything the client sends: this fragment decides whether to render a start control, so it is
/// making the same authorization decision `show` does and must make it the same way.
#[derive(Template, WebTemplate)]
#[template(path = "rooms/panel.html")]
pub struct PanelTemplate {
    room: Room,
    is_closed: bool,
    may_start: bool,
    is_working: bool,
    message: Option<&'static str>,
    elapsed: String,
}

#[get("/room/<id>/panel")]
async fn panel(id: RoomParam, session: Session, pool: &State<Pool>) -> Result<PanelTemplate> {
    let mut conn = pool.get().await?;
    let room = room::get(&mut conn, id.0)
        .await?
        .ok_or_else(|| not_found("no such room"))?;

    // Deliberately NO implicit start here, unlike `show`. D8's trigger is about a person arriving
    // at a room, and this is a poll -- firing it would mean a page left open on a stopped room
    // restarted it every few seconds, which is the link-unfurl hazard with a worse cadence.
    let role = resolve_role(&mut conn, &session, room.id).await?;
    let message = event::latest(&mut conn, room.id)
        .await?
        .and_then(|e| phrase(&e.kind));
    let elapsed = human_duration(since_ms(room.state_changed_at));

    Ok(PanelTemplate {
        is_closed: room.desired_state == DesiredState::Closed.as_sql(),
        may_start: may_start(&room, role),
        is_working: is_working(&room),
        room,
        message,
        elapsed,
    })
}

/// The poll target behind the starting spinner. Two row reads, no template.
///
/// `since_ms` is a **server-computed duration** rather than a timestamp, deliberately: a client
/// whose clock is wrong — and a cold start is exactly when someone is watching a counter — would
/// otherwise render an elapsed time that is minutes out or negative.
#[get("/room/<id>/status")]
async fn status(id: RoomParam, pool: &State<Pool>) -> Result<Json<serde_json::Value>> {
    let mut conn = pool.get().await?;
    let room = room::get(&mut conn, id.0)
        .await?
        .ok_or_else(|| not_found("no such room"))?;

    let latest = event::latest(&mut conn, room.id).await?;

    Ok(Json(serde_json::json!({
        "state": room.state,
        "desired_state": room.desired_state,
        "host": room.advertised_host,
        "port": room.advertised_port,
        "filtered_port": room.advertised_filtered_port,
        "last_error": room.last_error,
        "since_ms": since_ms(room.state_changed_at),
        // What the room is actually doing, in words. A spinner over "starting" for ninety seconds
        // is indistinguishable from a stuck one; "waiting for the pod to be scheduled" is not.
        "message": latest.as_ref().and_then(|e| phrase(&e.kind)),
    })))
}

/// Milliseconds since an instant, floored at zero.
///
/// Clamped because a row written by a machine whose clock is ahead would otherwise produce a
/// negative elapsed time, and "started -3 seconds ago" reads as a bug in the page rather than in
/// the clock.
fn since_ms(at: chrono::DateTime<chrono::Utc>) -> i64 {
    (chrono::Utc::now() - at).num_milliseconds().max(0)
}

/// The same shape `room.js` renders, so the first paint and the first poll do not disagree about
/// how to spell forty seconds.
fn human_duration(ms: i64) -> String {
    let seconds = (ms as f64 / 1000.0).round() as i64;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {}s", seconds / 60, seconds % 60)
    }
}

/// A room event, in words a player can act on.
///
/// Deliberately a small allowlist rather than a formatting of every kind: an event nobody has
/// written a sentence for renders as nothing, and the page falls back to the state. The failure
/// mode of the alternative -- showing raw kinds -- is a page that says `ip_mismatch` to a player.
fn phrase(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "provisioned" => "preparing this room's files",
        "requested_start" => "starting the room",
        "starting" => "waiting for the room's server to come up",
        "running" => "the room is up",
        "stopping" => "shutting the room down",
        "stopped" => "the room has stopped",
        "requested_close" => "closing the room",
        "deployment_gone" => "the room's server went away; it can be started again",
        "retrying" => "trying again after a failure",
        "degraded" => "the room is not answering; it may be restarting",
        "ip_mismatch" => "the address was wrong, so the room is moving to another port",
        "failed" => "the last attempt to start this room failed",
        "port_reclaimed" => "this room's port was reassigned while it was idle",
        _ => return None,
    })
}

/// Start an idle room.
///
/// Guarded only by holding the room URL, which is the same capability that reaches the page: a
/// player whose async has idled out must be able to bring it back without being on the roster.
/// D8's link-unfurl hazard is about the *implicit* trigger on `GET`; this is the explicit button.
#[post("/room/<id>/start")]
async fn start(id: RoomParam, session: Session, pool: &State<Pool>) -> Result<Redirect> {
    let mut conn = pool.get().await?;

    // **Re-checked here, not trusted from the page.** The page hides the control for a closed room,
    // and hiding a control is a courtesy rather than a boundary -- this route is reachable by anyone
    // who can construct a POST, which for a room whose URL is its only credential is anyone at all.
    let room = room::get(&mut conn, id.0)
        .await?
        .ok_or_else(|| not_found("no such room"))?;
    if !may_start(&room, resolve_role(&mut conn, &session, room.id).await?) {
        // 403 rather than 404: the room's existence is not the secret here -- the page renders it
        // to everybody -- so pretending it is gone would be a worse answer to a real question.
        return Err(Error::new(
            Status::Forbidden,
            anyhow::anyhow!("this room is closed; an organizer can reopen it"),
        ));
    }

    let changed = room::request_state(&mut conn, id.0, DesiredState::Running).await?;
    // Only when something changed: a second click on a room that is already coming up is not an
    // event, and recording it would fill a room's history with the sound of somebody being impatient.
    if changed {
        event::record(
            &mut conn,
            id.0,
            event::Actor::web(session.user_id),
            "requested_start",
            serde_json::json!({ "implicit": false }),
        )
        .await?;
    }
    tracing::info!(room = %id, user_id = ?session.user_id, changed, "start requested");
    Ok(Redirect::to(format!("/room/{id}")))
}

#[post("/room/<id>/stop")]
async fn stop(
    id: RoomParam,
    access: RoomAccess<Organizer>,
    pool: &State<Pool>,
) -> Result<Redirect> {
    let mut conn = pool.get().await?;
    if room::request_state(&mut conn, id.0, DesiredState::Stopped).await? {
        event::record(
            &mut conn,
            id.0,
            event::Actor::User(access.user_id()),
            "requested_stop",
            serde_json::json!({}),
        )
        .await?;
    }
    tracing::info!(
        room = %id,
        user_id = access.user_id(),
        role = access.role().as_sql(),
        "stop requested"
    );
    Ok(Redirect::to(format!("/room/{id}")))
}

#[derive(FromForm)]
struct CloneForm {
    name: String,
    keep_owners: bool,
}

/// Open a new room from this room's generation.
///
/// Two guards, because two separate things are being asserted: that the caller belongs to the
/// source room, and that they may create rooms at all. The creation gate is not implied by
/// membership -- an organizer of somebody else's room is not thereby a creator.
#[post("/room/<id>/clone", data = "<form>")]
async fn clone_room(
    id: RoomParam,
    access: RoomAccess<Helper>,
    gate: CanCreateRoom<Direct>,
    form: Form<CloneForm>,
    pool: &State<Pool>,
) -> Result<Redirect> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err(Error::new(
            Status::BadRequest,
            anyhow::anyhow!("a room needs a name"),
        ));
    }

    let mut conn = pool.get().await?;
    let clone = room::clone_room(
        &mut conn,
        access.room.id,
        name.to_string(),
        gate.user_id(),
        form.keep_owners,
    )
    .await?;

    tracing::info!(
        source = %id,
        clone = %clone,
        user_id = gate.user_id(),
        role = access.role().as_sql(),
        grant = ?gate.grant(),
        keep_owners = form.keep_owners,
        "room cloned"
    );
    Ok(Redirect::to(format!("/room/{clone}")))
}

// ---- roster ----------------------------------------------------------------

#[derive(Template, WebTemplate)]
#[template(path = "rooms/members.html")]
pub struct MembersTemplate {
    base: TplContext,
    room: Room,
    members: Vec<member::Member>,
    invites: Vec<member::Invite>,
}

#[get("/room/<id>/members")]
async fn members(
    id: RoomParam,
    access: RoomAccess<Organizer>,
    pool: &State<Pool>,
) -> Result<MembersTemplate> {
    let mut conn = pool.get().await?;
    Ok(MembersTemplate {
        base: TplContext::new(access.session.session()),
        members: member::list(&mut conn, id.0).await?,
        invites: member::list_invites(&mut conn, id.0).await?,
        room: access.room,
    })
}

#[derive(FromForm)]
struct AddMemberForm {
    /// A Discord snowflake as text: it exceeds 2^53 and would lose precision as a JSON number.
    user_id: String,
    role: String,
}

#[post("/room/<id>/members", data = "<form>")]
async fn add_member(
    id: RoomParam,
    access: RoomAccess<Organizer>,
    form: Form<AddMemberForm>,
    pool: &State<Pool>,
) -> Result<Redirect> {
    let user_id: i64 = form.user_id.trim().parse().map_err(|_| {
        Error::new(
            Status::BadRequest,
            anyhow::anyhow!("a Discord id is a number"),
        )
    })?;
    let role = RoomRole::parse(&form.role)
        .ok_or_else(|| Error::new(Status::BadRequest, anyhow::anyhow!("unknown role")))?;

    let mut conn = pool.get().await?;
    // Unlike the creator allowlist, membership IS foreign-keyed to `users` -- a member is somebody
    // the room's pages will name -- so a placeholder row is created for an id that has never
    // logged in. Their username fills in on first login.
    user::ensure_exists(&mut conn, user_id).await?;
    member::set_role(&mut conn, id.0, user_id, role, Some(access.user_id()))
        .await
        .map_err(member_error)?;

    tracing::info!(room = %id, by = access.user_id(), user_id, role = role.as_sql(), "member set");
    Ok(Redirect::to(format!("/room/{id}/members")))
}

#[derive(FromForm)]
struct RemoveMemberForm {
    user_id: String,
}

#[post("/room/<id>/members/remove", data = "<form>")]
async fn remove_member(
    id: RoomParam,
    access: RoomAccess<Organizer>,
    form: Form<RemoveMemberForm>,
    pool: &State<Pool>,
) -> Result<Redirect> {
    let user_id: i64 = form.user_id.trim().parse().map_err(|_| {
        Error::new(
            Status::BadRequest,
            anyhow::anyhow!("a Discord id is a number"),
        )
    })?;

    let mut conn = pool.get().await?;
    member::remove(&mut conn, id.0, user_id)
        .await
        .map_err(member_error)?;

    tracing::info!(room = %id, by = access.user_id(), user_id, "member removed");
    Ok(Redirect::to(format!("/room/{id}/members")))
}

/// The last-organizer rule is something a user hits, not a fault: a 409 with the reason, rather
/// than the 500 a raw database error would produce.
fn member_error(e: MemberError) -> Error {
    match e {
        MemberError::LastOrganizer => Error::new(Status::Conflict, anyhow::anyhow!(e)),
        MemberError::InviteSpent | MemberError::NoSuchInvite => {
            Error::new(Status::NotFound, anyhow::anyhow!(e))
        }
        MemberError::Db(e) => Error::new(Status::InternalServerError, e.into()),
    }
}

#[derive(FromForm)]
struct InviteForm {
    role: String,
    /// Blank means unlimited, which is the ordinary case for a small trusted group.
    uses: Option<String>,
}

#[post("/room/<id>/invites", data = "<form>")]
async fn create_invite(
    id: RoomParam,
    access: RoomAccess<Organizer>,
    form: Form<InviteForm>,
    pool: &State<Pool>,
) -> Result<Redirect> {
    let role = RoomRole::parse(&form.role)
        .ok_or_else(|| Error::new(Status::BadRequest, anyhow::anyhow!("unknown role")))?;
    let uses = form
        .uses
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<i32>())
        .transpose()
        .map_err(|_| Error::new(Status::BadRequest, anyhow::anyhow!("uses must be a number")))?;

    let mut conn = pool.get().await?;
    member::create_invite(&mut conn, id.0, role, access.user_id(), None, uses).await?;
    Ok(Redirect::to(format!("/room/{id}/members")))
}

#[derive(FromForm)]
struct RevokeInviteForm {
    token: String,
}

#[post("/room/<id>/invites/revoke", data = "<form>")]
async fn revoke_invite(
    id: RoomParam,
    _access: RoomAccess<Organizer>,
    form: Form<RevokeInviteForm>,
    pool: &State<Pool>,
) -> Result<Redirect> {
    let mut conn = pool.get().await?;
    member::revoke_invite(&mut conn, id.0, &form.token).await?;
    Ok(Redirect::to(format!("/room/{id}/members")))
}

/// Follow an invite link. Requires login, then grants the membership and lands on the room.
#[get("/invite/<token>")]
async fn redeem_invite(
    token: &str,
    session: LoggedInSession,
    pool: &State<Pool>,
) -> Result<Redirect> {
    let mut conn = pool.get().await?;
    let (room, role) = member::redeem_invite(&mut conn, token, session.user_id())
        .await
        .map_err(member_error)?;
    tracing::info!(room = %room, user_id = session.user_id(), role = role.as_sql(), "invite redeemed");
    Ok(Redirect::to(format!("/room/{room}")))
}

// ---- slots -----------------------------------------------------------------

/// Follow a claim link. Single-use, and the room it belongs to is derived from the token rather
/// than supplied, so a claim cannot be aimed at a room the token does not name.
#[get("/claim/<token>")]
async fn claim_slot(token: &str, session: LoggedInSession, pool: &State<Pool>) -> Result<Redirect> {
    let mut conn = pool.get().await?;
    let claimed = slot::claim(&mut conn, token, session.user_id())
        .await
        .map_err(|e| match e {
            slot::ClaimError::NoSuchToken => Error::new(Status::NotFound, anyhow::anyhow!(e)),
            slot::ClaimError::Db(e) => Error::new(Status::InternalServerError, e.into()),
        })?;

    tracing::info!(
        room = %claimed.room_id,
        slot = claimed.slot_number,
        user_id = session.user_id(),
        "slot claimed"
    );
    Ok(Redirect::to(format!("/room/{}", claimed.room_id)))
}

/// A slot's password, for whoever `SlotAccess` admitted.
///
/// JSON rather than a page, because it is one string that wants copying. `404` outside `per_slot`
/// mode: there is no password, and saying so is better than an empty field that looks like a bug.
#[get("/room/<_id>/slot/<_n>/password")]
async fn slot_password(
    _id: RoomParam,
    _n: i32,
    access: SlotAccess,
) -> Result<Json<serde_json::Value>> {
    if access.room.slot_auth != SlotAuth::PerSlot {
        return Err(not_found("this room does not use per-slot passwords"));
    }
    let password = access
        .slot
        .password
        .as_deref()
        .ok_or_else(|| not_found("no password on this slot"))?;

    Ok(Json(serde_json::json!({
        "slot": access.slot.slot_number,
        "player_name": access.slot.player_name,
        "password": password,
        "is_owner": access.is_owner(),
    })))
}

// ---- settings --------------------------------------------------------------

#[derive(FromForm)]
struct SlotAuthForm {
    mode: String,
}

/// Change the room's password mode.
///
/// **Every transition is a restart**, because pahoa reads the mode from the environment at startup
/// and its live rotation route cannot create a mode that is not already in force.
///
/// **This route requested no restart until M17, and that was a live security hole.** The plan said
/// a mode change applies immediately, and nothing implemented it: the row changed, the sweep
/// refreshed the Secret within the hour, and the pod was never bounced — so a room switched *to*
/// per-slot passwords went on accepting unauthenticated connections until something else happened
/// to restart it. The person making that change is usually reacting to something, which is exactly
/// when "it will apply eventually" is the wrong answer.
#[post("/room/<id>/settings/slot-auth", data = "<form>")]
async fn set_slot_auth(
    id: RoomParam,
    access: RoomAccess<Organizer>,
    form: Form<SlotAuthForm>,
    pool: &State<Pool>,
) -> Result<Redirect> {
    let mode = SlotAuth::parse(&form.mode)
        .ok_or_else(|| Error::new(Status::BadRequest, anyhow::anyhow!("unknown mode")))?;

    let mut conn = pool.get().await?;
    room::set_slot_auth(&mut conn, id.0, mode).await?;
    // The same signal the admin console's restart button uses. One mechanism, because a room that
    // needs a bounce should not care which half of Puna asked for it.
    puna_core::model::fleet::request_redeploy(&mut conn, &[id.0]).await?;

    tracing::info!(
        room = %id,
        by = access.user_id(),
        mode = mode.as_sql(),
        "slot_auth changed; a restart is queued so it takes effect now rather than eventually"
    );
    Ok(Redirect::to(format!("/room/{id}")))
}

pub fn routes() -> Vec<rocket::Route> {
    routes![
        create,
        my_rooms,
        show,
        status,
        panel,
        start,
        stop,
        close,
        clone_room,
        members,
        add_member,
        remove_member,
        create_invite,
        revoke_invite,
        redeem_invite,
        claim_slot,
        slot_password,
        set_slot_auth,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(number: i32, owner: Option<i64>) -> Slot {
        Slot {
            room_id: puna_core::ids::RoomId::new(),
            slot_number: number,
            player_name: format!("player{number}"),
            game: "A Link to the Past".into(),
            kind: puna_core::artifact::SlotKind::Player,
            password: Some("a-secret".into()),
            owner_id: owner,
            claim_token: Some("a-claim-token".into()),
            claimed_at: None,
            tracker_id: puna_core::ids::TrackerId::new(),
        }
    }

    /// **A slot's tracker id reaches its owner and nobody else, staff included.**
    ///
    /// The id exists so a player can share their own progress without handing over the multiworld's
    /// tracker. That promise is only worth anything if the room page does not hand every slot's link
    /// to whoever is looking -- which is the easy mistake, because staff legitimately see more of
    /// every other column in this table.
    #[test]
    fn a_slot_tracker_id_is_offered_only_to_the_slot_owner() {
        let mine = 100_i64;
        let theirs = 200_i64;
        let slots = vec![slot(1, Some(mine)), slot(2, Some(theirs)), slot(3, None)];

        // The owner, holding no role at all.
        let views = slot_views(slots.clone(), Some(mine), None, &Default::default());
        assert!(views[0].tracker_id.is_some(), "own slot: link expected");
        assert!(
            views[1].tracker_id.is_none(),
            "another player's slot leaked"
        );
        assert!(views[2].tracker_id.is_none(), "unclaimed slot leaked");

        // Staff who own nothing here. They see claim tokens and every patch, and STILL get no
        // per-slot tracker link -- this is the case the field's note is about.
        let views = slot_views(
            slots.clone(),
            Some(999),
            Some(RoomRole::Organizer),
            &Default::default(),
        );
        assert!(
            views.iter().all(|v| v.tracker_id.is_none()),
            "an organizer was handed players' personal tracker links"
        );

        // Anonymous.
        let views = slot_views(slots, None, None, &Default::default());
        assert!(views.iter().all(|v| v.tracker_id.is_none()));
    }

    /// The page and the poller must spell a duration the same way, or the first poll visibly
    /// rewrites what the server just rendered.
    #[test]
    fn durations_read_the_way_room_js_writes_them() {
        assert_eq!(human_duration(0), "0s");
        assert_eq!(human_duration(999), "1s");
        assert_eq!(human_duration(59_400), "59s");
        assert_eq!(human_duration(60_000), "1m 0s");
        assert_eq!(human_duration(95_000), "1m 35s");
    }

    /// A clock skewed forward must not produce "started -3 seconds ago", which reads as a bug in
    /// the page rather than in the clock.
    #[test]
    fn an_elapsed_time_is_never_negative() {
        let future = chrono::Utc::now() + chrono::TimeDelta::hours(1);
        assert_eq!(since_ms(future), 0);
    }

    /// An event nobody has written a sentence for renders as nothing, and the page falls back to
    /// the state. The alternative -- formatting the raw kind -- says `ip_mismatch` to a player.
    #[test]
    fn only_events_with_a_sentence_are_shown() {
        assert!(phrase("starting").is_some());
        assert!(phrase("ip_mismatch").is_some());
        assert_eq!(
            phrase("provisioned").map(str::to_lowercase),
            Some("preparing this room's files".to_string())
        );
        assert_eq!(phrase("some_kind_added_later"), None);

        // Nothing here leaks an internal noun a player would have to look up.
        for kind in ["starting", "deployment_gone", "degraded", "ip_mismatch"] {
            let text = phrase(kind).expect("a sentence");
            assert!(!text.contains('_'), "{kind}: {text}");
            assert!(
                text.chars().next().is_some_and(char::is_lowercase),
                "{kind}"
            );
        }
    }

    fn a_room() -> Room {
        Room {
            id: puna_core::ids::RoomId::new(),
            name: "Friday async".into(),
            environment: puna_core::Environment::Dev,
            generation_id: puna_core::ids::GenerationId::new(),
            source: puna_core::model::RoomSource::Direct,
            created_by: Some(7),
            created_at: chrono::Utc::now(),
            cloned_from: None,
            desired_state: "running".into(),
            slot_auth: puna_core::model::room::SlotAuth::None,
            password: None,
            spoiler_policy: puna_core::model::room::SpoilerPolicy::AdminOnly,
            tracker_id: puna_core::ids::TrackerId::new(),
            tracker_policy: puna_core::model::room::TrackerPolicy::Link,
            wants_filtered: true,
            state: "running".into(),
            state_changed_at: chrono::Utc::now(),
            advertised_host: Some("mw.example".into()),
            advertised_port: Some(40000),
            advertised_filtered_port: Some(40001),
            last_error: None,
        }
    }

    fn page(is_staff: bool) -> RoomTemplate {
        RoomTemplate {
            base: crate::tpl::TplContext {
                is_logged_in: true,
                is_admin: false,
                username: "troy".into(),
                site_name: "puna",
                version: "test",
                static_version: "test",
            },
            room: a_room(),
            slots: Vec::new(),
            is_staff,
            is_organizer: is_staff,
            siblings: Vec::new(),
            can_see_spoiler: false,
            can_see_tracker: true,
            message: None,
            elapsed: "1m".into(),
            is_closed: false,
            may_start: true,
            is_working: false,
        }
    }

    /// **A route with no link is a route nobody uses, and this has now happened twice.** The
    /// tracker link was built and never rendered; the console was built, deployed, and never
    /// rendered. Both were found by somebody going looking for a feature that already existed.
    #[test]
    fn staff_see_a_link_to_everything_staff_can_do() {
        let html = page(true).render().expect("renders");

        for (path, what) in [
            ("/console", "the console"),
            ("/members", "members and invites"),
            ("/clone", "create a room from this seed"),
        ] {
            assert!(
                html.contains(path),
                "the room page offers no way to reach {what}"
            );
        }
    }

    /// And the links follow the guard rather than the other way round: somebody who is not staff is
    /// offered none of them.
    #[test]
    fn a_visitor_is_offered_no_management_links() {
        let html = page(false).render().expect("renders");

        assert!(
            !html.contains("/console"),
            "a non-member was offered the console"
        );
        assert!(!html.contains("/members"));
        assert!(!html.contains("/clone"));
    }

    /// A closed room, seen by somebody holding the link.
    ///
    /// **The page still works** — that is the whole shape of the state. Patches, tracker and roster
    /// are unchanged; what is gone is the door. Rendering the room as though it were merely idle
    /// would offer a button the route now refuses, which teaches people the site is broken.
    #[test]
    fn a_closed_room_renders_for_a_visitor_but_offers_no_way_in() {
        let mut closed = page(false);
        closed.room.state = "idle".into();
        closed.room.desired_state = "closed".into();
        closed.is_closed = true;
        closed.may_start = false;

        let html = closed.render().expect("renders");

        assert!(html.contains("closed"), "the state is not named");
        assert!(
            !html.contains("/start"),
            "a visitor was offered a start control the route would refuse"
        );
        // The page is not a dead end: the tracker is exactly what a player wants from a room that
        // is down, and closing must not take it away.
        assert!(html.contains("/tracker/"), "the tracker link was hidden");
    }

    /// The same room, seen by an organizer: the one door, labelled for what it does.
    #[test]
    fn an_organizer_can_reopen_a_closed_room() {
        let mut closed = page(true);
        closed.room.state = "idle".into();
        closed.room.desired_state = "closed".into();
        closed.is_closed = true;
        closed.may_start = true;

        let html = closed.render().expect("renders");
        assert!(
            html.contains("/start"),
            "staff were offered no way to reopen"
        );
        assert!(
            html.contains("Reopen"),
            "reopening a closed room reads as reopening, not as starting"
        );
        // Closing again is not offered, because it is already closed.
        assert!(
            !html.contains("/close"),
            "a closed room offered Close again"
        );
    }

    fn a_panel() -> PanelTemplate {
        PanelTemplate {
            room: a_room(),
            is_closed: false,
            may_start: true,
            is_working: false,
            message: None,
            elapsed: "12s".into(),
        }
    }

    /// **The poller's contract with the template, which nothing else checks.**
    ///
    /// `room.js` decides whether anything moved by comparing `panel.dataset.state` and
    /// `panel.dataset.desired` against the status JSON. If the wrapper stopped emitting either,
    /// every comparison would be `undefined !== undefined` — false, forever — and the page would
    /// poll happily and never update. Nothing would error and nothing would look wrong.
    ///
    /// Asserted from the script's own text so the two cannot drift: a renamed attribute has to be
    /// renamed in both places or this fails.
    #[test]
    fn the_panel_carries_every_attribute_the_poller_reads() {
        let script = include_str!("../../static/room.js");
        let html = a_panel().render().expect("renders");

        let mut found = 0;
        for reference in script.match_indices("panel.dataset.") {
            let key: String = script[reference.0 + "panel.dataset.".len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            assert!(
                html.contains(&format!("data-{key}=")),
                "room.js reads panel.dataset.{key}, which the panel does not render"
            );
            found += 1;
        }
        assert!(
            found >= 3,
            "the lint found {found} references, so it proves little"
        );
    }

    /// The running panel is a table of both spellings of the room, each copyable.
    ///
    /// The `data-copy` value is what actually lands in somebody's clipboard and then in a game
    /// client, so it is asserted to be the whole `host:port` — a copy control that takes half the
    /// address is worse than none, because it looks like it worked.
    #[test]
    fn a_running_panel_offers_both_ports_and_copies_them_whole() {
        let html = a_panel().render().expect("renders");

        for port in [40000, 40001] {
            let address = format!("mw.example:{port}");
            assert!(
                html.contains(&format!("<code>{address}</code>")),
                "{address} is not shown"
            );
            assert!(
                html.contains(&format!("data-copy=\"{address}\"")),
                "{address} has no copy control, or copies something other than the whole address"
            );
        }

        // The filtered port is described rather than left as a second address with no explanation:
        // it is the same room, and somebody has to be able to tell which one to take.
        assert!(html.contains("standard room"));
        assert!(html.contains("feed-filtered room"));

        // The label is what a screen reader announces, and suppression eats the space before an
        // expression even inside an attribute -- where nothing on screen would reveal it.
        assert!(
            html.contains("aria-label=\"Copy mw.example:40000\""),
            "the copy control's label is missing or ran together"
        );
    }

    /// A room with no filtered port renders one row, not an empty second one.
    #[test]
    fn a_room_without_a_filtered_port_shows_one_address() {
        let mut single = a_panel();
        single.room.advertised_filtered_port = None;

        let html = single.render().expect("renders");
        assert!(html.contains("mw.example:40000"));
        assert!(!html.contains("feed-filtered"));
    }

    /// A transient panel carries the two things the poller updates in place between swaps, plus the
    /// spinner. Losing either hook is silent: the panel would freeze at the elapsed time it was
    /// rendered with while continuing to poll.
    #[test]
    fn a_transient_panel_has_the_hooks_the_poller_writes_into() {
        let mut starting = a_panel();
        starting.room.state = "starting".into();
        starting.is_working = true;
        starting.message = Some("shutting the room down");

        let html = starting.render().expect("renders");
        assert!(html.contains("data-room-message"), "no message hook");
        assert!(html.contains("data-room-elapsed"), "no elapsed hook");
        assert!(html.contains("swirl"), "no spinner");
        // The words are the message; the spinner is beside them. A panel that had only the
        // animation could not tell "coming up" from "stuck".
        assert!(html.contains("shutting the room down"));
        assert!(html.contains("12s"));

        // And the message is escaped on the way in. `phrase()` returns a fixed table today, but
        // this element is the one the poller also writes into from JSON -- so it is worth pinning
        // that the server side escapes rather than relying on the table staying literal.
        //
        // Asserted as absence rather than against a particular entity spelling: which of `&#39;`
        // and `&#x27;` askama emits is its business, and pinning it would make this test fail on an
        // upgrade that changed nothing that matters.
        starting.message = Some("the room's server <b>");
        let escaped = starting.render().expect("renders");
        assert!(
            !escaped.contains("room's"),
            "an apostrophe reached the markup raw"
        );
        assert!(!escaped.contains("<b>"), "a tag reached the markup raw");
        // And the no-JS path still advances.
        assert!(html.contains("http-equiv=\"refresh\""));
    }

    /// A settled panel does NOT carry the meta refresh.
    ///
    /// It used to be unconditional in `show.html`'s transient branch, which was correct there and
    /// would not be here: a resting room has nothing to refresh toward, and a page reloading itself
    /// every five seconds forever is one that never stops asking.
    #[test]
    fn a_resting_panel_does_not_refresh_itself() {
        let html = a_panel().render().expect("renders");
        assert!(!html.contains("http-equiv=\"refresh\""));
        assert!(
            !html.contains("swirl"),
            "a resting room is not working on anything"
        );
    }

    /// **The bug this exists to prevent, stated as a table.**
    ///
    /// A request writes `desired_state` and returns; the observed state does not move until the
    /// orchestrator reaches the room, which can be a full reconcile interval. A panel rendered from
    /// `state` alone shows the room exactly as it was — so clicking Stop on a running room hands
    /// back the address table, and it changes on its own some seconds later. Reported from the live
    /// deployment as "it flickers and returns to the existing display".
    ///
    /// Not a scripting fault: without JavaScript the same click redirects to a page saying "This
    /// room is not running" with a Start button, having just been asked to start it.
    #[test]
    fn a_requested_transition_reads_as_working_before_the_orchestrator_moves() {
        // (observed, desired, working?)
        let cases = [
            // Asked, and not yet acted on. These are the ones that were wrong.
            ("running", "stopped", true),
            ("running", "closed", true),
            ("idle", "running", true),
            // Being acted on.
            ("starting", "running", true),
            ("stopping", "stopped", true),
            ("provisioning", "running", true),
            ("deleting", "deleted", true),
            // Not answering is not settled either.
            ("degraded", "running", true),
            // At rest, agreeing with what is wanted.
            ("running", "running", false),
            ("idle", "stopped", false),
            // **An idle room asked to close is already where it is going.** Showing it as in-flight
            // would be waiting for an event that never comes.
            ("idle", "closed", false),
            // `failed` is at rest with an error and a retry time, though its desired state is
            // usually still `running`. A spinner would hide the reason it failed for up to the
            // ten-minute backoff cap.
            ("failed", "running", false),
            ("integrity_fault", "running", false),
        ];

        for (state, desired, expected) in cases {
            let mut room = a_room();
            room.state = state.into();
            room.desired_state = desired.into();
            assert_eq!(
                is_working(&room),
                expected,
                "state={state} desired={desired}"
            );
        }
    }

    /// And the panel actually renders that, rather than the display that was just clicked away.
    #[test]
    fn stopping_a_running_room_replaces_the_address_immediately() {
        let mut stopping = a_panel();
        stopping.room.desired_state = "stopped".into();
        stopping.is_working = is_working(&stopping.room);
        assert!(stopping.is_working, "the fixture does not reach the case");

        let html = stopping.render().expect("renders");
        assert!(
            !html.contains("mw.example:40000"),
            "the address survived a stop request, which is the flicker being fixed"
        );
        assert!(html.contains("swirl"), "no transition is shown");
        // And the poller keeps watching, which it decides from this attribute alone.
        assert!(html.contains("data-working=\"1\""));
    }

    /// A settled panel says so, or the poller never stops.
    #[test]
    fn a_settled_panel_tells_the_poller_to_stop() {
        let html = a_panel().render().expect("renders");
        assert!(html.contains("data-working=\"0\""));
    }

    /// `may_start` is the single decision the page and the route share.
    ///
    /// Asserted directly rather than only through markup, because the route calls it with a role it
    /// resolves itself — a page that agreed with a route by coincidence would drift the first time
    /// either changed.
    #[test]
    fn only_staff_may_start_a_closed_room() {
        let open = a_room();
        for role in [None, Some(RoomRole::Helper), Some(RoomRole::Organizer)] {
            assert!(
                may_start(&open, role),
                "anyone holding the link may start an ordinary room ({role:?})"
            );
        }

        let mut closed = a_room();
        closed.desired_state = "closed".into();
        assert!(!may_start(&closed, None), "a visitor must be refused");
        assert!(
            !may_start(&closed, Some(RoomRole::Helper)),
            "a helper is not an organizer -- closing is an organizer's decision to undo"
        );
        assert!(may_start(&closed, Some(RoomRole::Organizer)));
    }
}
