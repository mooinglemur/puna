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
    /// A command and a slot chosen in advance, from the room page's moderation controls.
    ///
    /// **This is the whole of the no-JavaScript path for that column.** Those controls are links
    /// here, so somebody who followed one arrives with the command and the player already picked
    /// and only the value left to fill in. `kind` is checked against [`MENU`] before it is
    /// rendered, because it comes out of a URL.
    preselect_kind: Option<String>,
    preselect_slot: Option<i32>,
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
#[get("/room/<_id>/console?<ran>&<pending>&<stored>&<uncertain>&<kind>&<slot>")]
#[allow(clippy::too_many_arguments)]
async fn show(
    _id: RoomParam,
    ran: Option<String>,
    pending: Option<bool>,
    stored: Option<bool>,
    uncertain: Option<bool>,
    kind: Option<String>,
    slot: Option<i32>,
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
        // **Validated against the menu, not echoed.** These arrive in a URL, and a `<option
        // selected>` rendered from an unchecked query parameter is arbitrary text chosen by
        // whoever wrote the link -- the same shape M17b's flash cookie replaced `?notice=` to
        // avoid. An unknown value selects nothing, which is what a bare console does anyway.
        preselect_kind: kind.filter(|k| MENU.contains(&k.as_str())),
        preselect_slot: slot,
        is_organizer: access.role() >= puna_core::model::member::RoomRole::Organizer,
    })
}

