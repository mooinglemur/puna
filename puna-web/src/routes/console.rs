//! The room console.
//!
//! Every action here is a **row in `room_commands`**, not a call into a room. The web tier holds no
//! path to pahoa's admin API and needs none: it writes what was asked and who asked, the
//! orchestrator does it, and the answer comes back through the row. That is what buys the audit
//! trail and what survives a restart mid-command.
//!
//! ## The tier check happens once, on the command
//!
//! The route is `Helper`-gated because that is the floor for reaching the console at all, and then
//! each command is checked against [`RoomCommand::required_role`]. One table, checked in one place —
//! so adding a command means answering "which tier?" rather than remembering to guard a route.
//!
//! ## What the UI must not do
//!
//! **Not grey out commands based on the room's options.** An admin is deliberately not bound by the
//! modes that gate players: `--release-mode disabled` stops `!release` and does not stop
//! `{"command":"release"}`, because acting for somebody who cannot is the point.

use puna_core::db::Pool;
use puna_core::ids::CommandId;
use puna_core::model::command::{self, CommandRow, RoomCommand};
use rocket::form::Form;
use rocket::http::Status;
use rocket::response::Redirect;
use rocket::{FromForm, State, get, post, routes};

use askama::Template;
use askama_web::WebTemplate;

use crate::commands::{self, Waiters};
use crate::error::{Error, Result, forbidden};
use crate::guards::{Helper, RoomAccess};
use crate::params::RoomParam;
use crate::tpl::TplContext;

#[derive(Template, WebTemplate)]
#[template(path = "rooms/console.html")]
pub struct ConsoleTemplate {
    base: TplContext,
    room_id: String,
    room_name: String,
    room_state: String,
    /// The room's slots, for the target picker. A dropdown rather than a number field: a mistyped
    /// slot number is a release into somebody else's game.
    slots: Vec<(i32, String)>,
    history: Vec<HistoryEntry>,
    /// The result of the command just submitted, if this is the redirect after one.
    outcome: Option<HistoryEntry>,
    /// Set when the command outlived the request budget. **Not an error**: it is still running, and
    /// the history pane will show it.
    still_running: bool,
    /// Set when a credential change landed in the database for a room that is not running, so there
    /// was nothing to tell. Distinct from an error and from a refusal: it worked, and it takes
    /// effect at the next start.
    stored: bool,
    /// Set when the room began starting while a credential change was being written, so whether the
    /// pod read it is unknown. **Neither of the confident answers is available here**, and saying
    /// one anyway is the failure this flag exists to avoid.
    uncertain: bool,
    /// Whether to offer `option`, which is the one command a helper may not run. Hidden rather than
    /// disabled — a visible control that refuses teaches people the tool is broken — and the route
    /// re-checks regardless, since it is reachable by anyone who can construct a POST.
    is_organizer: bool,
    /// Whether locking has a spelling in this room. It is expressed as an omission from the
    /// per-slot password map, so outside that mode there is no map and nothing to omit from.
    per_slot_passwords: bool,
}

/// One line of the console's history.
pub struct HistoryEntry {
    pub id: String,
    pub kind: String,
    pub state: String,
    /// `true` only when the room said yes. A refusal is a completed command whose answer was no,
    /// and rendering the two the same way is how "it worked" gets misread.
    pub succeeded: bool,
    pub lines: Vec<String>,
    pub requested_at: String,
}

impl HistoryEntry {
    fn from_row(row: &CommandRow) -> Self {
        let mut lines: Vec<String> = row
            .result
            .as_ref()
            .map(|r| r.output.clone())
            .unwrap_or_default();
        if let Some(error) = &row.error {
            lines.push(error.clone());
        }

        Self {
            id: row.id.to_string(),
            kind: row.command.name().to_string(),
            state: row.state.clone(),
            succeeded: row.state == "ok" && row.result.as_ref().is_some_and(|r| r.ok),
            lines,
            requested_at: row.requested_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        }
    }
}

