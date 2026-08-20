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
use puna_core::model::member::RoomRole;
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
    /// What the caller may do, so the form offers only what they can run rather than letting them
    /// press a button that answers 403.
    is_organizer: bool,
    /// The room's slots, for the target picker. A dropdown rather than a number field: a mistyped
    /// slot number is a release into somebody else's game.
    slots: Vec<(i32, String)>,
    history: Vec<HistoryEntry>,
    /// The result of the command just submitted, if this is the redirect after one.
    outcome: Option<HistoryEntry>,
    /// Set when the command outlived the request budget. **Not an error**: it is still running, and
    /// the history pane will show it.
    still_running: bool,
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
#[get("/room/<_id>/console?<ran>&<pending>")]
async fn show(
    _id: RoomParam,
    ran: Option<String>,
    pending: Option<bool>,
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
        is_organizer: access.role() >= RoomRole::Organizer,
        slots,
        history: history.iter().map(HistoryEntry::from_row).collect(),
        outcome,
        still_running: pending.unwrap_or(false),
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
    seconds: Option<i64>,
    #[field(default = false)]
    force: bool,
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
        "hint" => RoomCommand::Hint {
            slot: slot()?,
            item: item()?,
            force: form.force,
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

    let room = access.room.id.to_string();
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

    fn form(kind: &str) -> CommandForm {
        CommandForm {
            kind: kind.into(),
            slot: None,
            text: None,
            item: None,
            seconds: None,
            force: false,
            reason: None,
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

        for kind in [
            "status",
            "say",
            "countdown",
            "release",
            "collect",
            "send_item",
            "hint",
            "kick",
        ] {
            let mut f = form(kind);
            f.slot = Some(1);
            f.text = Some("x".into());
            f.item = Some("x".into());
            f.seconds = Some(5);
            assert!(build(&f).is_ok(), "{kind} does not build");
        }

        assert!(build(&form("rotate_password")).is_err(), "not a command");
        assert!(build(&form("drop_database")).is_err());
    }

    /// The tier split, asserted through the form so the route's check has something to check.
    #[test]
    fn helper_commands_and_organizer_commands_are_separated() {
        let helper_only = ["status", "say", "countdown", "hint"];
        for kind in helper_only {
            let mut f = form(kind);
            f.slot = Some(1);
            f.text = Some("x".into());
            f.item = Some("x".into());
            f.seconds = Some(5);
            assert_eq!(
                build(&f).unwrap().required_role(),
                RoomRole::Helper,
                "{kind} moved tier"
            );
        }

        for kind in ["release", "collect", "send_item", "kick"] {
            let mut f = form(kind);
            f.slot = Some(1);
            f.item = Some("x".into());
            assert_eq!(
                build(&f).unwrap().required_role(),
                RoomRole::Organizer,
                "{kind} moved tier"
            );
        }
    }
}