/// Every `<option value>` the console's command menu offers, in the order it offers them.
///
/// Read together they *are* the menu, and three things depend on that: the tests below assert each
/// builds and that each is offered to the right tier, and [`show`] uses it to decide whether a
/// `?kind=` in a URL names a real command before rendering it as selected. A command added to
/// `console.html` and not here is one nobody checked either way, and one the moderation column
/// cannot link to.
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
    "lock",
    "set_status",
    "option",
];

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
    /// For `lock`, and the same reasoning as `allowed`.
    locked: Option<bool>,
    /// For `set_status`. **No default**, though the only one anybody reaches for is `goal`:
    /// defaulting it would let a malformed request declare somebody's game finished, and goal
    /// cannot be undone by anyone including the player.
    status: Option<String>,
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
        "lock" => RoomCommand::LockSlot {
            slot: slot()?,
            locked: form.locked.ok_or_else(|| "lock, or unlock?".to_string())?,
        },
        "set_status" => RoomCommand::SetStatus {
            slot: slot()?,
            // Parsed against the enum rather than passed through, so a typo is refused here with a
            // list rather than at the far end as "unknown status" — and so the vocabulary has one
            // definition. `goal` is the one anybody reaches for and is still not a default.
            status: form
                .status
                .as_deref()
                .and_then(puna_core::model::command::SlotStatus::parse)
                .ok_or_else(|| {
                    format!(
                        "which status? one of {}",
                        puna_core::model::command::SlotStatus::ALL
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?,
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
) -> Result<Prepared> {
    use puna_core::model::room::SlotAuth;
    use puna_core::model::{room, slot};

    let RoomCommand::RotatePassword { slot: slot_number } = command else {
        return Ok(Prepared::NotApplicable);
    };
    let slot_number = *slot_number;

    // The two states where a rotation is expressible. Everything else -- `starting`, `stopping`,
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
    // no such thing as this slot's password, so there is nothing to rotate.
    if room.slot_auth != SlotAuth::PerSlot {
        return Err(crate::error::not_found(
            "this room does not use per-slot passwords",
        ));
    }

    if slot::get(conn, room.id, slot_number).await?.is_none() {
        return Err(crate::error::not_found("no such slot"));
    }

    slot::rotate_password(conn, room.id, slot_number).await?;
    room::mark_secret_stale(conn, room.id).await?;
    let kind = "slot_password_rotated";

    if room.state == "running" {
        return Ok(Prepared::Live(kind));
    }

    // **Read the state back**, because the one above came from the request guard and a room can
    // begin starting in between. `start` renders the Secret from the row, so a start that read the
    // row before this write produces a pod running the old map with the row saying otherwise --
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

/// Record that staff barred a slot, or let it back in.
///
/// **Separate from [`prepare_slot_credential`] because locking is no longer a credential
/// operation.** pahoa owns the verb now: `lock` is an ordinary passthrough command that reaches the
/// running room and nothing else, so there is no Secret to write, no ordering to get right, and no
/// per-slot mode requirement -- it works in every password mode.
///
/// What stays on this side is the **intent and the audit trail**. pahoa persists the lock in
/// `room.save`, which goes with a save that is reset or a PVC that is recreated, and it records
/// only the fact rather than who asked. `room_slots.locked_at` / `locked_by` is therefore the
/// authority, and the orchestrator re-applies it whenever a room reaches `running`.
///
/// Returns the `room_events` kind, or `None` when the row did not move -- a repeat click, which
/// should not write an event claiming something changed.
async fn record_lock(
    conn: &mut diesel_async::AsyncPgConnection,
    room: &puna_core::model::room::Room,
    command: &RoomCommand,
    by: i64,
) -> Result<Option<&'static str>> {
    use puna_core::model::slot;

    let RoomCommand::LockSlot { slot: n, locked } = command else {
        return Ok(None);
    };

    if slot::get(conn, room.id, *n).await?.is_none() {
        return Err(crate::error::not_found("no such slot"));
    }
    if !slot::set_locked(conn, room.id, *n, *locked, by).await? {
        return Ok(None);
    }

    Ok(Some(if *locked {
        "slot_locked"
    } else {
        "slot_unlocked"
    }))
}

/// What a command run answers with: a page, or the same outcome as JSON.
///
/// **One route rather than two**, so the tier check, the preparation and the wait exist once. The
/// moderation controls on the room page ask for JSON and render the answer in a dialog; the console
/// form asks for a page and gets a redirect, which is also what happens with no scripting at all.
#[derive(rocket::Responder)]
pub enum Ran {
    Json(rocket::serde::json::Json<serde_json::Value>),
    Redirect(Box<Redirect>),
    Failed(Error),
}

/// Turn a refusal into something the dialog can show.
///
/// **Only for statuses below 500.** The [`Error`] responder deliberately sends no body at all,
/// because an `anyhow` chain from a database failure can name tables, columns and connection
/// strings — and every such error arrives through `From`, which always builds a `500`. A 4xx here
/// is always hand-built with a message written for the person reading it, so the two cases are
/// distinguishable and only the authored one is repeated back.
fn refusal_as_json(error: &Error) -> serde_json::Value {
    let message = if error.status.code < 500 {
        error.source.to_string()
    } else {
        "Something went wrong on our side. The command was not run.".to_string()
    };
    serde_json::json!({
        "ok": false,
        "pending": false,
        "heading": "Refused",
        "lines": [message],
    })
}

/// Queue a command and wait for the answer.
#[post("/room/<id>/command", data = "<form>")]
async fn run(
    id: RoomParam,
    form: Form<CommandForm>,
    access: RoomAccess<Helper>,
    pool: &State<Pool>,
    waiters: &State<std::sync::Arc<Waiters>>,
    wants_json: crate::guards::WantsJson,
) -> Ran {
    match run_inner(id, form, access, pool, waiters, wants_json.0).await {
        Ok(ran) => ran,
        // Rendered here rather than propagated, because the `?` in `run_inner` would otherwise
        // hand a JSON caller a bare status with no body and the dialog would have nothing to say.
        Err(error) if wants_json.0 => {
            let body = refusal_as_json(&error);
            // Logged the way the responder would have, since that path is being skipped.
            if error.status.code >= 500 {
                tracing::error!(error = ?error.source, "command request failed");
            } else {
                tracing::debug!(error = %error.source, "command request rejected");
            }
            Ran::Json(rocket::serde::json::Json(body))
        }
        Err(error) => Ran::Failed(error),
    }
}

async fn run_inner(
    _id: RoomParam,
    form: Form<CommandForm>,
    access: RoomAccess<Helper>,
    pool: &State<Pool>,
    waiters: &State<std::sync::Arc<Waiters>>,
    wants_json: bool,
) -> Result<Ran> {
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
    let prepared = prepare_slot_credential(&mut conn, &access.room, &command).await?;

    // **Locking writes Puna's record and then travels on as an ordinary command.** Unlike a
    // rotation there is nothing to prepare -- pahoa's `lock` needs no Secret and no password mode --
    // but the intent belongs here, because pahoa's copy lives in a save that a reset would take with
    // it and records nothing about who asked.
    let locked_kind = record_lock(&mut conn, &access.room, &command, access.user_id()).await?;

    if let Some(kind) = prepared.kind().or(locked_kind) {
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
            return Ok(if wants_json {
                answer(serde_json::json!({
                    "ok": true,
                    "pending": false,
                    "heading": "Stored",
                    "lines": ["This room is not running, so there was nothing to tell. \
                              The change takes effect the next time it starts."],
                }))
            } else {
                page(format!("/room/{room}/console?stored=true"))
            });
        }
        Prepared::Uncertain(_) => {
            return Ok(if wants_json {
                answer(serde_json::json!({
                    // **Not `ok`.** The dialog draws this as a warning because that is what it is:
                    // the change is recorded and may not be in force, and the operator has to look.
                    "ok": false,
                    "pending": false,
                    "heading": "Recorded, but the room started underneath it",
                    "lines": ["The room began starting while this was being applied, so it may not \
                              be in force. Check the slot now that the room is up, and run it \
                              again if it did not take."],
                }))
            } else {
                page(format!("/room/{room}/console?uncertain=true"))
            });
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

    let finished = commands::wait_for(pool, waiters.inner(), id).await;
    if !wants_json {
        return Ok(match finished {
            Some(_) => page(format!("/room/{room}/console?ran={id}")),
            // Out of budget. The command is still running and the row is still readable, so this
            // is a slower answer rather than a lost one.
            None => page(format!("/room/{room}/console?ran={id}&pending=true")),
        });
    }

    Ok(answer(match finished {
        Some(row) => {
            // **A refusal is an answer, not a failure**, and the dialog has to show which. A room
            // that understood and said no lands in `ok` with `output` explaining why; retrying it
            // would loop, so the operator is told rather than offered another go.
            let succeeded = row.state == "ok" && row.result.as_ref().is_some_and(|r| r.ok);
            let mut lines: Vec<String> = row
                .result
                .as_ref()
                .map(|r| r.output.clone())
                .unwrap_or_default();
            if let Some(error) = &row.error {
                lines.push(error.clone());
            }
            if lines.is_empty() {
                // pahoa's own phrasing is what an organizer expects to read, so this only stands in
                // when there was none -- a terse `{"ok": true}` is a legal answer.
                lines.push(if succeeded {
                    "Done.".into()
                } else {
                    format!("The room answered {}.", row.state)
                });
            }
            serde_json::json!({
                "ok": succeeded,
                "pending": false,
                "heading": if succeeded { "Done" } else { "The room said no" },
                "lines": lines,
                "command": id.to_string(),
            })
        }
        None => serde_json::json!({
            "ok": false,
            "pending": true,
            "heading": "Still running",
            "lines": ["This is taking longer than usual. It has not been lost — it will appear in \
                      the room's command history when it finishes."],
            "command": id.to_string(),
        }),
    }))
}

fn answer(body: serde_json::Value) -> Ran {
    Ran::Json(rocket::serde::json::Json(body))
}

fn page(to: String) -> Ran {
    Ran::Redirect(Box::new(Redirect::to(to)))
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

/// Item or location names from the cached datapackage, for the moderation controls' autocomplete.
///
/// ## Why this exists at all
///
/// **pahoa matches exactly, never fuzzily** — the caller is a program, so a near miss should be a
/// visible error rather than a silent decision to act on something else. That is the right rule and
/// it makes a text box hostile: an operator typing "Progressive Sword " gets a refusal and no idea
/// which character was wrong. Suggestions turn an exact-match API into something a person can drive.
///
/// ## Scoped to the target slot's own game, which is also the correct game
///
/// Not a convenience: it is the resolution rule M16 transcribed from the reference. An item sent to
/// or hinted for a slot resolves in **that slot's** game, and a location in that slot's own world
/// likewise — so the one game this reads is the one game the command will be interpreted in.
/// Offering the whole seed's names would suggest things the room will refuse.
///
/// ## Disclosure
///
/// A game's datapackage is public knowledge — it ships with the world, not with the seed — and it
/// carries no information about *this* multiworld: not what is where, not who holds what. It is
/// `Helper`-guarded regardless, because that is the tier the controls it feeds belong to and there
/// is no reason to widen it.
#[get("/room/<_id>/slot/<n>/names?<kind>&<q>")]
async fn slot_names(
    _id: RoomParam,
    n: i32,
    kind: &str,
    q: Option<String>,
    access: RoomAccess<Helper>,
    pool: &State<Pool>,
    cache: &State<crate::routes::tracker::NameCache>,
) -> Result<rocket::serde::json::Json<serde_json::Value>> {
    /// Enough to choose from, few enough that the list stays a list. A datalist the length of a
    /// game's item table is the same problem as no suggestions at all.
    const LIMIT: usize = 20;

    let mut conn = pool.get().await?;
    let slot = puna_core::model::slot::get(&mut conn, access.room.id, n)
        .await?
        .ok_or_else(|| crate::error::not_found("no such slot"))?;

    let games = crate::routes::tracker::names_for(&mut conn, cache, access.room.generation_id)
        .await
        .map_err(|e| Error::new(Status::InternalServerError, anyhow::anyhow!(e.to_string())))?;

    // An absent game is an empty list, not an error: a generation ingested before the name cache
    // existed has no rows, and the operator can still type the name. Failing here would turn a
    // cosmetic gap into a control that looks broken.
    let table = games.get(&slot.game).map(|names| match kind {
        "location" => &names.locations,
        _ => &names.items,
    });

    let query = q.unwrap_or_default().trim().to_lowercase();
    let mut matches: Vec<&str> = Vec::new();
    if let Some(table) = table {
        // **Prefix matches first**, because somebody typing "Prog" wants the items that start that
        // way before the ones that merely contain it. Two passes rather than a sort with a key:
        // the tables run to thousands of entries and this walks each at most twice.
        for pass in [true, false] {
            for name in table.values() {
                if matches.len() >= LIMIT {
                    break;
                }
                let lower = name.to_lowercase();
                let hit = if pass {
                    lower.starts_with(&query)
                } else {
                    !lower.starts_with(&query) && lower.contains(&query)
                };
                if hit {
                    matches.push(name);
                }
            }
        }
    }

    Ok(rocket::serde::json::Json(serde_json::json!({
        "game": slot.game,
        "names": matches,
    })))
}

pub fn routes() -> Vec<rocket::Route> {
    routes![show, run, one, slot_names]
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
            status: None,
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
            status: Some("goal".into()),
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

    use super::MENU;

    /// **The moderation column names its commands in three places, and all three must agree.**
    ///
    /// A control there carries `data-command` for the script, a `kind=` in its `href` for the
    /// no-script path, and needs an entry in `moderation.js`'s `COMMANDS` table. Every mismatch is
    /// silent in its own way, which is why this reads all three rather than trusting review:
    ///
    /// * a `data-command` that is not in [`MENU`] posts a `kind` the form refuses, and the link it
    ///   falls back to arrives at a console with nothing selected;
    /// * an `href` naming a different command than `data-command` makes the scripted and
    ///   unscripted paths do **different things**, which is the worst of the three because both
    ///   work;
    /// * a command with no `COMMANDS` entry hits `if (!spec) return` and the glyph does nothing at
    ///   all — no dialog, no navigation, no error.
    #[test]
    fn the_moderation_column_agrees_with_the_command_set_and_the_script() {
        let page = include_str!("../../templates/rooms/show.html");
        let script = include_str!("../../static/moderation.js");

        let mut found = 0;
        for (at, _) in page.match_indices("data-command=\"") {
            found += 1;
            let rest = &page[at + "data-command=\"".len()..];
            let command = rest.split('"').next().expect("a closing quote");

            assert!(
                MENU.contains(&command),
                "the moderation column offers {command:?}, which the console form cannot build"
            );

            // The anchor's own href, which is the whole no-script path. Bounded to this element by
            // stopping at the tag's end rather than scanning into the next one.
            let element = rest.split('>').next().unwrap_or_default();
            assert!(
                element.contains(&format!("kind={command}&amp;")),
                "the {command:?} control links to a different command than it posts:\n{element}"
            );

            // The script's table. `COMMANDS` is keyed by the bare command name.
            assert!(
                script.contains(&format!("{command}: {{")),
                "moderation.js has no entry for {command:?}, so that glyph silently does nothing"
            );
        }

        assert!(
            found >= 9,
            "only {found} moderation controls found -- this lint is no longer looking at anything"
        );
    }

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
            template.contains(r#"<option value="option""#),
            "the option command left the menu"
        );
        // The gate, and that it is the one *immediately* above the command it gates -- searched
        // backwards from the option rather than forwards from the top, because the template has
        // three `is_organizer` blocks and the naive `rfind` matched the last one on the page,
        // which sits below this and would have passed with the gate deleted.
        let at = template
            .find(r#"<option value="option""#)
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

// --- the credential rule, against a real database ---------------------------------------------
//
// `prepare_slot_credential` is the piece of the console with the most reasoning behind it and no
// way to assert any of it without a room in a given state. Every branch is a statement about what
// the operator is told, and the failure mode of each is quiet: a lock that was dropped, a lock
// claimed to be in force when it is not, or a room left unable to start.
#[cfg(test)]
mod credential_tests {
    use super::*;
    use crate::testdb::{
        ACTOR, insert_generation, insert_room, insert_slot, insert_user, secret_is_stale, with_db,
    };
    use diesel_async::RunQueryDsl;
    use puna_core::model::room;
    use puna_core::model::slot;

    async fn a_room(conn: &mut diesel_async::AsyncPgConnection, state: &str) -> room::Room {
        insert_user(conn, ACTOR).await;
        let generation = insert_generation(conn).await;
        let id = insert_room(conn, generation, state, "per_slot").await;
        insert_slot(conn, id, 1, Some("aaaaa-bbbbb")).await;
        insert_slot(conn, id, 2, Some("ccccc-ddddd")).await;
        room::get(conn, id).await.expect("read").expect("the room")
    }

    fn lock(slot: i32, locked: bool) -> RoomCommand {
        RoomCommand::LockSlot { slot, locked }
    }

    fn rotate(slot: i32) -> RoomCommand {
        RoomCommand::RotatePassword { slot }
    }

    /// **A room in transition is refused, and NOTHING is written.**
    ///
    /// The rule Troy gave, and the reason it is a refusal rather than either confident answer: a
    /// `starting` pod may already have read the old password map and is not yet answering, so
    /// "it worked" may be false and "it takes effect at the next start" is false by construction —
    /// the start it refers to is the one already in flight. Failing is an acceptable answer;
    /// claiming a lock is in force when it may not be is not.
    ///
    /// The half-written case is what makes "nothing is written" worth asserting separately: a
    /// refusal that had already locked the row would leave the row and the pod disagreeing with
    /// nothing to reconcile them.
    #[tokio::test]
    async fn a_room_in_transition_refuses_and_changes_nothing() {
        with_db(|pool| async move {
            let mut conn = pool.get().await.expect("connection");

            for state in ["starting", "stopping", "degraded", "provisioning"] {
                let room = a_room(&mut conn, state).await;

                let refused = prepare_slot_credential(&mut conn, &room, &rotate(1))
                    .await
                    .expect_err("a room in transition must refuse");
                assert_eq!(refused.status, Status::Conflict, "{state}");
                assert!(
                    refused.source.to_string().contains(state),
                    "the refusal does not say which state the room is in: {refused}"
                );

                let untouched = slot::get(&mut conn, room.id, 1)
                    .await
                    .expect("read")
                    .expect("slot");
                assert!(
                    !untouched.is_locked(),
                    "{state}: the row was written anyway"
                );
                assert!(
                    !secret_is_stale(&mut conn, room.id).await,
                    "{state}: the Secret was marked stale by a refused change"
                );
            }
        })
        .await;
    }

    /// A room at rest has no process to tell, and that is not a failure: a start renders the Secret
    /// from the row, so the change is guaranteed to be in force when it comes up.
    #[tokio::test]
    async fn a_room_at_rest_stores_the_change_for_its_next_start() {
        with_db(|pool| async move {
            let mut conn = pool.get().await.expect("connection");

            for state in ["idle", "failed"] {
                let room = a_room(&mut conn, state).await;
                let prepared = prepare_slot_credential(&mut conn, &room, &rotate(1))
                    .await
                    .expect("prepared");

                assert_eq!(
                    prepared,
                    Prepared::Stored("slot_password_rotated"),
                    "{state}"
                );
                assert!(secret_is_stale(&mut conn, room.id).await, "{state}");
            }
        })
        .await;
    }

    /// **The narrow window the confident answers cannot cover.**
    ///
    /// The guard read the room before the write; if it began starting in between, `start` may have
    /// rendered the Secret from the row as it was. Constructed exactly as it happens: the caller
    /// holds an `idle` snapshot while the row now says `starting`, which is what the function
    /// compares.
    ///
    /// It must not report `Stored` — that would promise the change takes effect at a start which
    /// has already begun.
    #[tokio::test]
    async fn a_room_that_starts_underneath_the_change_is_reported_as_uncertain() {
        with_db(|pool| async move {
            let mut conn = pool.get().await.expect("connection");
            let stale_snapshot = a_room(&mut conn, "idle").await;

            // The room starts between the guard's read and the write.
            diesel::sql_query("UPDATE rooms SET state = 'starting' WHERE id = $1")
                .bind::<diesel::sql_types::Uuid, _>(stale_snapshot.id)
                .execute(&mut conn)
                .await
                .expect("the room starts");

            let prepared = prepare_slot_credential(&mut conn, &stale_snapshot, &rotate(1))
                .await
                .expect("prepared");

            assert_eq!(prepared, Prepared::Uncertain("slot_password_rotated"));
            // Written, and marked stale: the change is real, it is only its timing that is unknown.
            assert!(secret_is_stale(&mut conn, stale_snapshot.id).await);
        })
        .await;
    }

    /// **Locking records intent and nothing else.** No Secret, no password mode, no ordering — it is
    /// an ordinary passthrough command now, and the row is what makes the intent durable across a
    /// save that pahoa loses.
    #[tokio::test]
    async fn locking_records_the_intent_and_leaves_the_secret_alone() {
        with_db(|pool| async move {
            let mut conn = pool.get().await.expect("connection");
            let room = a_room(&mut conn, "running").await;

            let kind = record_lock(&mut conn, &room, &lock(1, true), ACTOR)
                .await
                .expect("recorded");
            assert_eq!(kind, Some("slot_locked"));

            let stored = slot::get(&mut conn, room.id, 1)
                .await
                .expect("read")
                .expect("slot");
            assert!(stored.is_locked());
            assert_eq!(stored.locked_by, Some(ACTOR));
            assert!(
                stored.password.is_some(),
                "locking must not disturb the credential -- the two stopped being the same thing"
            );
            assert!(
                !secret_is_stale(&mut conn, room.id).await,
                "locking asked for a Secret rewrite, which it no longer has any reason to do"
            );

            // A repeat is a no-op and must not write an event claiming something changed.
            assert_eq!(
                record_lock(&mut conn, &room, &lock(1, true), ACTOR)
                    .await
                    .expect("recorded"),
                None
            );

            assert_eq!(
                record_lock(&mut conn, &room, &lock(1, false), ACTOR)
                    .await
                    .expect("recorded"),
                Some("slot_unlocked")
            );
        })
        .await;
    }

    /// **In every password mode**, which is the whole reason for adopting pahoa's verb: the
    /// omission trick it replaced needed per-slot mode to be in force.
    #[tokio::test]
    async fn locking_works_whatever_the_rooms_password_mode_is() {
        with_db(|pool| async move {
            let mut conn = pool.get().await.expect("connection");
            insert_user(&mut conn, ACTOR).await;

            for mode in ["none", "room", "per_slot"] {
                let generation = insert_generation(&mut conn).await;
                let id = insert_room(&mut conn, generation, "running", mode).await;
                let password = (mode == "per_slot").then_some("aaaaa-bbbbb");
                insert_slot(&mut conn, id, 1, password).await;
                let room = room::get(&mut conn, id).await.expect("read").expect("room");

                assert_eq!(
                    record_lock(&mut conn, &room, &lock(1, true), ACTOR)
                        .await
                        .unwrap_or_else(|e| panic!("{mode}: {e}")),
                    Some("slot_locked"),
                    "{mode}: locking was refused"
                );
            }
        })
        .await;
    }

    /// A rotation still refuses everything but per-slot mode, and that asymmetry is the point: a
    /// password mode is what a rotation acts *within*, where a lock is about access.
    #[tokio::test]
    async fn rotation_still_needs_a_password_mode_to_rotate_within() {
        with_db(|pool| async move {
            let mut conn = pool.get().await.expect("connection");
            insert_user(&mut conn, ACTOR).await;
            let generation = insert_generation(&mut conn).await;
            let id = insert_room(&mut conn, generation, "running", "none").await;
            insert_slot(&mut conn, id, 1, None).await;
            let room = room::get(&mut conn, id).await.expect("read").expect("room");

            let refused = prepare_slot_credential(&mut conn, &room, &rotate(1))
                .await
                .expect_err("must refuse");
            assert_eq!(refused.status, Status::NotFound);
        })
        .await;
    }

    /// Rotation travels the same path, and the value never reaches the queue: the orchestrator
    /// reads it from the row, which is what keeps a credential out of the audit trail.
    #[tokio::test]
    async fn rotation_replaces_the_password_and_marks_the_secret_stale() {
        with_db(|pool| async move {
            let mut conn = pool.get().await.expect("connection");
            let room = a_room(&mut conn, "running").await;

            let prepared =
                prepare_slot_credential(&mut conn, &room, &RoomCommand::RotatePassword { slot: 1 })
                    .await
                    .expect("prepared");

            assert_eq!(prepared, Prepared::Live("slot_password_rotated"));
            let rotated = slot::get(&mut conn, room.id, 1)
                .await
                .expect("read")
                .expect("slot");
            assert_ne!(rotated.password.as_deref(), Some("aaaaa-bbbbb"));
            assert!(secret_is_stale(&mut conn, room.id).await);
        })
        .await;
    }

    /// Everything else passes through untouched — this function must not have opinions about the
    /// twelve commands that are pahoa's.
    #[tokio::test]
    async fn an_ordinary_command_is_left_alone() {
        with_db(|pool| async move {
            let mut conn = pool.get().await.expect("connection");
            // `starting`, which the credential commands refuse: a passthrough must not inherit
            // that rule, since a room that is not running rejects the command later with a message
            // that offers a Start button.
            let room = a_room(&mut conn, "starting").await;

            let prepared =
                prepare_slot_credential(&mut conn, &room, &RoomCommand::Release { slot: 1 })
                    .await
                    .expect("prepared");

            assert_eq!(prepared, Prepared::NotApplicable);
            assert!(!secret_is_stale(&mut conn, room.id).await);
        })
        .await;
    }
}