/// The console pane.
#[get("/room/<_id>/console?<ran>&<pending>&<stored>&<uncertain>")]
async fn show(
    _id: RoomParam,
    ran: Option<String>,
    pending: Option<bool>,
    stored: Option<bool>,
    uncertain: Option<bool>,
    access: RoomAccess<Helper>,
    pool: &State<Pool>,
) -> Result<ConsoleTemplate> {
    let mut conn = pool.get().await?;
    let room = &access.room;

    let history = command::recent(&mut conn, room.id, 20).await?;
    let outcome = match ran.as_deref().and_then(|id| id.parse::<CommandId>().ok()) {
        Some(id) => command::get(&mut conn, id)
            .await?
            .as_ref()
            .map(HistoryEntry::from_row),
        None => None,
    };

    let slots = puna_core::model::slot::list(&mut conn, room.id)
        .await?
        .into_iter()
        .map(|s| (s.slot_number, s.player_name))
        .collect();

    Ok(ConsoleTemplate {
        base: TplContext::new(access.session.session()),
        room_id: room.id.to_string(),
        room_name: room.name.clone(),
        room_state: room.state.clone(),
        slots,
        history: history.iter().map(HistoryEntry::from_row).collect(),
        outcome,
        still_running: pending.unwrap_or(false),
        stored: stored.unwrap_or(false),
        uncertain: uncertain.unwrap_or(false),
        is_organizer: access.role() >= puna_core::model::member::RoomRole::Organizer,
        per_slot_passwords: room.slot_auth == puna_core::model::room::SlotAuth::PerSlot,
    })
}

/// The console form.
///
/// One shape for every command, because a form per command would be eight nearly-identical routes.
/// The fields a command does not use are simply absent, and [`build`] is where "absent but needed"
/// becomes an error somebody can read.
#[derive(FromForm)]
pub struct CommandForm {
    kind: String,
    slot: Option<i32>,
    text: Option<String>,
    item: Option<String>,
    /// For `hint_location` and `send_location`. A separate field from `item` rather than one
    /// "name" box, because they are looked up in different tables and an autocomplete has to know
    /// which — and because pahoa matches **exactly**, so offering the wrong kind of name produces a
    /// refusal rather than a near miss.
    location: Option<String>,
    seconds: Option<i64>,
    /// For `send_multiple`. **Required, with no default**: pahoa caps it at 100 and every copy is
    /// replayed from index zero on each reconnect, so a default of one would make a command that
    /// did a fraction of its job look like it worked.
    amount: Option<i64>,
    #[field(default = false)]
    force: bool,
    /// For `allow_release`. `Option<bool>` from a select rather than a checkbox, because both
    /// answers are deliberate here: an unchecked box is indistinguishable from a box nobody read,
    /// and `false` on this command means something specific and easy to mistake for a denial.
    allowed: Option<bool>,
    /// For `lock_slot`, and the same reasoning as `allowed`.
    locked: Option<bool>,
    /// For `alias`. **Not filtered for blankness**: empty is how an alias is cleared.
    alias: Option<String>,
    option_name: Option<String>,
    option_value: Option<String>,
    reason: Option<String>,
}

