//! Rooms: the public page, the lifecycle buttons, the roster, invites and claims.
//!
//! Every room-scoped route spells the room id as its **first** dynamic segment, because that is
//! where [`RoomAccess`](crate::guards::RoomAccess) and [`SlotAccess`](crate::guards::SlotAccess)
//! look for it. See the note at the top of `guards.rs`.
//!
//! `GET /room/<id>` is deliberately **public**: players arrive from a shared link, and the
//! unguessable id is the authorization, exactly as the reference implementation does it. What that
//! page shows varies by who is looking -- credentials only ever through `SlotAccess`.

use puna_core::artifact;
use puna_core::model::command::{self, RoomCommand};
use puna_core::model::event;
use puna_core::model::generation;
use puna_core::model::member::{self, MemberError, RoomRole};
use puna_core::model::room::{self, DesiredState, MyRoom, Room, RoomState, SlotAuth};
use puna_core::model::slot::{self, Slot};
use puna_core::model::user;
use rocket::form::Form;
use rocket::http::Status;
use rocket::response::{Flash, Redirect};
use rocket::serde::json::Json;
use rocket::{FromForm, State, get, post, routes};

use askama::Template;
use askama_web::WebTemplate;

use crate::DataDir;
use crate::auth::{LoggedInSession, Session};
use crate::error::{Error, Result, not_found};
use crate::gate::{CanCreateRoom, Direct};
use crate::guards::{Helper, Navigation, Organizer, RoomAccess, SlotAccess};
use crate::params::RoomParam;
use crate::tpl::TplContext;

type Pool = puna_core::db::Pool;

/// Refuse to open a room from a seed a room would not load.
///
/// The same check the upload form runs, run again here, and it is not redundant: those checks live
/// in `pahoa-multidata` at a pinned rev and **they change** -- so every generation on the volume
/// was last checked under whatever the rules were the day it arrived. Bumping the pin is what makes
/// this the only place the answer is current, and a room opened from a stale pass is a pod that
/// exits at startup with the reason in a container log.
///
/// Costs one read and one parse per room CREATION, which is a form POST by a person: ~140 ms on the
/// largest seed in the corpus, against a room that then takes the better part of a minute to come
/// up. It is deliberately not on the start path, where it would be paid over and over for an answer
/// that cannot have changed.
async fn refuse_unloadable_seed(
    conn: &mut diesel_async::AsyncPgConnection,
    data_dir: &std::path::Path,
    generation_id: puna_core::ids::GenerationId,
) -> Result<()> {
    let Some(generation) = generation::get(conn, generation_id).await? else {
        return Err(not_found("no such generation"));
    };
    let sha256: [u8; 32] = generation.sha256.clone().try_into().map_err(|_| {
        Error::new(
            Status::InternalServerError,
            anyhow::anyhow!("this generation's stored hash is not 32 bytes"),
        )
    })?;

    let seed = std::fs::read(artifact::GenerationPaths::new(data_dir, &sha256).seed()).map_err(
        |e| {
            // The provisioning step copies this same file, so a room created here would park in
            // `provisioning` instead. Saying so now beats a room that never comes up.
            tracing::error!(generation = %generation_id, error = %e, "the promoted seed is unreadable");
            Error::new(
                Status::InternalServerError,
                anyhow::anyhow!(
                    "this generation's seed could not be read; this is a server-side fault \
                     and has been logged"
                ),
            )
        },
    )?;

    match artifact::seed_refusal(&seed) {
        Ok(None) => Ok(()),
        Ok(Some(reason)) => {
            tracing::warn!(
                generation = %generation_id,
                %reason,
                "refused to open a room from a seed a room would not load"
            );
            Err(Error::new(
                Status::BadRequest,
                anyhow::anyhow!(
                    "this generation's seed will not load: {reason}. \
                     A room opened from it would exit at startup instead of serving."
                ),
            ))
        }
        Err(e) => {
            tracing::error!(generation = %generation_id, error = %e, "the promoted seed no longer parses");
            Err(Error::new(
                Status::InternalServerError,
                anyhow::anyhow!("this generation's seed could not be read: {e}"),
            ))
        }
    }
}

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
    /// The room's own filter as a hover summary, or `None` when there is none — or when the viewer
    /// is not staff, which is decided here rather than in markup for the reason `SlotView`'s note
    /// gives: a template cannot prove it did not render something.
    ///
    /// **A room-wide filter is otherwise invisible from every page a player or a helper looks at**,
    /// which turns "why did my DeathLinks stop" into a mystery with no thread to pull. The chip is
    /// the thread; the hover is the answer.
    room_filter: Option<String>,
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
    /// Whether this viewer holds a slot here, which decides two things: the roster's usernames are
    /// visible to them, and the "only my slots" toggle is worth offering. Somebody holding none
    /// would get a control that hides every row.
    owns_a_slot: bool,
    /// The room-wide password. Same field, same gate and same reason as [`PanelTemplate`]'s — this
    /// page `{% include %}`s that template, so both structs must offer the name it renders.
    room_password: Option<String>,
    /// Same field and same reason as [`PanelTemplate`]'s. `is_organizer` is already above.
    needs_password: bool,
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
    /// This slot's password, and **only when the viewer may see it** -- its owner, the room's
    /// staff, or an admin, which is the same rule `SlotAccess` applies to the JSON route.
    ///
    /// The struct's note above exists for exactly this field: a template cannot prove it did not
    /// render something, so the decision is made in `slot_views` and the markup only asks whether
    /// there is a value. `None` outside per-slot mode, where a per-slot password has no meaning.
    pub password: Option<String>,
    /// Whether this viewer is offered a control to unbind the slot from its owner.
    pub can_release: bool,
    /// Who holds this slot, by Discord username, and **only for a viewer entitled to the roster**:
    /// the room's staff, or somebody who holds a slot in it themselves.
    ///
    /// `Some("")` never happens; a person with a row but no login yet is `Some(placeholder)` and the
    /// template says "never logged in" rather than showing a Discord ID, which is what the stored
    /// stand-in actually contains. See `user::is_placeholder`.
    ///
    /// The gate is the same shape as `may_see_spoiler`'s `players` tier, and it exists because
    /// `GET /room/<id>` is public: without it, holding a room link would list everybody playing.
    pub owner_name: Option<String>,
    /// True when the holder exists but has never signed in — the lobby-push case, where a slot is
    /// assigned to a Discord id that has no account here yet.
    pub owner_never_logged_in: bool,
    /// Whether this slot is barred from connecting, which decides which way the lock control points.
    ///
    /// Shown to staff only, with the rest of the moderation column: it is a fact about a sanction
    /// rather than about the game, and `GET /room/<id>` is public.
    pub is_locked: bool,
    /// **Divergence from the room's filter, not "is filtered".** With a room filter in force every
    /// slot is filtered, so a chip meaning that would land on every row and distinguish nothing.
    /// `filtered` is a slot with rules of its own; `unfiltered` is one deliberately exempt from
    /// rules everybody else has — opposite facts, so one word for both would be worse than none.
    /// Staff only, for the same reason `is_locked` is: it means nothing to a player and this page
    /// is public.
    pub filter_chip: Option<&'static str>,
    /// What is actually in effect for this slot, as a hover. **The rules in words, not the
    /// probabilities**: `p` is the fraction dropped and the opposite reading is equally natural, so
    /// a tooltip printing numbers would be a tooltip that misleads at a glance.
    pub filter_summary: String,
}

/// What the roster needs to know about filtering: the room's state, and the slots that diverge.
///
/// **Both, because neither answers the question alone.** A slot with rules of its own is remarkable
/// for a different reason depending on whether the room filters — with a room filter it is *not
/// running the room's*, and without one it is simply the only filtered slot. So this is one
/// parameter rather than a ninth, which is the context struct the note below wants, arriving one
/// field at a time.
#[derive(Default)]
pub struct Filters {
    pub room_filters: bool,
    /// Only the divergent slots have entries; a slot that follows the room is absent.
    pub slots: std::collections::HashMap<i32, puna_core::model::filter::SlotFilter>,
}

impl Filters {
    fn of(&self, slot: i32) -> Option<&puna_core::model::filter::SlotFilter> {
        self.slots.get(&slot)
    }
}

