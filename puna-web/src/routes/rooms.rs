//! Rooms: the public page, the lifecycle buttons, the roster, invites and claims.
//!
//! Every room-scoped route spells the room id as its **first** dynamic segment, because that is
//! where [`RoomAccess`](crate::guards::RoomAccess) and [`SlotAccess`](crate::guards::SlotAccess)
//! look for it. See the note at the top of `guards.rs`.
//!
//! `GET /room/<id>` is deliberately **public**: players arrive from a shared link, and the
//! unguessable id is the authorization, exactly as the reference implementation does it. What that
//! page shows varies by who is looking -- credentials only ever through `SlotAccess`.

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
use crate::guards::{Helper, Organizer, RoomAccess, SlotAccess};
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
async fn show(id: RoomParam, session: Session, pool: &State<Pool>) -> Result<RoomTemplate> {
    let mut conn = pool.get().await?;
    let room = room::get(&mut conn, id.0)
        .await?
        .ok_or_else(|| not_found("no such room"))?;

    let role = if session.is_admin {
        Some(RoomRole::Organizer)
    } else if let Some(user_id) = session.user_id {
        member::role_of(&mut conn, room.id, user_id).await?
    } else {
        None
    };

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

    let slots = slot_views(room_slots, session.user_id, role, &patched);
    let siblings = room::siblings(&mut conn, room.id, room.generation_id).await?;

    Ok(RoomTemplate {
        base: TplContext::new(&session),
        room,
        slots,
        is_staff: role.is_some(),
        is_organizer: role.is_some_and(|r| r >= RoomRole::Organizer),
        siblings,
        can_see_spoiler,
    })
}

/// The poll target behind the starting spinner. One row read, no template.
///
/// `since_ms` is a server-computed duration rather than a timestamp, so a client whose clock is
/// wrong still renders a sensible elapsed time.
#[get("/room/<id>/status")]
async fn status(id: RoomParam, pool: &State<Pool>) -> Result<Json<serde_json::Value>> {
    let mut conn = pool.get().await?;
    let room = room::get(&mut conn, id.0)
        .await?
        .ok_or_else(|| not_found("no such room"))?;

    Ok(Json(serde_json::json!({
        "state": room.state,
        "desired_state": room.desired_state,
        "host": room.advertised_host,
        "port": room.advertised_port,
        "filtered_port": room.advertised_filtered_port,
        "last_error": room.last_error,
    })))
}

/// Start an idle room.
///
/// Guarded only by holding the room URL, which is the same capability that reaches the page: a
/// player whose async has idled out must be able to bring it back without being on the roster.
/// D8's link-unfurl hazard is about the *implicit* trigger on `GET`; this is the explicit button.
#[post("/room/<id>/start")]
async fn start(id: RoomParam, session: Session, pool: &State<Pool>) -> Result<Redirect> {
    let mut conn = pool.get().await?;
    let changed = room::request_state(&mut conn, id.0, DesiredState::Running).await?;
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
    room::request_state(&mut conn, id.0, DesiredState::Stopped).await?;
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
/// and its live rotation route cannot create a mode that is not already in force. This writes the
/// new state and asks for the room to come back; M7 is what makes the Secret and the bounce
/// actually happen.
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

    tracing::info!(
        room = %id,
        by = access.user_id(),
        mode = mode.as_sql(),
        "slot_auth changed; the room must restart for it to take effect"
    );
    Ok(Redirect::to(format!("/room/{id}")))
}

pub fn routes() -> Vec<rocket::Route> {
    routes![
        create,
        my_rooms,
        show,
        status,
        start,
        stop,
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