/// Turn the form into a typed command, naming the field that is missing.
///
/// Hand-built rather than derived so a missing field says which one — the same reasoning pahoa's
/// own parser gives, and for the same audience.
fn build(form: &CommandForm) -> std::result::Result<RoomCommand, String> {
    let slot = || form.slot.ok_or_else(|| "choose a slot".to_string());
    let item = || {
        form.item
            .as_deref()
            .filter(|i| !i.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| "name an item".to_string())
    };
    let location = || {
        form.location
            .as_deref()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| "name a location".to_string())
    };

    Ok(match form.kind.as_str() {
        "status" => RoomCommand::Status,
        "say" => RoomCommand::Say {
            text: form
                .text
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| "say what?".to_string())?,
        },
        "countdown" => RoomCommand::Countdown {
            seconds: form
                .seconds
                .ok_or_else(|| "how many seconds?".to_string())?,
        },
        "release" => RoomCommand::Release { slot: slot()? },
        "collect" => RoomCommand::Collect { slot: slot()? },
        "send_item" => RoomCommand::SendItem {
            slot: slot()?,
            item: item()?,
        },
        "send_multiple" => RoomCommand::SendMultiple {
            slot: slot()?,
            item: item()?,
            // Bounded here as well as by pahoa, so the answer to a typo is a sentence rather than a
            // round trip -- and the limit is named, because "too many" without the number is the
            // kind of error that gets guessed at twice.
            amount: match form.amount {
                Some(amount) if (1..=100).contains(&amount) => amount,
                Some(amount) => {
                    return Err(format!("{amount} is not between 1 and 100 copies"));
                }
                None => return Err("how many copies?".to_string()),
            },
        },
        "hint" => RoomCommand::Hint {
            slot: slot()?,
            item: item()?,
            force: form.force,
        },
        "hint_location" => RoomCommand::HintLocation {
            slot: slot()?,
            location: location()?,
            force: form.force,
        },
        "send_location" => RoomCommand::SendLocation {
            slot: slot()?,
            location: location()?,
        },
        "allow_release" => RoomCommand::AllowRelease {
            slot: slot()?,
            allowed: form.allowed.ok_or_else(|| {
                "grant the exemption, or return the slot to the room's mode?".to_string()
            })?,
        },
        "alias" => RoomCommand::Alias {
            slot: slot()?,
            // **Deliberately not filtered for blankness**, unlike every other text field here:
            // empty is not a missing value, it is how an alias is cleared.
            alias: form.alias.clone().unwrap_or_default(),
        },
        "option" => RoomCommand::SetOption {
            name: form
                .option_name
                .as_deref()
                .filter(|n| !n.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| "which option?".to_string())?,
            value: form
                .option_value
                .as_deref()
                .filter(|v| !v.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| "what value?".to_string())?,
        },
        "rotate_password" => RoomCommand::RotatePassword { slot: slot()? },
        "lock_slot" => RoomCommand::LockSlot {
            slot: slot()?,
            locked: form.locked.ok_or_else(|| "lock, or unlock?".to_string())?,
        },
        "kick" => RoomCommand::Kick {
            slot: slot()?,
            reason: form
                .reason
                .as_deref()
                .filter(|r| !r.trim().is_empty())
                .map(str::to_string),
        },
        other => return Err(format!("unknown command {other:?}")),
    })
}