// Eight, and the honest fix is a context struct rather than this attribute -- deferred rather than
// dismissed, because it is nine call sites of churn for no behavior change. Worth doing next time
// this signature grows: the arguments most at risk of being transposed are the two `bool`s, and a
// struct is what makes that unspellable.
#[expect(
    clippy::too_many_arguments,
    reason = "a context struct is the right fix and is deliberately deferred; see the note above"
)]
fn slot_views(
    slots: Vec<Slot>,
    viewer: Option<i64>,
    role: Option<RoomRole>,
    patched: &std::collections::HashSet<i32>,
    per_slot_passwords: bool,
    owner_names: &std::collections::HashMap<i64, String>,
    may_see_roster: bool,
    filters: &Filters,
) -> Vec<SlotView> {
    slots
        .into_iter()
        .map(|s| SlotView {
            filter_summary: match filters.of(s.slot_number) {
                Some(puna_core::model::filter::SlotFilter::Exempt) if filters.room_filters => {
                    "Exempt: nothing is filtered for this slot, including the room's filter".into()
                }
                Some(puna_core::model::filter::SlotFilter::Exempt) => {
                    "Exempt: nothing is filtered for this slot".into()
                }
                Some(puna_core::model::filter::SlotFilter::Own(rules)) => {
                    // Its OWN rules, and the room's are deliberately absent -- which is the fact
                    // this hover exists to make visible without opening the editor. Said only when
                    // there IS a room filter to be replacing; otherwise it names a thing that does
                    // not exist and reads as though something were being lost.
                    let mut summary = String::from(if filters.room_filters {
                        "Its own rules, instead of the room's: "
                    } else {
                        "Its own rules: "
                    });
                    summary.push_str(
                        &rules
                            .iter()
                            .map(|r| r.describe(puna_core::model::filter::Subject::ThisSlot))
                            .collect::<Vec<_>>()
                            .join("; "),
                    );
                    summary
                }
                _ => String::new(),
            },
            filter_chip: match (role.is_some(), filters.of(s.slot_number)) {
                (true, Some(puna_core::model::filter::SlotFilter::Exempt)) => Some("unfiltered"),
                // **The override, and it only exists when there is something to override.** With a
                // room filter in force, a slot with rules of its own is not running the room's --
                // pahoa replaces rather than merges -- and "filtered" would say the opposite of the
                // thing worth knowing, since every other slot is filtered too. With no room filter,
                // there is nothing to diverge from and the plain word is the honest one.
                (true, Some(puna_core::model::filter::SlotFilter::Own(_)))
                    if filters.room_filters =>
                {
                    Some("overrides room filter")
                }
                (true, Some(_)) => Some("filtered"),
                // A slot that follows the room gets no chip: whatever the room does is not a fact
                // about this row, and the room's own filter is stated once above the table.
                _ => None,
            },
            owner_name: match (may_see_roster, s.owner_id) {
                (true, Some(owner)) => owner_names.get(&owner).cloned(),
                _ => None,
            },
            owner_never_logged_in: match (may_see_roster, s.owner_id) {
                (true, Some(owner)) => owner_names
                    .get(&owner)
                    .is_some_and(|name| puna_core::model::user::is_placeholder(name)),
                _ => false,
            },
            is_mine: matches!((viewer, s.owner_id), (Some(v), Some(o)) if v == o),
            // Staff only, and gated here rather than in markup for the same reason the password is:
            // this page is public, and whether a player has been shut out is nobody else's business.
            is_locked: role.is_some_and(|r| r >= RoomRole::Helper) && s.is_locked(),
            has_patch: patched.contains(&s.slot_number),
            // **The one field this struct's own note warns about**, so it is gated here and the
            // gate is the same three-way rule `SlotAccess` applies to the JSON route: the slot's
            // owner, the room's staff, or an admin. `GET /room/<id>` is a PUBLIC page, so anything
            // else would put every player's password in front of anyone holding the room's URL.
            //
            // Also `None` outside per-slot mode, where the column has no meaning: the other two
            // modes either have no password or have one shared room password, which is not a
            // property of a slot and is not rendered here at all.
            password: match (per_slot_passwords, role.is_some(), viewer, s.owner_id) {
                (false, ..) => None,
                (true, true, ..) => s.password.clone(),
                (true, _, Some(v), Some(o)) if v == o => s.password.clone(),
                _ => None,
            },
            // Staff, helpers included: unbinding a slot and handing out a fresh claim link is
            // running the room rather than deciding who runs it. The roster action a helper may
            // NOT take is on `room_members` -- adding staff, or promoting themselves.
            can_release: role.is_some_and(|r| r >= RoomRole::Helper) && s.owner_id.is_some(),
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

/// Unbind a slot from whoever holds it, and mint a fresh claim link.
///
/// The roster half of the same job `claim` does, and **helper-guarded**: a player dropping out
/// mid-async is the ordinary case this exists for, and making a helper fetch an organizer to hand
/// the slot to somebody else is the bottleneck the tier exists to remove. The roster a helper may
/// not touch is `room_members` — who is staff — which is a different table and a different route.
///
/// **A new claim token, not merely a cleared owner.** The old link was single-use and is already
/// spent, so leaving the slot unclaimed with no token would produce a slot nobody can take — the
/// organizer would have to release it and then find some other way to hand it out. `slot::release`
/// mints one in the same statement, which is why this route hands the page back rather than the
/// token: the roster is where staff read claim links, and it now has one.
///
/// It does **not** touch the room. A released slot's password is unchanged and its connection, if
/// somebody is playing on it right now, is not dropped — releasing is a statement about who owns a
/// slot on the roster, not a kick. Removing them from the running room is `kick` in the console,
/// which is a separate decision and says so.
#[post("/room/<id>/slot/<n>/release")]
async fn release_slot(
    id: RoomParam,
    n: i32,
    access: RoomAccess<Helper>,
    pool: &State<Pool>,
) -> Result<Redirect> {
    let mut conn = pool.get().await?;
    let previous = slot::get(&mut conn, id.0, n)
        .await?
        .ok_or_else(|| not_found("no such slot"))?;
    slot::release(&mut conn, id.0, n).await?;

    event::record(
        &mut conn,
        id.0,
        event::Actor::User(access.user_id()),
        "slot_released",
        // The slot and the person who held it, because "who was playing slot 3 before" is exactly
        // the question somebody asks later. Never the new token: an audit trail is not a place to
        // put a credential.
        serde_json::json!({ "slot": n, "previous_owner": previous.owner_id }),
    )
    .await?;

    tracing::info!(
        room = %id,
        slot = n,
        previous_owner = ?previous.owner_id,
        user_id = access.user_id(),
        "slot released"
    );
    Ok(Redirect::to(format!("/room/{id}")))
}

/// Give a slot a new password, now.
///
/// **Three writes, and the order is the point.** The value lands in `room_slots` first so the page
/// can show it on the next load; the Secret is marked stale so the hourly sweep is a backstop even
/// if everything after this fails; and a command is queued so the orchestrator makes it durable and
/// then live.
///
/// **The web tier cannot do the last part itself, and that is structural rather than an oversight.**
/// It has no egress to room pods at all — its NetworkPolicy says so and calls that the point — so
/// the only process that can reach a running room is the orchestrator. §6 says rotation is a direct
/// call rather than a command; it was written before that boundary was drawn. See
/// [`RoomCommand::RotatePassword`](puna_core::model::command::RoomCommand::RotatePassword).
///
/// A stopped room needs no command at all, and queueing one would be a rejection an organizer has
/// to read past: §4 is explicit that for a room that is not running, writing the Secret alone is
/// sufficient and correct.
///
/// **The database half is shared with the console**, which can build the same command — one
/// implementation of "write the row, then mark the Secret stale", because that ordering is the
/// whole correctness argument and two copies of it is how one of them loses a step.
#[post("/room/<id>/slot/<n>/rotate-password")]
async fn rotate_slot_password(
    id: RoomParam,
    n: i32,
    access: RoomAccess<Helper>,
    pool: &State<Pool>,
) -> Result<Redirect> {
    let mut conn = pool.get().await?;
    let command = RoomCommand::RotatePassword { slot: n };
    let prepared =
        crate::routes::console::prepare_slot_credential(&mut conn, &access.room, &command).await?;

    // Only for a room that could be told. The command would otherwise land `rejected` with "this
    // room is not running", which is true and is not something the organizer needs to act on.
    if matches!(prepared, crate::routes::console::Prepared::Live(_)) {
        command::enqueue(&mut conn, id.0, access.user_id(), access.role(), &command).await?;
    }

    if let Some(kind) = prepared.kind() {
        event::record(
            &mut conn,
            id.0,
            event::Actor::User(access.user_id()),
            kind,
            // The slot, never the value. This row is read by anyone who can read the room's history.
            serde_json::json!({ "slot": n }),
        )
        .await?;
    }

    tracing::info!(
        room = %id,
        slot = n,
        by = access.user_id(),
        live = access.room.state == "running",
        "slot password rotated"
    );
    Ok(Redirect::to(format!("/room/{id}")))
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
    data_dir: &State<DataDir>,
    environment: &State<puna_core::Environment>,
) -> Result<Redirect> {
    let generation_id = form
        .generation_id
        .parse()
        .map_err(|_| Error::new(Status::BadRequest, anyhow::anyhow!("not a generation id")))?;
    let name = room::validate_name(&form.name)
        .map_err(|e| Error::new(Status::BadRequest, anyhow::anyhow!(e)))?;
    let slot_auth = SlotAuth::parse(&form.slot_auth)
        .ok_or_else(|| Error::new(Status::BadRequest, anyhow::anyhow!("unknown password mode")))?;

    let mut conn = pool.get().await?;

    // The generation must exist before a room can reference it, and saying so here beats a
    // foreign-key violation surfacing as a 500 -- and it must be one a room will actually load.
    refuse_unloadable_seed(&mut conn, &data_dir.0, generation_id).await?;

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

    // Who is playing, for anybody entitled to the roster. Same tier as the `players` spoiler
    // policy: the room's staff, or somebody who holds a slot in it -- the people the roster is
    // *for*. `GET /room/<id>` is public, so without a gate a shared link would list everybody.
    let may_see_roster = role.is_some() || owns_a_slot;
    let owner_names = if may_see_roster {
        slot::owner_names(&mut conn, room.id).await?
    } else {
        Default::default()
    };

    // Staff only, and two reads for the whole page: the divergent slots (only those have rows) and
    // the room's own rules. Both feed the chips, and the room's is needed for the SLOT chips too --
    // a slot with rules of its own reads differently depending on whether there is a room filter it
    // is not running.
    let room_rules = if role.is_some() {
        puna_core::model::filter::room_filter(&mut conn, room.id)
            .await?
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let slot_filters = if role.is_some() {
        Filters {
            room_filters: !room_rules.is_empty(),
            slots: puna_core::model::filter::slot_filters(&mut conn, room.id)
                .await?
                .into_iter()
                .collect(),
        }
    } else {
        Filters::default()
    };

    // The chip beside the room's name, with what it drops on hover.
    let room_filter = slot_filters.room_filters.then(|| {
        // `AnySlot`, because a room-wide rule is about everybody — the same sentence the room's own
        // filter page renders, from the same function, so the chip and the page cannot describe one
        // rule two ways.
        let listed = room_rules
            .iter()
            .map(|r| r.describe(puna_core::model::filter::Subject::AnySlot))
            .collect::<Vec<_>>()
            .join("; ");
        // **"This room's filter:", not "This room drops:"** — every sentence `describe()` produces
        // already begins with a verb ("drop 95% of…"), so a prefix ending in one reads "drops:
        // drop". The prefix names what is being listed and lets the rules speak for themselves.
        format!("This room's filter: {listed}")
    });

    let slots = slot_views(
        room_slots,
        session.user_id,
        role,
        &patched,
        room.slot_auth == SlotAuth::PerSlot,
        &owner_names,
        may_see_roster,
        &slot_filters,
    );
    let siblings = room::siblings(&mut conn, room.id, room.generation_id).await?;
    let message = event::latest(&mut conn, room.id)
        .await?
        .and_then(|e| phrase(&e.kind));
    let elapsed = human_duration(since_ms(transition_began(&room)));

    let is_closed = room.desired_state == DesiredState::Closed.as_sql();
    Ok(RoomTemplate {
        base: TplContext::new(&session),
        // Both from `may_start`, so the page and the route cannot disagree about who gets a door.
        is_closed,
        may_start: may_start(&room, role),
        is_working: is_working(&room),
        owns_a_slot,
        room_password: room_password_for(&room, role.is_some(), owns_a_slot),
        needs_password: room.slot_auth == SlotAuth::Room,
        room,
        slots,
        is_staff: role.is_some(),
        is_organizer: role.is_some_and(|r| r >= RoomRole::Organizer),
        room_filter,
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

/// When the thing the panel is currently describing began: **the later of the two clocks.**
///
/// The counter times *the current state*, and the two columns are each right about half of what
/// that means, which is why this is a `max` rather than a choice between them.
///
/// `state_changed_at` alone was the original bug: it times the state the room is **leaving**, so
/// clicking Stop on a room that had been up all afternoon started the transition counter at all
/// afternoon. `desired_at` alone was the fix and was wrong in the other direction — **it does not
/// move on a redeploy**, because a redeploy never changes `desired_state`, so it still points at
/// whenever the room was first asked to run. Navigating to the page after one showed a counter
/// carried over from hours ago, and it did not reset as the phases advanced:
///
/// ```text
/// starting the room                        26s
/// waiting for the room's server to come up  27s   <- should have been 0
/// ```
///
/// Taking the later of the two gives each phase its own clock while keeping the property the
/// earlier fix existed for: a fresh request is *itself* the start of something, so a room asked to
/// stop reads zero even though it has been `running` for thirty-five minutes, and the count then
/// restarts as the orchestrator moves it through `stopping`.
///
/// **Monotonic across phases was the wrong goal**, and the argument for it — that a reset "reads as
/// a stall" — did not survive contact with the page: a number that keeps climbing through a
/// sentence change cannot say how long *this* step has taken, which is the question somebody
/// watching a cold start is actually asking.
///
/// `degraded` needs no special case under this rule, where it did under the last one: nobody asked
/// for it, so `desired_at` is old, and the `max` picks the state change — which is when it started
/// failing.
fn transition_began(room: &Room) -> chrono::DateTime<chrono::Utc> {
    if is_working(room) {
        room.state_changed_at.max(room.desired_at)
    } else {
        room.state_changed_at
    }
}

/// This session's role in a room, with a global admin resolving to the top of the ladder.
///
/// Factored out because `show` and `start` must answer it identically — the page decides whether to
/// render a control from this, and the route decides whether to honor one.
pub(crate) async fn resolve_role(
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
    /// The room-wide password, for the people entitled to it — `None` for everybody else, and for
    /// every room not in that mode.
    ///
    /// **Decided here and never in the template**, the same rule `SlotView` follows and for a
    /// sharper reason: `room` on this struct is the whole [`Room`], which *carries*
    /// `password`, so the raw value is in the context of a page rendered for anonymous visitors. A
    /// template cannot prove it did not render something, so the gate has to be a different field
    /// rather than a condition around the same one. A source lint holds the template to it.
    ///
    /// Until this existed the value was in the context and reachable and **nothing rendered it at
    /// all**, which is the bug this closes: `set_slot_auth` generates the password, ships it to the
    /// room as `PAHOA_PASSWORD` and restarts the room to enforce it, and no page, route or admin
    /// screen ever showed it to anyone. Choosing that mode made the room unjoinable by everybody,
    /// the organizer who chose it included.
    room_password: Option<String>,
    /// Whether this room asks for a shared password at all, which everybody may know even where the
    /// value is withheld — a refused connection with no explanation reads as a broken room.
    needs_password: bool,
    /// Whether this viewer may rotate it. The panel is rendered for anonymous visitors, so this is
    /// decided in the route like every other control's tier.
    is_organizer: bool,
}

/// The room-wide password, for a viewer who may have it.
///
/// **Participants and staff** — Troy's call, and the same tier the roster's usernames and the
/// `players` spoiler policy already use: the room's staff, or somebody who holds a slot in it.
/// `GET /room/<id>` is public, so rendering it to everyone would make the password exactly as
/// secret as the link and the mode meaningless.
///
/// `None` outside [`SlotAuth::Room`], so the per-slot and passwordless modes cannot leak a stale
/// value left in the column by a mode change.
fn room_password_for(room: &Room, is_staff: bool, owns_a_slot: bool) -> Option<String> {
    if room.slot_auth != SlotAuth::Room || !(is_staff || owns_a_slot) {
        return None;
    }
    room.password.clone()
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
    let elapsed = human_duration(since_ms(transition_began(&room)));

    // One indexed lookup, and only for a room that has a shared password to show -- so the ordinary
    // room pays nothing for this on a path the page re-fetches on every state change.
    let owns_a_slot = match session.user_id {
        Some(user_id) if room.slot_auth == SlotAuth::Room => {
            slot::owns_a_slot(&mut conn, room.id, user_id).await?
        }
        _ => false,
    };

    Ok(PanelTemplate {
        is_closed: room.desired_state == DesiredState::Closed.as_sql(),
        may_start: may_start(&room, role),
        is_working: is_working(&room),
        room_password: room_password_for(&room, role.is_some(), owns_a_slot),
        needs_password: room.slot_auth == SlotAuth::Room,
        is_organizer: role.is_some_and(|r| r >= RoomRole::Organizer),
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
        "since_ms": since_ms(transition_began(&room)),
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
        // `requested_stop` was RECORDED and had no phrase, so clicking Stop fell through to the
        // template's fallback and rendered "This room is running" -- the room's observed state,
        // stated as though nothing had been asked. An event kind with no sentence is silent in
        // exactly the moment somebody is watching for one.
        "requested_stop" => "stopping the room",
        "requested_close" => "closing the room",
        "deployment_gone" => "the room's server went away; it can be started again",
        "retrying" => "trying again after a failure",
        "degraded" => "the room is not answering; it may be restarting",
        "ip_mismatch" => "the address was wrong, so the room is moving to another port",
        "failed" => "the last attempt to start this room failed",
        "port_reclaimed" => "this room's port was reassigned while it was idle",
        "slot_released" => "a slot was released back to the pool",
        "slot_password_rotated" => "a slot password was changed",
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
    data_dir: &State<DataDir>,
) -> Result<Redirect> {
    let name = room::validate_name(&form.name)
        .map_err(|e| Error::new(Status::BadRequest, anyhow::anyhow!(e)))?;

    let mut conn = pool.get().await?;

    // The source room having run is not evidence its seed still passes: the checks move with the
    // `pahoa-multidata` pin, and a clone is a NEW room that has to come up on its own.
    refuse_unloadable_seed(&mut conn, &data_dir.0, access.room.generation_id).await?;

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
    /// **Empty for a helper, and that is a credential decision rather than a tidier page.**
    ///
    /// An invite token *is* the grant — following the link confers the role — so an organizer
    /// invite sitting in this list would let any helper who can read the page promote themselves,
    /// which is precisely what their tier withholds. Withheld at the query rather than hidden in
    /// markup, because a template cannot prove it did not render something.
    invites: Vec<member::Invite>,
    /// Whether the viewer may change any of this. A helper sees who the staff are and no controls.
    may_manage: bool,
}

/// Who is staff here, and — for an organizer — the controls that change it.
///
/// **Helper-guarded for the read, organizer for every write.** Knowing who else is staff is
/// ordinary context for somebody who is staff: it is who to escalate to, and it is already
/// inferable from the console's audit trail. What a helper must not gain is any way to add a
/// member, demote an organizer, or elevate themselves — so the five write routes below stay
/// `Organizer`, and the invite list is not loaded at all (see the field's note).
#[get("/room/<id>/members")]
async fn members(
    id: RoomParam,
    access: RoomAccess<Helper>,
    pool: &State<Pool>,
) -> Result<MembersTemplate> {
    let may_manage = access.role() >= RoomRole::Organizer;
    let mut conn = pool.get().await?;
    Ok(MembersTemplate {
        base: TplContext::new(access.session.session()),
        members: member::list(&mut conn, id.0).await?,
        invites: if may_manage {
            member::list_invites(&mut conn, id.0).await?
        } else {
            Vec::new()
        },
        may_manage,
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

/// Would a redeploy request actually restart this room, or sit waiting for somebody to start it?
///
/// **`is_live`, not `state == "running"`**, because that is the set the *planner* sees: its redeploy
/// arm fires only where a Deployment exists, so `starting` and `degraded` both take the request —
/// and both would otherwise be told "next time it starts" while coming up on the Secret they
/// already had.
///
/// A room with no Deployment must **not** get one. The request would sit pending and fire the
/// instant somebody started the room, bouncing it out from under them — the hazard `plan.rs` names
/// where it puts the reaper arm below the redeploy arm.
///
/// Its own function so the rule has one definition and a test can hold it against `RoomState::ALL`
/// rather than against a list repeated here.
fn a_restart_would_land(state: &str) -> bool {
    RoomState::parse(state).is_some_and(RoomState::is_live)
}

/// Give the room a new shared password.
///
/// **Organizer, where the per-slot rotation beside it is a helper's**, and the line is the one M20
/// drew: a helper runs the multiworld, an organizer decides how it is configured and whether it
/// runs at all. This costs a restart, which disconnects everybody — the same reason changing the
/// mode is an organizer's.
///
/// **It is a restart because pahoa has no live setter for this and will not get one**, so the room
/// learns its new password the only way it can. See [`room::rotate_password`] for the reasoning,
/// which is theirs and is good: the environment is authoritative precisely so a stale on-disk value
/// cannot shadow it, and a setter that cannot persist reverts at the next start.
///
/// A stopped room is not redeployed — there is nothing to restart, and its next start renders the
/// Secret from the column — so the flash says which of the two happened rather than making the
/// organizer guess whether anybody was disconnected.
#[post("/room/<id>/settings/rotate-password")]
async fn rotate_room_password(
    id: RoomParam,
    access: RoomAccess<Organizer>,
    pool: &State<Pool>,
) -> Result<Flash<Redirect>> {
    let mut conn = pool.get().await?;

    let Some(_password) = room::rotate_password(&mut conn, id.0).await? else {
        return Ok(Flash::warning(
            Redirect::to(format!("/room/{id}")),
            "This room does not use one shared password, so there is none to rotate.",
        ));
    };

    // Deliberately NOT logged, and not carried back in the flash either: the value is on the page a
    // reload away, for exactly the people entitled to it. A credential in an orchestrator's log is
    // a credential in whatever ships those logs.
    //
    let live = a_restart_would_land(&access.room.state);
    if live {
        // The same signal the admin console's restart button and a mode change both use. A restart
        // re-renders the Secret from scratch, so `mark_secret_stale` would be redundant here.
        puna_core::model::fleet::request_redeploy(&mut conn, &[id.0]).await?;
    }

    tracing::info!(
        room = %id,
        by = access.user_id(),
        restarting = live,
        "the room-wide password was rotated"
    );

    Ok(Flash::success(
        Redirect::to(format!("/room/{id}")),
        if live {
            "New password set. The room is restarting so it takes effect, which drops everyone \
             connected for about a minute."
        } else {
            "New password set. The room will use it the next time it starts."
        },
    ))
}

#[derive(FromForm)]
struct RenameForm {
    name: String,
}

/// Give the room a different name.
///
/// **The one setting on this page that is not a restart**, and the confirm copy says so: object
/// names are `mw-<room id>` and every label carries the id, so `rooms.name` reaches no manifest and
/// no spec hash. Nobody is disconnected and nothing is queued.
///
/// Organizer-guarded rather than helper: a room's name is how everybody refers to it, in Discord
/// and in every link already shared, so changing it is a decision about the room rather than a way
/// of running it. That puts it on the same side of the line as stop, close and the password mode.
#[post("/room/<id>/settings/name", data = "<form>")]
async fn rename_room(
    id: RoomParam,
    access: RoomAccess<Organizer>,
    form: Form<RenameForm>,
    pool: &State<Pool>,
) -> Result<Redirect> {
    let name = room::validate_name(&form.name)
        .map_err(|e| Error::new(Status::BadRequest, anyhow::anyhow!(e)))?;

    // Nothing changed, so record nothing. An audit trail with a row saying a name became itself is
    // one more line between somebody and the change they are looking for.
    if name == access.room.name {
        return Ok(Redirect::to(format!("/room/{id}")));
    }

    let mut conn = pool.get().await?;
    room::rename(&mut conn, id.0, &name).await?;

    // Both names, because the new one is on the row already and the old one is the half that is
    // otherwise gone -- "what was this room called before" is the question a rename raises.
    event::record(
        &mut conn,
        id.0,
        event::Actor::User(access.user_id()),
        "renamed",
        serde_json::json!({ "from": access.room.name, "to": name }),
    )
    .await?;

    tracing::info!(room = %id, by = access.user_id(), from = %access.room.name, to = %name, "room renamed");
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
        release_slot,
        rotate_slot_password,
        slot_password,
        set_slot_auth,
        rotate_room_password,
        rename_room,
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
            locked_at: None,
            locked_by: None,
        }
    }

    /// **A per-slot password reaches its owner and the room's staff, and nobody else at all.**
    ///
    /// `GET /room/<id>` is a PUBLIC page — the unguessable id is the whole authorization — so
    /// rendering the passwords into the slot table means the gate is the only thing between a
    /// shared room link and every player's credential. This is the rule `SlotAccess` applies to the
    /// JSON route, asserted here because the table now shows the value rather than linking to it.
    /// **The shared room password reaches participants and staff, and nobody else.**
    ///
    /// The mode's whole point is that the address alone is not enough to join, and `GET /room/<id>`
    /// is public — so a viewer who merely holds the link must not get the value, or the password is
    /// exactly as secret as the link and the mode means nothing.
    ///
    /// The other half is the mode check, and it is not decoration: switching a room *away* from the
    /// shared password leaves nothing behind today, but a value stranded in the column by any
    /// future path would otherwise render as though it were live — a password people would try, on
    /// a room that no longer wants one.
    #[test]
    fn the_room_password_reaches_participants_and_staff_and_nobody_else() {
        let mut room = a_room();
        room.slot_auth = SlotAuth::Room;
        room.password = Some("abcde-fghij-klmno".into());

        // staff, participant, both
        for (is_staff, owns_a_slot) in [(true, false), (false, true), (true, true)] {
            assert_eq!(
                room_password_for(&room, is_staff, owns_a_slot).as_deref(),
                Some("abcde-fghij-klmno"),
                "staff={is_staff} participant={owns_a_slot}: entitled and not shown the password"
            );
        }

        // Somebody holding nothing but the link.
        assert!(
            room_password_for(&room, false, false).is_none(),
            "a shared room link must not carry the password that gates the room"
        );

        // Every other mode, for a viewer who WOULD be entitled in the shared mode.
        for mode in [SlotAuth::None, SlotAuth::PerSlot] {
            let mut other = room.clone();
            other.slot_auth = mode;
            assert!(
                room_password_for(&other, true, true).is_none(),
                "{mode:?}: a stale column value rendered as this room's password"
            );
        }
    }

    #[test]
    fn a_slot_password_reaches_its_owner_and_staff_and_nobody_else() {
        let mine = 100_i64;
        let theirs = 200_i64;
        let slots = vec![slot(1, Some(mine)), slot(2, Some(theirs)), slot(3, None)];

        // A player: their own, and nothing else. Not the unclaimed slot either -- an unclaimed slot
        // still has a password, and it is not a free credential for whoever asks first.
        let views = slot_views(
            slots.clone(),
            Some(mine),
            None,
            &Default::default(),
            true,
            &Default::default(),
            false,
            &Default::default(),
        );
        assert!(views[0].password.is_some(), "own slot: password expected");
        assert!(
            views[1].password.is_none(),
            "another player's password leaked"
        );
        assert!(
            views[2].password.is_none(),
            "an unclaimed slot's password leaked"
        );

        // A visitor holding the room link and nothing else: none of them.
        let views = slot_views(
            slots.clone(),
            None,
            None,
            &Default::default(),
            true,
            &Default::default(),
            false,
            &Default::default(),
        );
        assert!(
            views.iter().all(|v| v.password.is_none()),
            "the public page handed a visitor the room's credentials"
        );

        // Staff: all of them, which is what makes the table usable for handing them out.
        let views = slot_views(
            slots.clone(),
            Some(999),
            Some(RoomRole::Organizer),
            &Default::default(),
            true,
            &Default::default(),
            false,
            &Default::default(),
        );
        assert!(views.iter().all(|v| v.password.is_some()));

        // **And nothing at all outside per-slot mode**, where the column is not rendered and the
        // value would be a credential with nowhere legitimate to appear.
        let views = slot_views(
            slots,
            Some(999),
            Some(RoomRole::Organizer),
            &Default::default(),
            false,
            &Default::default(),
            false,
            &Default::default(),
        );
        assert!(
            views.iter().all(|v| v.password.is_none()),
            "a password was carried in a mode that has none"
        );
    }

    /// **Who is playing is roster information, and this page is public.**
    ///
    /// The tier is the same as `may_see_spoiler`'s `players`: the room's staff, or somebody who
    /// holds a slot here — the people the roster is *for*. Without the gate, holding a room link
    /// would list everybody in it by Discord username.
    #[test]
    fn usernames_reach_the_room_and_nobody_else() {
        let slots = vec![slot(1, Some(100)), slot(2, Some(200)), slot(3, None)];
        let names: std::collections::HashMap<i64, String> = [
            (100, "alice".to_string()),
            // The lobby-push case: a row exists so the slot can be owned, but nobody has ever
            // signed in under it, so the stored name is the stand-in `ensure_exists` writes.
            (200, puna_core::model::user::placeholder_username(200)),
        ]
        .into_iter()
        .collect();

        // Entitled: a participant, holding no role.
        let views = slot_views(
            slots.clone(),
            Some(100),
            None,
            &Default::default(),
            false,
            &names,
            true,
            &Default::default(),
        );
        assert_eq!(views[0].owner_name.as_deref(), Some("alice"));
        assert!(
            views[1].owner_never_logged_in,
            "a placeholder must be reported as such, not rendered"
        );
        assert!(
            views[2].owner_name.is_none(),
            "an unclaimed slot has no holder"
        );

        // Not entitled: somebody holding the link and nothing else.
        let views = slot_views(
            slots,
            None,
            None,
            &Default::default(),
            false,
            &names,
            false,
            &Default::default(),
        );
        assert!(
            views
                .iter()
                .all(|v| v.owner_name.is_none() && !v.owner_never_logged_in),
            "a visitor was handed the roster"
        );
    }

    /// A placeholder is told apart by its shape, and a real username never wears it.
    #[test]
    fn a_stand_in_username_is_recognizable() {
        use puna_core::model::user::{is_placeholder, placeholder_username};

        assert!(is_placeholder(&placeholder_username(4931)));
        // Discord names cannot contain angle brackets, which is what makes the shape unambiguous.
        for real in ["alice", "Bob_99", "троя", "a<b"] {
            assert!(!is_placeholder(real), "{real} read as a stand-in");
        }
    }

    /// Releasing is **staff's**, helpers included, and only where there is somebody to release.
    ///
    /// A player going quiet mid-async is the case this exists for, so a helper handing the slot to
    /// somebody else is the tier working rather than a gap in it. The line a helper does not cross
    /// is `room_members`, which is a different table and an organizer-guarded route.
    #[test]
    fn staff_are_offered_release_and_only_on_a_claimed_slot() {
        let slots = vec![slot(1, Some(100)), slot(2, None)];

        // A visitor, and somebody who merely holds a slot here: neither is staff, and owning slot 1
        // must not confer a roster action over it.
        for viewer in [None, Some(100)] {
            let views = slot_views(
                slots.clone(),
                viewer,
                None,
                &Default::default(),
                false,
                &Default::default(),
                false,
                &Default::default(),
            );
            assert!(
                views.iter().all(|v| !v.can_release),
                "viewer {viewer:?} is not staff and was offered a roster action"
            );
        }

        for role in [RoomRole::Helper, RoomRole::Organizer] {
            let views = slot_views(
                slots.clone(),
                None,
                Some(role),
                &Default::default(),
                false,
                &Default::default(),
                false,
                &Default::default(),
            );
            assert!(views[0].can_release, "{role:?} may release a claimed slot");
            assert!(!views[1].can_release, "nobody holds slot 2");
        }

        let views = slot_views(
            slots,
            Some(999),
            Some(RoomRole::Organizer),
            &Default::default(),
            false,
            &Default::default(),
            false,
            &Default::default(),
        );
        assert!(views[0].can_release, "a claimed slot can be released");
        assert!(
            !views[1].can_release,
            "an unclaimed slot has nobody to release, so the control would do nothing"
        );
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
        let views = slot_views(
            slots.clone(),
            Some(mine),
            None,
            &Default::default(),
            true,
            &Default::default(),
            false,
            &Default::default(),
        );
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
            true,
            &Default::default(),
            false,
            &Default::default(),
        );
        assert!(
            views.iter().all(|v| v.tracker_id.is_none()),
            "an organizer was handed players' personal tracker links"
        );

        // Anonymous.
        let views = slot_views(
            slots,
            None,
            None,
            &Default::default(),
            true,
            &Default::default(),
            false,
            &Default::default(),
        );
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
            // A room that has been up for a while, which is the situation the elapsed-time bug
            // needed: timing a transition from `state_changed_at` starts the counter at however
            // long the room had been sitting in the state it is leaving.
            state_changed_at: chrono::Utc::now() - chrono::TimeDelta::minutes(35),
            desired_at: chrono::Utc::now() - chrono::TimeDelta::minutes(35),
            advertised_host: Some("mw.example".into()),
            advertised_port: Some(40000),
            advertised_filtered_port: Some(40001),
            last_error: None,
        }
    }

    fn page(is_staff: bool) -> RoomTemplate {
        page_as(is_staff, is_staff)
    }

    fn page_as(is_staff: bool, is_organizer: bool) -> RoomTemplate {
        RoomTemplate {
            base: crate::tpl::TplContext {
                is_logged_in: true,
                is_admin: false,
                username: "troy".into(),
                site_name: "puna",
                version: "test",
                static_version: "test",
                view_as: None,
            },
            room: a_room(),
            slots: Vec::new(),
            is_staff,
            is_organizer,
            // Set only where a room filter is what the test is about; `slot_views` decides the
            // per-slot chips and this decides the room's, so neither is a template's call.
            room_filter: if is_staff {
                Some("This room's filter: drop every PrintJSON Chat sent by any slot".into())
            } else {
                None
            },
            siblings: Vec::new(),
            can_see_spoiler: false,
            can_see_tracker: true,
            message: None,
            elapsed: "1m".into(),
            is_closed: false,
            may_start: true,
            is_working: false,
            owns_a_slot: false,
            room_password: None,
            needs_password: false,
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

    /// **The helper boundary, as the page draws it.**
    ///
    /// A helper runs the room and cannot change whether it runs or who runs it. Every control on
    /// this page falls on one side or the other, and the split is only visible in markup — the
    /// routes enforce it, but a page offering a control the route refuses is how people learn the
    /// site is broken, and a page hiding one they may use is how a tier becomes useless.
    ///
    /// Asserted from one render rather than as separate tests, because the point is the *line*: a
    /// control moving from one list to the other should fail here whichever way it moves.
    #[test]
    fn a_helper_runs_the_room_and_an_organizer_owns_it() {
        let mut helper = page_as(true, false);
        helper.room.slot_auth = SlotAuth::PerSlot;
        helper.slots = vec![SlotView {
            filter_chip: None,
            filter_summary: String::new(),
            slot_number: 1,
            player_name: "Kai".into(),
            game: "A Link to the Past".into(),
            is_spectator: false,
            owner_id: Some(77),
            is_mine: false,
            is_locked: false,
            claim_token: None,
            has_patch: true,
            can_download: true,
            password: Some("abcde-fghij".into()),
            can_release: true,
            tracker_id: None,
            owner_name: Some("kai".into()),
            owner_never_logged_in: false,
        }];
        let html = helper.render().expect("renders");

        // A helper's: running the multiworld and the roster of players.
        for (fragment, what) in [
            ("/console", "the console"),
            ("/slot/1/rotate-password", "rotating a slot's password"),
            ("/slot/1/release", "releasing a claimed slot"),
            ("/members", "seeing who else is staff"),
        ] {
            assert!(html.contains(fragment), "a helper is not offered {what}");
        }

        // An organizer's: whether the room runs, how it is configured, what it is called.
        for (fragment, what) in [
            ("/stop", "stopping the room"),
            ("/close", "closing the room"),
            ("/settings/slot-auth", "changing the password mode"),
            ("/settings/name", "renaming the room"),
        ] {
            assert!(!html.contains(fragment), "a helper was offered {what}");
        }

        // And an organizer gets both halves, so nothing above is hidden from everybody.
        let mut organizer = page_as(true, true);
        organizer.room.slot_auth = SlotAuth::PerSlot;
        let html = organizer.render().expect("renders");
        for fragment in ["/stop", "/close", "/settings/slot-auth", "/settings/name"] {
            assert!(html.contains(fragment), "an organizer lost {fragment}");
        }
    }

    /// **Enter in the name field must SAVE, and what decides that is source order.**
    ///
    /// Pressing Enter in a text input activates the form's *first* submit button. The rename form
    /// holds two controls, and if the cancel one were a `<button>` placed first, Enter would
    /// silently discard the edit — the worst possible outcome for the key everybody presses, and
    /// invisible in review because both controls are correct on their own.
    ///
    /// Two things keep it right and both are asserted: the save button comes first, and cancel is
    /// an `<a>` rather than a submit at all, which is also what makes it work unscripted.
    #[test]
    fn enter_in_the_rename_field_saves_rather_than_cancels() {
        let html = page_as(true, true).render().expect("renders");

        let form = html
            .split_once("/settings/name")
            .expect("the rename form is rendered")
            .1
            .split_once("</form>")
            .expect("the form closes")
            .0;

        let save = form
            .find("<button type=\"submit\"")
            .expect("a save control");
        let cancel = form.find("class=\"cancel\"").expect("a cancel control");
        assert!(
            save < cancel,
            "cancel comes first, so Enter would discard the edit instead of saving it"
        );
        // The tag `class="cancel"` sits in, rather than anything after it -- an earlier version of
        // this scanned forward from the attribute and so could not see the opening tag it was
        // trying to identify, which made it pass against a cancel `<button>`.
        let tag = form[..cancel].rfind('<').expect("cancel sits inside a tag");
        assert!(
            form[tag..].starts_with("<a "),
            "cancel must be a link, not a button: as a submit it would race Enter, and as a plain \
             button it could not close the form without scripting"
        );
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

    /// The same room, seen by an organizer: the one door, labeled for what it does.
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

    /// **The panel tells three viewers three different things about one password.**
    ///
    /// Rendered rather than reasoned about, because the gate and the notice and the control are
    /// three separate conditions over two fields, and the failure that matters is a combination:
    /// a viewer who gets the value they should not, or an organizer offered a control the route
    /// would refuse. `room_password_for` is unit-tested above; this is the markup honouring it.
    #[test]
    fn the_panel_shows_the_password_to_participants_the_control_to_organizers_and_neither_to_a_visitor()
     {
        let in_state = |state: &str, password: Option<&str>, is_organizer: bool| {
            let mut panel = a_panel();
            panel.room.state = state.into();
            panel.is_working = state == "starting";
            panel.room.advertised_host = Some("mw.example".into());
            panel.room.advertised_port = Some(40000);
            panel.room.slot_auth = SlotAuth::Room;
            panel.needs_password = true;
            panel.room_password = password.map(str::to_string);
            panel.is_organizer = is_organizer;
            panel.render().expect("renders")
        };
        let running = |password: Option<&str>, is_organizer: bool| {
            in_state("running", password, is_organizer)
        };

        // **Every state, because the first version of this rendered only in `running`** -- so a
        // stopped room showed no password at all, and rotating on one answered "the room will use
        // it the next time it starts" while showing nothing. Idle is most of an async's life, and a
        // player watching a start is the most likely person here to want the credential in hand.
        for state in [
            "running",
            "idle",
            "failed",
            "stopped",
            "starting",
            "integrity_fault",
        ] {
            let html = in_state(state, Some("abcde-fghij-klmno"), true);
            assert!(
                html.contains("abcde-fghij-klmno"),
                "{state}: the password is withheld from somebody entitled to it"
            );
            assert!(
                html.contains("/settings/rotate-password"),
                "{state}: an organizer cannot rotate a password they can see"
            );
            assert!(
                in_state(state, None, false).contains("needs a password to join"),
                "{state}: a visitor is not told the room needs a password"
            );
        }

        // A participant: the value and the label, and no control that would 403.
        let participant = running(Some("abcde-fghij-klmno"), false);
        assert!(participant.contains("abcde-fghij-klmno"));
        assert!(
            !participant.contains("rotate-password"),
            "a participant is offered a control only an organizer may use"
        );

        // An organizer: both, and the restart named before the button rather than after.
        let organizer = running(Some("abcde-fghij-klmno"), true);
        assert!(organizer.contains("abcde-fghij-klmno"));
        assert!(organizer.contains("/settings/rotate-password"));
        assert!(
            organizer.contains("disconnects everyone"),
            "the rotate control does not say what it costs"
        );

        // Somebody holding nothing but the link: told that a password is needed, never which.
        let visitor = running(None, false);
        assert!(
            !visitor.contains("abcde-fghij-klmno") && !visitor.contains("rotate-password"),
            "a public room page leaked the password that gates the room"
        );
        assert!(
            visitor.contains("needs a password to join"),
            "a visitor is refused by the room with no hint that a password is why"
        );

        // And a room with no shared password says nothing about one at all.
        let mut none = a_panel();
        none.room.state = "running".into();
        none.room.advertised_host = Some("mw.example".into());
        none.room.advertised_port = Some(40000);
        let html = none.render().expect("renders");
        assert!(
            !html.contains("needs a password to join") && !html.contains("rotate-password"),
            "a passwordless room advertises a password it does not have"
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
            room_password: None,
            needs_password: false,
            is_organizer: false,
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

        // **The filtered port still explains itself, and the standard one no longer needs to.**
        // Both tables are one column now -- the descriptions were a column that, once the filtered
        // address moved behind a disclosure, had exactly one row saying the only address on screen
        // is the one to use. The explanation that matters lives on the disclosure itself, which is
        // where somebody deciding between them actually is.
        assert!(
            html.contains("Client lagging or dropping out?"),
            "the second address is offered with nothing to tell somebody whether they want it"
        );
        assert!(
            html.contains("<th>Address</th>"),
            "the address column is not named for the thing players are looking for"
        );

        // **The BODY has to match the header, and asserting the header alone does not say that.**
        // The first version of this checked for `<th>Address</th>` and the absence of
        // `<th>Description</th>` -- both true of a one-column header sitting over a two-column body,
        // which is exactly the state that shipped: a table with a stray cell hanging off every row.
        // Counting cells is the assertion that a table is not broken.
        // `<thead>` starts with `<th`, so a naive substring count reads a one-column header as two.
        // This counts tag STARTS: `<th` or `<td` followed by `>` or an attribute.
        fn cells(fragment: &str, tag: &str) -> usize {
            fragment
                .match_indices(tag)
                .filter(|(at, _)| {
                    fragment[at + tag.len()..].starts_with(|c: char| c == '>' || c.is_whitespace())
                })
                .count()
        }

        for table in html.split("<table class=\"address\">").skip(1) {
            let table = table.split("</table>").next().unwrap_or_default();
            assert_eq!(
                cells(table, "<th"),
                1,
                "the address table has more than one column heading:\n{table}"
            );
            // Body rows only -- the heading row is a `<tr>` too, and counting `<td>` in it finds
            // none.
            let body = table
                .split("<tbody>")
                .nth(1)
                .and_then(|b| b.split("</tbody>").next())
                .expect("the address table has a body");
            assert!(!body.trim().is_empty(), "the address table has no rows");

            for row in body.split("<tr>").skip(1) {
                let row = row.split("</tr>").next().unwrap_or_default();
                assert_eq!(
                    cells(row, "<td"),
                    1,
                    "an address row has a cell the header does not account for:\n{row}"
                );
            }
        }

        // The label is what a screen reader announces, and suppression eats the space before an
        // expression even inside an attribute -- where nothing on screen would reveal it.
        assert!(
            html.contains("aria-label=\"Copy mw.example:40000\""),
            "the copy control's label is missing or ran together"
        );
    }

    /// **Only ONE address is on screen by default, and it is the standard one.**
    ///
    /// The two ports fail asymmetrically. A client that cannot keep up on the standard port is
    /// dropped by pahoa, loudly, with a line in the room's log naming the reason. A player who
    /// takes the *filtered* port by accident has everything work — their game plays, their own
    /// items arrive — and simply never sees anybody else's finds, which reads as a dead multiworld
    /// and gives them no reason to suspect the address they pasted.
    ///
    /// So the guarded failure is somebody copying the first address they see without reading either
    /// label, and the test is positional: the filtered one must sit *after* the disclosure that
    /// hides it. Asserting only that both are present -- which the previous version did -- passes
    /// just as happily with them side by side, which is the layout this replaced.
    #[test]
    fn the_filtered_address_is_behind_a_disclosure_and_the_standard_one_is_not() {
        let html = a_panel().render().expect("renders");

        let disclosure = html
            .find("<details class=\"alt-address\"")
            .expect("the second address has no disclosure to hide it");
        let standard = html
            .find("data-copy=\"mw.example:40000\"")
            .expect("the standard address is not offered");
        let filtered = html
            .find("data-copy=\"mw.example:40001\"")
            .expect("the filtered address is not offered at all");

        assert!(
            standard < disclosure,
            "the standard address must be the one on screen, not the hidden one"
        );
        assert!(
            filtered > disclosure,
            "the filtered address is on screen beside the standard one, so it can be copied by \
             somebody who read neither label -- which fails silently"
        );

        // The summary names the symptom rather than the feature, so the person with the problem
        // recognizes themselves and nobody else opens it.
        assert!(
            html.contains("Client lagging or dropping out?"),
            "the disclosure does not say who it is for"
        );
        // And it warns, at the point of copying, about the thing that has no other symptom.
        //
        // Asserted around the apostrophe rather than through it: the template writes `&rsquo;`
        // literally, askama would write `&#x27;` for the same character in an expression, and a
        // test that pins either spelling breaks on a rewording that changed nothing. This codebase
        // has made that mistake once already.
        assert!(
            html.contains("finds go by"),
            "the second address does not say what it costs"
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

    /// **The counter times the CURRENT state — each phase gets its own clock.**
    ///
    /// Both columns are half right, so `transition_began` takes the later of them. This asserts all
    /// four cases, because each one is a bug that has actually been reported.
    #[test]
    fn each_phase_of_a_transition_is_timed_from_its_own_start() {
        let long_running = chrono::TimeDelta::minutes(35);
        let ago = |secs: i64| chrono::Utc::now() - chrono::TimeDelta::seconds(secs);

        // Just clicked: up for 35 minutes, asked to stop a moment ago. `state_changed_at` alone
        // said "35m" a second after the click, which was the original report.
        let mut stopping = a_room();
        stopping.desired_state = "stopped".into();
        stopping.desired_at = chrono::Utc::now();
        assert!(
            since_ms(transition_began(&stopping)) < 1_000,
            "the counter started at the age of the state being left"
        );

        // **The phase advances and the clock RESTARTS.** The orchestrator acts, `state` becomes
        // `stopping`, and the sentence changes — so the number is how long *this* step has taken,
        // not how long ago the button was pressed. Carrying it across was the previous behavior and
        // is what this test now exists to prevent.
        let mut draining = stopping.clone();
        draining.state = "stopping".into();
        draining.state_changed_at = chrono::Utc::now();
        draining.desired_at = ago(20);
        assert!(
            since_ms(transition_began(&draining)) < 1_000,
            "the count carried over from the request instead of timing this phase"
        );

        // **A redeploy: `desired_at` is STALE and must not be used.** A redeploy never changes
        // `desired_state`, so the request clock still points at whenever the room was first asked
        // to run — hours, on a room that has been up all day. Reported from the live deployment as
        // a counter that opened at a large number and did not reset between phases.
        let mut redeployed = a_room();
        redeployed.state = "starting".into();
        redeployed.desired_state = "running".into();
        redeployed.desired_at = chrono::Utc::now() - chrono::TimeDelta::hours(6);
        redeployed.state_changed_at = ago(5);
        let shown = since_ms(transition_began(&redeployed));
        assert!(
            (4_000..7_000).contains(&shown),
            "a redeploy carried the original start request's clock: {shown}ms"
        );

        // `degraded` needs no special case under this rule: nobody asked for it, so the request
        // clock is old and the `max` picks the state change — which is when it started failing.
        let mut degraded = a_room();
        degraded.state = "degraded".into();
        degraded.desired_at = ago(9_000);
        degraded.state_changed_at = chrono::Utc::now();
        assert!(since_ms(transition_began(&degraded)) < 1_000);

        // And a settled room still reports the age of its state, which is what that branch means.
        let settled = a_room();
        assert!(since_ms(transition_began(&settled)) >= long_running.num_milliseconds() - 1_000);
    }

    /// Every event kind a route records has a sentence.
    ///
    /// `requested_stop` was recorded from the day the stop button existed and never had one, so
    /// clicking Stop fell through to the template's fallback and rendered **"This room is
    /// running"** — the observed state, stated as though nothing had been asked. A kind with no
    /// phrase is silent in exactly the moment somebody is watching for a message.
    #[test]
    fn every_requested_event_has_something_to_say() {
        // The routes only, not this module's own tests -- which necessarily name the prefix in
        // order to search for it, and would otherwise be what the lint reports.
        let source = include_str!("rooms.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("the file has a non-test half");
        let mut checked = 0;

        // The kinds this file records, read out of the file itself so a new one cannot be added
        // without either a phrase or a failure here.
        for reference in source.match_indices("\"requested_") {
            let kind: String = source[reference.0 + 1..]
                .chars()
                .take_while(|c| *c != '"')
                .collect();
            assert!(
                phrase(&kind).is_some(),
                "the route records {kind:?} and nothing turns it into a sentence"
            );
            checked += 1;
        }
        assert!(
            checked >= 3,
            "the lint found {checked} kinds, so it proves little"
        );
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

    /// **The moderation column is staff-only.** `GET /room/<id>` is public — the unguessable id is
    /// the whole authorization — so every control here is one a shared link must not hand out.
    ///
    /// The **width** is not staff-only and briefly was. This page is a heading, an address and a
    /// roster; the only long-form text sits inside a collapsed section, so there is no prose for the
    /// 62rem measure to protect.
    #[test]
    fn only_staff_see_the_moderation_column() {
        let mut staff = page_as(true, false);
        staff.room.slot_auth = SlotAuth::PerSlot;
        staff.slots = vec![a_slot(false)];
        let staff_html = staff.render().expect("renders");

        for command in [
            "lock",
            "kick",
            "hint",
            "hint_location",
            "send_location",
            "send_item",
            "collect",
            "release",
        ] {
            assert!(
                staff_html.contains(&format!("data-command=\"{command}\"")),
                "staff are not offered {command}"
            );
        }
        assert!(
            staff_html.contains("<body class=\"wide\">"),
            "the room page no longer opts out of the prose measure"
        );
        assert!(
            staff_html.contains("id=\"moderate\""),
            "the dialog is missing"
        );

        // **Reachable for a slot that has NO filter yet**, which is the whole point and is what was
        // missing: the chip beside a player's name links here too, and it renders only once a slot
        // already diverges -- so the editor was reachable for exactly the slots that did not need
        // it, and a slot's first filter could only be set from the bulk panel or by typing a URL.
        // The fixture slot has no filter, so this assertion fails if the link goes back behind the
        // chip.
        assert!(
            staff_html.contains("/slot/1/filter"),
            "staff cannot reach a slot's filter editor unless the slot already has one"
        );

        // A player: the same room, the same slot, none of it.
        let mut player = page_as(false, false);
        player.room.slot_auth = SlotAuth::PerSlot;
        player.slots = vec![a_slot(false)];
        let player_html = player.render().expect("renders");

        assert!(
            !player_html.contains("data-command="),
            "a moderation control reached a public page"
        );
        assert!(
            !player_html.contains("id=\"moderate\""),
            "the moderation dialog reached a public page"
        );
        assert!(
            !player_html.contains("/filter"),
            "a filter control reached a public page"
        );
        // The width is the same for everybody: it is a property of the page, not of the viewer.
        assert!(player_html.contains("<body class=\"wide\">"));
    }

    /// The lock control is drawn as the state's REMEDY, not its description: a locked slot offers
    /// "unlock". Getting this backwards is a control that reads correctly and does the opposite of
    /// what the operator wants, twice in a row.
    #[test]
    fn a_locked_slot_offers_the_way_out_of_the_lock() {
        let mut staff = page_as(true, false);
        staff.room.slot_auth = SlotAuth::PerSlot;

        staff.slots = vec![a_slot(false)];
        let open = staff.render().expect("renders");
        assert!(
            open.contains(r#"data-locked="true""#),
            "an open slot must offer a lock"
        );
        assert!(
            open.contains("Lock:"),
            "the tooltip does not describe locking"
        );

        staff.slots = vec![a_slot(true)];
        let shut = staff.render().expect("renders");
        assert!(
            shut.contains(r#"data-locked="false""#),
            "a locked slot must offer an unlock"
        );
        assert!(
            shut.contains("Unlock:"),
            "the tooltip does not describe unlocking"
        );
    }

    /// **Locking is offered in every password mode**, which is the whole point of adopting pahoa's
    /// own verb: the trick it replaced — withholding a slot from `PAHOA_SLOT_PASSWORDS` — needed
    /// per-slot mode to exist at all, so a room with no password or one shared password could not
    /// bar anybody.
    ///
    /// Asserted against the mode that previously hid it, because "it works everywhere now" is the
    /// claim, and the old gate would still pass a test that only looked at a per-slot room.
    #[test]
    fn the_lock_control_is_offered_in_every_password_mode() {
        for mode in [SlotAuth::None, SlotAuth::Room, SlotAuth::PerSlot] {
            let mut staff = page_as(true, false);
            staff.room.slot_auth = mode;
            staff.slots = vec![a_slot(false)];
            let html = staff.render().expect("renders");

            assert!(
                html.contains(r#"data-command="lock""#),
                "{mode:?}: the lock control is missing, and pahoa's verb needs no password mode"
            );
        }
    }

    /// **A locked slot says so in a word, and only to staff.**
    ///
    /// The chip is the primary signal rather than a decoration on the glyph: the lock and unlock
    /// controls are a padlock either way, and telling them apart at a glance down a roster is not
    /// something a 15px icon does. So the column that answers "who is shut out?" is the roster, and
    /// the glyph is only the control that changes it.
    ///
    /// Staff-only because `slot_views` gates `is_locked` on the viewer's role — this page is public,
    /// and whether somebody has been barred is not a fact for everyone holding the link.
    #[test]
    fn a_locked_slot_is_named_in_the_roster_and_only_to_staff() {
        let mut staff = page_as(true, false);
        staff.slots = vec![a_slot(true)];
        let staff_html = staff.render().expect("renders");
        assert!(
            staff_html.contains(">locked<"),
            "a locked slot is indistinguishable from an open one while scanning the roster"
        );

        // Not locked: no chip, or the word means nothing.
        let mut open = page_as(true, false);
        open.slots = vec![a_slot(false)];
        assert!(!open.render().expect("renders").contains(">locked<"));

        // **And the gate is asserted where it lives, which is not the template.** `SlotView`'s own
        // rule is that the decision happens in `slot_views` and the markup only asks whether there
        // is a value -- a template cannot prove it did not render something. So this goes through
        // the function rather than rendering a hand-built view, which would only prove the template
        // renders what it is given.
        let mut locked = slot(1, Some(100));
        locked.locked_at = Some(chrono::Utc::now());

        let staff_view = slot_views(
            vec![locked.clone()],
            None,
            Some(RoomRole::Helper),
            &Default::default(),
            false,
            &Default::default(),
            false,
            &Default::default(),
        );
        assert!(staff_view[0].is_locked, "staff cannot see who is barred");

        let public_view = slot_views(
            vec![locked],
            None,
            None,
            &Default::default(),
            false,
            &Default::default(),
            false,
            &Default::default(),
        );
        assert!(
            !public_view[0].is_locked,
            "a public page would name somebody as barred"
        );
    }

    /// **The same slot state means two different things, and the chip has to say which.**
    ///
    /// A slot with rules of its own is *not running the room's* — pahoa replaces rather than merges
    /// — and that is the fact worth a chip. But it is only a fact when there IS a room filter: with
    /// none, "overrides room filter" names something that does not exist, and the slot is simply the
    /// only filtered one. So the word depends on the room, which is why `slot_views` takes the
    /// room's state alongside the slots'.
    #[test]
    fn a_slot_chip_says_whether_it_is_overriding_anything() {
        use puna_core::model::filter::{Direction, Kind, Rule, SlotFilter};

        let own = || {
            let mut slots = std::collections::HashMap::new();
            slots.insert(
                1,
                SlotFilter::Own(vec![Rule {
                    direction: Direction::FromSlot,
                    kind: Kind::Bounce,
                    tag: Some("DeathLink".into()),
                    subtype: None,
                    p: None,
                }]),
            );
            slots
        };

        let chip = |room_filters: bool, slots| {
            slot_views(
                vec![slot(1, Some(77))],
                Some(77),
                Some(RoomRole::Helper),
                &Default::default(),
                false,
                &Default::default(),
                true,
                &Filters {
                    room_filters,
                    slots,
                },
            )
            .remove(0)
        };

        let overriding = chip(true, own());
        assert_eq!(overriding.filter_chip, Some("overrides room filter"));
        assert!(
            overriding.filter_summary.contains("instead of the room's"),
            "{}",
            overriding.filter_summary
        );

        // No room filter, so there is nothing to be overriding and nothing to be instead of.
        let alone = chip(false, own());
        assert_eq!(alone.filter_chip, Some("filtered"));
        assert!(
            !alone.filter_summary.contains("instead of"),
            "the hover names a room filter that does not exist: {}",
            alone.filter_summary
        );

        // Exempt reads the same either way — "unfiltered" already says it is not doing what the
        // room does, and with no room filter it is still a deliberate state worth marking.
        let mut exempt = std::collections::HashMap::new();
        exempt.insert(1, SlotFilter::Exempt);
        assert_eq!(chip(true, exempt.clone()).filter_chip, Some("unfiltered"));
        assert_eq!(chip(false, exempt).filter_chip, Some("unfiltered"));

        // And a slot that follows the room gets no chip at all, whatever the room does: it is not a
        // fact about that row.
        assert_eq!(chip(true, Default::default()).filter_chip, None);
    }

    /// **A room-wide filter has to be visible from the room, and to a helper as well.**
    ///
    /// Nothing on this page said one existed, so a room quietly dropping every DeathLink looked
    /// exactly like a room where DeathLink was broken — and the helper fielding that question is
    /// the person least equipped to find out, because the editor is an organizer's.
    ///
    /// So the chip is shown to both and is a **link for an organizer only**. A helper following it
    /// would meet a 403, which is the "control that exists and cannot be used" failure wearing its
    /// other face.
    #[test]
    fn a_room_filter_is_visible_to_staff_and_only_an_organizer_can_follow_it() {
        let organizer = page_as(true, true).render().expect("renders");
        let helper = page_as(true, false).render().expect("renders");
        let player = page_as(false, false).render().expect("renders");

        assert!(
            organizer.contains(r#"<a class="tag filter" href="/room/"#),
            "an organizer gets no way through to the room filter"
        );
        assert!(
            helper.contains("room filtered"),
            "a helper cannot tell the room is filtering at all"
        );
        assert!(
            !helper.contains(r#"<a class="tag filter""#),
            "a helper is offered a link to a page that answers 403"
        );
        assert!(
            !player.contains("room filtered"),
            "a public page names a filter that explains nothing to a player"
        );

        // The hover carries what it drops, in the room's own words rather than a bare `p`.
        for page in [&organizer, &helper] {
            assert!(
                // Matched from `filter:` rather than the whole prefix: askama escapes the
                // apostrophe in "room's" to `&#x27;` inside the attribute, which is correct and
                // which an assertion on the plain sentence would fail on for the wrong reason.
                page.contains("filter: drop every PrintJSON Chat"),
                "the chip has no hover summary, so it says something is filtered and not what"
            );
        }
    }

    /// **Lock bars the next login and disconnects nobody**, and the control has to say so: the
    /// obvious reading of "Lock" is that it ejects somebody. The order that actually works against a
    /// griefer is lock THEN kick — kicking first leaves a window to reconnect.
    #[test]
    fn the_lock_control_says_it_does_not_disconnect_anybody() {
        let mut staff = page_as(true, false);
        staff.slots = vec![a_slot(false)];
        let html = staff.render().expect("renders");

        let at = html
            .find(r#"data-command="lock""#)
            .expect("the lock control is gone");
        let element = &html[at..at + 400.min(html.len() - at)];
        assert!(
            element.contains("disconnect"),
            "the lock tooltip does not mention that it disconnects nobody:\n{element}"
        );
        assert!(
            element.contains("kick"),
            "the lock tooltip does not point at kick, which is the half that ejects:\n{element}"
        );
    }

    fn a_slot(locked: bool) -> SlotView {
        SlotView {
            filter_chip: None,
            filter_summary: String::new(),
            slot_number: 1,
            player_name: "Kai".into(),
            game: "A Link to the Past".into(),
            is_spectator: false,
            owner_id: Some(77),
            is_mine: false,
            is_locked: locked,
            claim_token: None,
            has_patch: true,
            can_download: true,
            password: Some("abcde-fghij".into()),
            tracker_id: None,
            can_release: true,
            owner_name: Some("kai".into()),
            owner_never_logged_in: false,
        }
    }

    /// **Every dialog has to say which slot it is for**, and the pieces that make that possible are
    /// in three files, so this checks all three agree.
    ///
    /// The failure is a misclick that cannot be caught: a confirmation reading only "Hint an item"
    /// looks the same whichever row was clicked, and the two controls that skip confirmation have
    /// nothing but this to show who they acted on. The target is read from the control's own cell
    /// rather than passed around, so it cannot describe one row while acting on another.
    #[test]
    fn every_moderation_dialog_can_name_the_slot_it_acts_on() {
        let mut staff = page_as(true, false);
        staff.room.slot_auth = SlotAuth::PerSlot;
        staff.slots = vec![a_slot(false)];
        let html = staff.render().expect("renders");

        // The cell carries who and what game, once per row rather than on each of nine controls.
        assert!(
            html.contains(r#"data-player="Kai""#),
            "the moderation cell does not carry the player it acts on"
        );
        assert!(
            html.contains(r#"data-game="A Link to the Past""#),
            "the moderation cell does not carry the game, which scopes the suggestions"
        );

        // And the dialog has somewhere to put it.
        assert!(
            html.contains("data-mod-target"),
            "the dialog has no target line"
        );

        // **Outside the three stages**, or the answer stage -- the moment it matters most -- would
        // not show it. Asserted by position: the target must precede the form that the working and
        // result panes replace.
        let target = html.find("data-mod-target").expect("checked above");
        let form = html.find("data-mod-form").expect("the dialog has a form");
        assert!(
            target < form,
            "the target sits inside a stage, so it disappears when that stage is swapped out"
        );

        // The script reads exactly these names. A rename on either side is silent: the dialog still
        // opens, and the line is simply blank.
        let script = include_str!("../../static/moderation.js");
        for hook in ["data-mod-target", "dataset.player", "dataset.game"] {
            assert!(
                script.contains(hook),
                "moderation.js no longer reads {hook}, so the target line renders empty"
            );
        }

        // Every command must have a title, including the two that never open a confirmation --
        // their spinner and answer use the same shared heading.
        for command in ["lock", "kick"] {
            let at = script
                .find(&format!("{command}: {{"))
                .unwrap_or_else(|| panic!("{command} left the script"));
            let entry = &script[at..at + 120.min(script.len() - at)];
            assert!(
                entry.contains("title:"),
                "{command} has no title, so its dialog is headed \"Confirm\" with nothing confirmed"
            );
        }
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::testdb::{insert_generation, insert_room, with_db};
    use diesel_async::RunQueryDsl;
    use puna_core::model::fleet;

    /// **Rotating the shared password refuses every other mode, and restarts only a live room.**
    ///
    /// The mode is scoped in the `WHERE` rather than checked first, so this is what says a room in
    /// another mode is left alone entirely rather than being handed a password the
    /// `room_password_matches_mode` CHECK would then refuse.
    ///
    /// The redeploy half is the one worth a database: the planner's redeploy arm fires only where a
    /// Deployment exists, so requesting one on a room with none leaves it pending to fire the
    /// instant somebody starts the room — bouncing it out from under them. `is_live` is the same
    /// three states the planner sees, which is why the route uses it rather than `== "running"`.
    #[tokio::test]
    async fn rotating_the_room_password_needs_the_mode_and_restarts_only_a_live_room() {
        with_db(|pool| async move {
            let conn = &mut pool.get().await.expect("connection");
            let generation = insert_generation(conn).await;

            // Every other mode: refused, and nothing written.
            for mode in ["none", "per_slot"] {
                let id = insert_room(conn, generation, "running", mode).await;
                assert!(
                    room::rotate_password(conn, id).await.unwrap().is_none(),
                    "{mode}: rotated a shared password on a room that has none"
                );
            }

            // Shared mode: a new value, and it really is new.
            let id = insert_room(conn, generation, "running", "room").await;
            let before = room::get(conn, id).await.unwrap().unwrap().password;
            let rotated = room::rotate_password(conn, id).await.unwrap();
            assert!(rotated.is_some());
            let after = room::get(conn, id).await.unwrap().unwrap().password;
            assert_eq!(
                after, rotated,
                "the row does not hold what the caller was handed"
            );
            assert_ne!(
                after, before,
                "rotation returned the password it was already using"
            );

            // Live states get the restart; a room with no Deployment must not.
            for (state, expect_redeploy) in [
                ("running", true),
                ("starting", true),
                ("degraded", true),
                ("idle", false),
                ("failed", false),
            ] {
                let id = insert_room(conn, generation, state, "room").await;
                // Through the function the ROUTE calls, so the rule and its use are one thing.
                let live = a_restart_would_land(state);
                assert_eq!(
                    live, expect_redeploy,
                    "{state}: the restart predicate disagrees with the planner"
                );
                if live {
                    fleet::request_redeploy(conn, &[id]).await.unwrap();
                }
                // `Room` does not carry the column -- only the fleet projection does -- so read it
                // directly rather than widening a struct for a test.
                #[derive(diesel::QueryableByName)]
                struct Pending {
                    #[diesel(sql_type = diesel::sql_types::Bool)]
                    queued: bool,
                }
                let pending: Vec<Pending> = diesel::sql_query(
                    "SELECT redeploy_requested_at IS NOT NULL AS queued FROM rooms WHERE id = $1",
                )
                .bind::<diesel::sql_types::Uuid, _>(id)
                .load(conn)
                .await
                .unwrap();
                assert_eq!(
                    pending[0].queued, expect_redeploy,
                    "{state}: a redeploy left pending on a room with no Deployment fires the \
                     moment somebody starts it"
                );
            }
        })
        .await;
    }
}