/// What the database half of a slot-credential command left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Prepared {
    /// Not one of the two credential commands. Nothing was written.
    NotApplicable,
    /// Written, and the room is up: queue the command so the orchestrator makes it live.
    Live(&'static str),
    /// Written, and there is no process to tell. It takes effect the next time the room starts,
    /// because a start renders the Secret from the row.
    Stored(&'static str),
    /// Written — and the room began starting while it was being written, so whether the pod read
    /// the new map is a coin toss.
    ///
    /// **Reported as "we do not know" rather than as either outcome.** The tempting answers are
    /// both wrong: "it worked" may be false, and "it takes effect at the next start" is false *by
    /// construction*, because the start it would take effect on is the one already in flight.
    Uncertain(&'static str),
}

impl Prepared {
    /// The `room_events` kind, where something was written.
    pub(crate) fn kind(self) -> Option<&'static str> {
        match self {
            Self::NotApplicable => None,
            Self::Live(kind) | Self::Stored(kind) | Self::Uncertain(kind) => Some(kind),
        }
    }
}

/// The database half of a slot-credential command, which happens **before** the row is queued.
///
/// [`RoomCommand::RotatePassword`] and [`RoomCommand::LockSlot`] are the two commands that are not
/// pahoa's: each writes `room_slots` here and then asks the orchestrator to make the room agree.
/// The value never travels on the queue — the orchestrator reads the row — which is what keeps a
/// credential out of the audit trail.
///
/// **`mark_secret_stale` is the durable half and is not optional.** Without it the change lives only
/// in a running pod's memory and lapses at the next restart — silently, and restarts happen for
/// reasons nobody decided, like a reap or an image bump.
///
/// ## A room in transition is refused, and nothing is written
///
/// The credential reaches a pod two ways: through the Secret, which the pod reads **once**, when its
/// container starts; and through the live endpoint, which needs a room that is answering. A room
/// that is `starting` has neither — the pod may have already read the old map, and it is not yet
/// accepting requests — so there is no honest thing to say about a change made then. It would be
/// stored and possibly not in force, with the page claiming it takes effect at a start that has
/// already happened.
///
/// So that case refuses **before writing anything**, and says which state the room is in. Failing is
/// an acceptable answer here; claiming a lock is in force when it may not be is not.
pub(crate) async fn prepare_slot_credential(
    conn: &mut diesel_async::AsyncPgConnection,
    room: &puna_core::model::room::Room,
    command: &RoomCommand,
    by: i64,
) -> Result<Prepared> {
    use puna_core::model::room::SlotAuth;
    use puna_core::model::{room, slot};

    let (slot_number, locking) = match command {
        RoomCommand::RotatePassword { slot } => (*slot, None),
        RoomCommand::LockSlot { slot, locked } => (*slot, Some(*locked)),
        _ => return Ok(Prepared::NotApplicable),
    };

    // The two states where a change is expressible. Everything else -- `starting`, `stopping`,
    // `degraded`, `provisioning`, `deleting`, `integrity_fault` -- is a room whose pod exists or is
    // about to, and cannot be told.
    let at_rest = matches!(room.state.as_str(), "idle" | "failed");
    if room.state != "running" && !at_rest {
        return Err(crate::error::Error::new(
            Status::Conflict,
            anyhow::anyhow!(
                "this room is {}, so a credential change can neither reach it now nor be \
                 guaranteed to reach it when it comes up. Nothing was changed; try again once it \
                 is running.",
                room.state
            ),
        ));
    }

    // 404 rather than 400, matching the JSON route and pahoa itself: outside per-slot mode there is
    // no such thing as this slot's password, so there is nothing to rotate and nothing to withhold.
    // Locking is expressed *as* an omission from the password map, so without the map it has no
    // spelling at all.
    if room.slot_auth != SlotAuth::PerSlot {
        return Err(crate::error::not_found(
            "this room does not use per-slot passwords",
        ));
    }

    let slots = slot::list(conn, room.id).await?;
    if !slots.iter().any(|s| s.slot_number == slot_number) {
        return Err(crate::error::not_found("no such slot"));
    }

    let kind = match locking {
        None => {
            slot::rotate_password(conn, room.id, slot_number).await?;
            "slot_password_rotated"
        }
        Some(true) => {
            // **Refused here rather than discovered later.** The Secret builder will not render an
            // empty password map, so locking the last unlocked slot would leave the room unable to
            // start at all -- and it would surface minutes later as a `failed` room with a Secret
            // error, long after the person who caused it stopped looking. Locking everybody is
            // closing the room, which has its own control and says what it does.
            let unlocked = slots
                .iter()
                .filter(|s| !s.is_locked() && s.slot_number != slot_number)
                .count();
            if unlocked == 0 {
                return Err(crate::error::Error::new(
                    Status::Conflict,
                    anyhow::anyhow!(
                        "that is the last slot anyone can connect to. Locking every slot is \
                         closing the room, which is on the room's own controls."
                    ),
                ));
            }
            if !slot::set_locked(conn, room.id, slot_number, true, by).await? {
                return Ok(Prepared::NotApplicable);
            }
            "slot_locked"
        }
        Some(false) => {
            if !slot::set_locked(conn, room.id, slot_number, false, by).await? {
                return Ok(Prepared::NotApplicable);
            }
            "slot_unlocked"
        }
    };

    room::mark_secret_stale(conn, room.id).await?;

    if room.state == "running" {
        return Ok(Prepared::Live(kind));
    }

    // **Read the state back**, because the one above came from the request guard and a room can
    // begin starting in between. `start` renders the Secret from the row, so a start that read the
    // row before this write produces a pod running the old map with the row saying otherwise —
    // silently, which is the whole class of failure worth spending a query to avoid.
    let moved = room::get(conn, room.id)
        .await?
        .is_none_or(|current| current.state != room.state);
    Ok(if moved {
        Prepared::Uncertain(kind)
    } else {
        Prepared::Stored(kind)
    })
}

/// Queue a command and wait for the answer.
#[post("/room/<_id>/command", data = "<form>")]
async fn run(
    _id: RoomParam,
    form: Form<CommandForm>,
    access: RoomAccess<Helper>,
    pool: &State<Pool>,
    waiters: &State<std::sync::Arc<Waiters>>,
) -> Result<Redirect> {
    let command =
        build(&form).map_err(|message| Error::new(Status::BadRequest, anyhow::anyhow!(message)))?;

    // **The one place the tier is checked.** The route's own guard is only the floor for reaching
    // the console; this is what separates a helper from an organizer.
    if access.role() < command.required_role() {
        return Err(forbidden(
            "that command needs an organizer, and you are a helper here",
        ));
    }

    let mut conn = pool.get().await?;

    // The two Puna-side commands write the row before the row is queued, so the orchestrator has
    // something to push. Every other command passes straight through.
    let prepared =
        prepare_slot_credential(&mut conn, &access.room, &command, access.user_id()).await?;
    if let Some(kind) = prepared.kind() {
        puna_core::model::event::record(
            &mut conn,
            access.room.id,
            puna_core::model::event::Actor::User(access.user_id()),
            kind,
            // The slot, never the value. This row is read by anyone who can read the room's
            // history.
            serde_json::json!({ "slot": command.target_slot() }),
        )
        .await?;
    }

    let room = access.room.id.to_string();

    // **A room with no process to tell is told nothing, and that is not a failure.** The durable
    // half has landed and a start renders the Secret from the row, so queueing would only produce a
    // `rejected` row saying the room is down -- true, and nothing the operator needs to act on.
    match prepared {
        Prepared::Stored(_) => {
            return Ok(Redirect::to(format!("/room/{room}/console?stored=true")));
        }
        Prepared::Uncertain(_) => {
            return Ok(Redirect::to(format!("/room/{room}/console?uncertain=true")));
        }
        Prepared::NotApplicable | Prepared::Live(_) => {}
    }

    let id = command::enqueue(
        &mut conn,
        access.room.id,
        access.user_id(),
        // **What authorized it, frozen now.** The roster can change, and "an organizer did this"
        // has to stay true afterwards.
        access.role(),
        &command,
    )
    .await?;
    drop(conn);

    match commands::wait_for(pool, waiters.inner(), id).await {
        Some(_) => Ok(Redirect::to(format!("/room/{room}/console?ran={id}"))),
        // Out of budget. The command is still running and the row is still readable, so this is a
        // slower answer rather than a lost one.
        None => Ok(Redirect::to(format!(
            "/room/{room}/console?ran={id}&pending=true"
        ))),
    }
}

/// One command's row, for polling and for a link out of the history pane.
#[get("/room/<_id>/command/<cid>")]
async fn one(
    _id: RoomParam,
    cid: &str,
    access: RoomAccess<Helper>,
    pool: &State<Pool>,
) -> Result<rocket::serde::json::Json<serde_json::Value>> {
    let id: CommandId = cid
        .parse()
        .map_err(|_| Error::new(Status::NotFound, anyhow::anyhow!("not a command id")))?;

    let mut conn = pool.get().await?;
    let row = command::get(&mut conn, id)
        .await?
        .ok_or_else(|| Error::new(Status::NotFound, anyhow::anyhow!("no such command")))?;

    // A command id is not a capability: it is scoped to its room, and the guard authorized THIS
    // room. Without this, holding any room's helper role would read every room's commands.
    if row.room_id != access.room.id {
        return Err(Error::new(
            Status::NotFound,
            anyhow::anyhow!("no such command"),
        ));
    }

    Ok(rocket::serde::json::Json(serde_json::json!({
        "id": row.id.to_string(),
        "state": row.state,
        "finished": row.is_finished(),
        "ok": row.result.as_ref().map(|r| r.ok),
        "output": row.result.as_ref().map(|r| r.output.clone()).unwrap_or_default(),
        "error": row.error,
    })))
}

pub fn routes() -> Vec<rocket::Route> {
    routes![show, run, one]
}

#[cfg(test)]
mod tests {
    use super::*;
    use puna_core::model::member::RoomRole;

    fn form(kind: &str) -> CommandForm {
        CommandForm {
            kind: kind.into(),
            slot: None,
            text: None,
            item: None,
            location: None,
            seconds: None,
            amount: None,
            force: false,
            allowed: None,
            locked: None,
            alias: None,
            option_name: None,
            option_value: None,
            reason: None,
        }
    }

    /// A form with every optional field supplied, for asserting that each `kind` builds at all.
    fn filled(kind: &str) -> CommandForm {
        CommandForm {
            slot: Some(1),
            text: Some("x".into()),
            item: Some("x".into()),
            location: Some("x".into()),
            seconds: Some(5),
            amount: Some(5),
            allowed: Some(true),
            locked: Some(true),
            alias: Some("x".into()),
            option_name: Some("hint_cost".into()),
            option_value: Some("20".into()),
            ..form(kind)
        }
    }

    /// A missing field names itself, because the person reading it is mid-task and the alternative
    /// is a generic 400 that says only that something was wrong.
    #[test]
    fn a_missing_field_says_which_one() {
        assert_eq!(build(&form("say")).unwrap_err(), "say what?");
        assert_eq!(build(&form("release")).unwrap_err(), "choose a slot");
        assert_eq!(build(&form("countdown")).unwrap_err(), "how many seconds?");

        let mut hint = form("hint");
        hint.slot = Some(3);
        assert_eq!(build(&hint).unwrap_err(), "name an item");
    }

    /// Whitespace is not a value. `say` with a space in the box is a mistake, not a message.
    #[test]
    fn blank_input_is_treated_as_absent() {
        let mut say = form("say");
        say.text = Some("   ".into());
        assert!(build(&say).is_err());

        // But an omitted OPTIONAL stays omitted rather than becoming an empty string, which pahoa
        // would deliver to the player as a blank reason.
        let mut kick = form("kick");
        kick.slot = Some(3);
        kick.reason = Some("  ".into());
        assert_eq!(
            build(&kick).unwrap(),
            RoomCommand::Kick {
                slot: 3,
                reason: None
            }
        );
    }

    /// Every `<option value>` the console's command menu offers, in the order it offers them.
    ///
    /// Read together they *are* the menu, and both tests below depend on that: one asserts each
    /// builds, the other asserts each is offered to the right tier. A command added to
    /// `console.html` and not here is one nobody checked either way.
    const MENU: &[&str] = &[
        "status",
        "say",
        "countdown",
        "hint",
        "hint_location",
        "release",
        "collect",
        "send_item",
        "send_multiple",
        "send_location",
        "allow_release",
        "alias",
        "kick",
        "lock_slot",
        "option",
    ];

    /// The form covers the whole command set, and nothing else. An unknown `kind` is refused rather
    /// than silently doing nothing.
    #[test]
    fn the_form_builds_every_command_and_no_others() {
        let mut full = form("send_item");
        full.slot = Some(3);
        full.item = Some("Bow".into());
        assert_eq!(
            build(&full).unwrap(),
            RoomCommand::SendItem {
                slot: 3,
                item: "Bow".into()
            }
        );

        for kind in MENU {
            assert!(build(&filled(kind)).is_ok(), "{kind} does not build");
        }

        // Not on the menu -- the room page's password column has its own control for it -- but
        // buildable, because that control and the console share one route.
        assert!(build(&filled("rotate_password")).is_ok());

        assert!(build(&form("drop_database")).is_err());
    }

    /// **The console's menu and the capability table have to agree**, and the one command that is
    /// an organizer's has to be the one the template gates.
    ///
    /// Asserted through the form rather than against the enum, so the route's check and the menu's
    /// contents are covered together. `option` is gated by `{% if is_organizer %}` in
    /// `console.html`; if another command moves tier, this fails and that gate has to move with it.
    #[test]
    fn the_console_offers_option_to_organizers_and_everything_else_to_helpers() {
        let organizer_only: Vec<&str> = MENU
            .iter()
            .filter(|kind| build(&filled(kind)).unwrap().required_role() > RoomRole::Helper)
            .copied()
            .collect();

        assert_eq!(
            organizer_only,
            ["option"],
            "the menu's tiering moved; console.html's `{{% if is_organizer %}}` has to match"
        );

        let template = include_str!("../../templates/rooms/console.html");
        assert!(
            template.contains(r#"<option value="option">"#),
            "the option command left the menu"
        );
        // The gate, and that it is the one *immediately* above the command it gates -- searched
        // backwards from the option rather than forwards from the top, because the template has
        // three `is_organizer` blocks and the naive `rfind` matched the last one on the page,
        // which sits below this and would have passed with the gate deleted.
        let at = template
            .find(r#"<option value="option">"#)
            .expect("checked above");
        let gate = template[..at]
            .rfind("{% if is_organizer %}")
            .expect("the organizer gate is gone from console.html");
        assert!(
            !template[gate..at].contains("{% endif %}"),
            "the option command is offered outside the organizer gate"
        );
    }
}
