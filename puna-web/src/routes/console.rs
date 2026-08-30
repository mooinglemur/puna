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
    /// here, so somebody who followed one arrives with the command marked and the player already
    /// picked, and only the value left to fill in. `kind` is checked against [`MENU`] before it is
    /// used, because it comes out of a URL.
    ///
    /// The links also carry a `#cmd-<kind>` fragment, which is what actually *scrolls* to the form
    /// on a page of fifteen. The two are not redundant: the fragment moves the viewport and never
    /// reaches the server, and this marks which form is the one — so a link opened in a new tab, a
    /// bookmark saved without the fragment, or a browser that declines to scroll all still arrive
    /// somewhere legible.
    preselect_kind: Option<String>,
    preselect_slot: Option<i32>,
    /// The room's own rules, flattened for `rooms/_gameplay_options.html`. **Read from the room's
    /// last probe, never from Puna's configuration** — see [`room::gameplay_option_rows`].
    ///
    /// The field names here and on the options page have to match, because the include reads them
    /// out of whichever context it is rendered in.
    gameplay_options: Vec<(String, String)>,
    gameplay_options_at: Option<String>,
}

impl ConsoleTemplate {
    /// `" chosen"` for the form the moderation column sent us to, empty for every other.
    ///
    /// **The leading space is inside the Rust string on purpose.** Written in the template as
    /// `class="cmd {% if … %}chosen{% endif %}"` the separator sits next to a tag, and `askama.toml`
    /// sets `whitespace = "suppress"` — so it would be eaten and the class would render as
    /// `cmdchosen`, which matches no rule and fails silently. Returning the space with the word
    /// keeps it out of the template's reach entirely.
    fn chosen(&self, kind: &str) -> &'static str {
        if self.preselect_kind.as_deref() == Some(kind) {
            " chosen"
        } else {
            ""
        }
    }
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
        gameplay_options: puna_core::model::room::gameplay_option_rows(
            room.gameplay_options.as_ref(),
        ),
        gameplay_options_at: probe_stamp(room),
    })
}

/// When the room last answered a probe, spelled the way the console's history table spells a time.
///
/// **Paired with the rules rather than offered on its own**, and only where there are rules to
/// date: it is the difference between "these are the room's rules" and "these were its rules the
/// last time anybody managed to ask", which for a stopped room is the whole of what can honestly be
/// said. `None` where the room has never answered, in which case there is nothing to stamp.
///
/// Plain UTC rather than the `data-at` shorthand `localtime.js` renders, because neither page that
/// uses this loads that script and a duration is the wrong shape here anyway — "17 hours ago" reads
/// as staleness where a room that has simply been stopped since yesterday is not stale, it is off.
pub fn probe_stamp(room: &puna_core::model::room::Room) -> Option<String> {
    room.probed_at
        .map(|at| at.format("%Y-%m-%d %H:%M:%S UTC").to_string())
}

/// Every command the console offers, in the order the page lays them out.
///
/// Read together they *are* the console, and three things depend on that: the tests below assert
/// each builds and that each is offered to the right tier, and [`show`] uses it to decide whether a
/// `?kind=` in a URL names a real command before highlighting it. A command added to `console.html`
/// and not here is one nobody checked either way, and one the moderation column cannot link to.
/// [`the_console_and_the_template_offer_the_same_commands`] holds the two together, order included.
///
/// **The order is the page's two groups**, room-wide first and per-slot after. That division is the
/// one an operator actually navigates by, and it is also the honest statement of what the console
/// is still *for*: everything in the second group has a control on the room's own roster, where the
/// slot is already in front of you, and nothing in the first group has one anywhere else.
///
/// **`send_multiple` is deliberately absent, and it is still reachable.** It is not a command an
/// operator picks — it is `send_item` with a number beside it — so it is a field on that command
/// rather than a second entry describing the same act. [`build`] chooses the verb from the count.
///
/// [`the_console_and_the_template_offer_the_same_commands`]:
///     tests::the_console_and_the_template_offer_the_same_commands
const MENU: &[&str] = &[
    // The room itself.
    "status",
    "say",
    "countdown",
    "option",
    // One slot.
    "hint",
    "hint_location",
    "send_location",
    "send_item",
    "collect",
    "release",
    "set_status",
    "alias",
    "allow_release",
    "lock",
    "kick",
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
    /// How many copies `send_item` sends. **One when absent**, which is the only default on this
    /// form and is safe for the same reason the others are not: it is the smallest thing the
    /// command can do, so a request that lost this field under-sends rather than over-sends.
    ///
    /// The old spelling was a `send_multiple` command with **no** default, on the grounds that a
    /// command quietly sending one copy would look like it had worked. That reasoning was right
    /// about a menu entry called "send multiple" and does not survive folding the two together:
    /// here the field sits beside the item on the one control that sends items, so one copy is
    /// what the operator asked for rather than a fraction of it.
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
        // **One control, two verbs, and the count is what chooses.** pahoa keeps `send_item` and
        // `send_multiple` apart and so does `RoomCommand`; what is folded here is the *asking*,
        // because "send this item" and "send this item five times" are one decision with a number
        // in it and were two menu entries describing the same act.
        //
        // One copy stays `SendItem` rather than becoming `SendMultiple { amount: 1 }`, so nothing
        // about the wire moves for the command anybody actually runs -- `send_item` is the
        // one-copy spelling, as `RoomCommand::SendMultiple`'s own doc says.
        "send_item" => {
            // Bounded here as well as by pahoa, so the answer to a typo is a sentence rather than a
            // round trip -- and the limit is named, because "too many" without the number is the
            // kind of error that gets guessed at twice.
            let amount = match form.amount {
                Some(amount) if (1..=100).contains(&amount) => amount,
                Some(amount) => {
                    return Err(format!("{amount} is not between 1 and 100 copies"));
                }
                None => 1,
            };
            if amount == 1 {
                RoomCommand::SendItem {
                    slot: slot()?,
                    item: item()?,
                }
            } else {
                RoomCommand::SendMultiple {
                    slot: slot()?,
                    item: item()?,
                    amount,
                }
            }
        }
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

/// Everything **Puna itself** must write before a command is queued, and the audit row for it.
///
/// **The single entry point, and it is single because it was not.** The console called
/// [`prepare_slot_credential`] and [`record_lock`] one after the other; the bulk panel reimplemented
/// that sequence and called only the first, so a bulk lock reached pahoa and left
/// `room_slots.locked_at` unwritten. The visible symptom was a roster with no "locked" chip. The
/// worse one was invisible: that column is what `steps::reapply_locks` re-asserts on every
/// transition to `running`, so those slots would have quietly let their holders back in at the next
/// restart -- pahoa's own copy lives in `room.save`, which a save reset takes with it.
///
/// So this is not tidying. **Two callers deciding independently which side effects a command needs
/// is the bug**, and one function they both go through is the fix: a command that grows a Puna-side
/// half later gets it in both places or neither.
///
/// Returns the credential [`Prepared`] state, which is the one thing a caller still has to branch
/// on -- a rotation against a room that is not running has landed durably and must not be queued.
pub(crate) async fn prepare_command(
    conn: &mut diesel_async::AsyncPgConnection,
    room: &puna_core::model::room::Room,
    command: &RoomCommand,
    by: i64,
) -> Result<Prepared> {
    // Rotation: writes the slot's password and marks the Secret stale, so the orchestrator has
    // something to push. Refuses outright against a room mid-transition.
    let prepared = prepare_slot_credential(conn, room, command).await?;

    // Locking: writes Puna's record of intent and then travels on as an ordinary command. Nothing
    // to prepare -- pahoa's `lock` needs no Secret and no password mode -- but the intent belongs
    // here, because pahoa's copy records neither who asked nor survives a save reset.
    let locked_kind = record_lock(conn, room, command, by).await?;

    if let Some(kind) = prepared.kind().or(locked_kind) {
        puna_core::model::event::record(
            conn,
            room.id,
            puna_core::model::event::Actor::User(by),
            kind,
            // The slot, never the value. This row is read by anyone who can read the room's history.
            serde_json::json!({ "slot": command.target_slot() }),
        )
        .await?;
    }

    Ok(prepared)
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

    let prepared = prepare_command(&mut conn, &access.room, &command, access.user_id()).await?;

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
    ///   all — no dialog, no navigation, no error;
    /// * a `#cmd-…` fragment naming no form leaves the no-script path at the top of a page of
    ///   fifteen forms, with the right one somewhere below the fold and nothing having failed.
    #[test]
    fn the_moderation_column_agrees_with_the_command_set_and_the_script() {
        let page = include_str!("../../templates/rooms/show.html");
        let script = include_str!("../../static/moderation.js");
        let console = include_str!("../../templates/rooms/console.html");

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

            // And the fragment, which is what actually moves the viewport once the console is a
            // column of fifteen forms. A bad one is the quietest failure of the four: the browser
            // simply does not scroll, so the page looks like it ignored the link.
            assert!(
                element.contains(&format!("#cmd-{command}\"")),
                "the {command:?} control does not point at its own form on the console:\n{element}"
            );
            assert!(
                console.contains(&format!("id=\"cmd-{command}\"")),
                "the moderation column anchors at #cmd-{command}, which console.html does not render"
            );

            // The script's table. `COMMANDS` is keyed by the bare command name.
            assert!(
                script.contains(&format!("{command}: {{")),
                "moderation.js has no entry for {command:?}, so that glyph silently does nothing"
            );
        }

        assert!(
            found >= 12,
            "only {found} moderation controls found -- this lint is no longer looking at anything"
        );
    }

    /// **The console and [`MENU`] are one list written twice**, so they are checked against each
    /// other rather than reviewed.
    ///
    /// Drift is silent in both directions and differently each way. A command in `MENU` with no
    /// form is one the console cannot run, while `?kind=` still claims to mark it — so a moderation
    /// link for it lands on a page where nothing is highlighted and nothing is wrong. A form
    /// missing from `MENU` is worse: it still builds and still runs, so the console works, and the
    /// only casualty is that `show` filters that `kind` out and the link's highlight vanishes.
    ///
    /// **Order is asserted, not just membership**, because the order *is* the page's two groups —
    /// room-wide, then per-slot. A command that drifts into the wrong group renders under a heading
    /// that says the opposite of what it does, and "these also live on the roster" becomes false
    /// for something that has no control there.
    ///
    /// Written for the removal of `send_multiple`, which had to leave both lists together.
    #[test]
    fn the_console_and_the_template_offer_the_same_commands() {
        let template = include_str!("../../templates/rooms/console.html");

        // Each form declares its command in one hidden field, which is also what the browser posts
        // -- so this reads the same string the route will act on rather than a label beside it.
        let offered: Vec<&str> = template
            .match_indices(r#"<input type="hidden" name="kind" value=""#)
            .map(|(at, m)| {
                template[at + m.len()..]
                    .split('"')
                    .next()
                    .expect("a closing quote")
            })
            .collect();

        assert_eq!(
            offered, MENU,
            "console.html's commands and MENU have parted company, in content or in order"
        );

        // Every form is anchorable, which is what the moderation column's `#cmd-…` fragments need.
        // Asserted here as well as from the column's side, so a command nothing links to yet still
        // gets its id rather than growing one the day somebody adds the link.
        for kind in MENU {
            assert!(
                template.contains(&format!("id=\"cmd-{kind}\"")),
                "the {kind:?} form has no id, so nothing can link to it"
            );
        }
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

        // `send_multiple` is no longer a `kind` anybody can post -- it left the menu when it became
        // a field on this command -- so the form refuses the old spelling outright rather than
        // keeping a second door onto the same act.
        assert!(build(&filled("send_multiple")).is_err());

        // Not on the menu -- the room page's password column has its own control for it -- but
        // buildable, because that control and the console share one route.
        assert!(build(&filled("rotate_password")).is_ok());

        assert!(build(&form("drop_database")).is_err());
    }

    /// **The copies field chooses the verb**, which is the whole of what folding `send_multiple`
    /// into `send_item` means.
    ///
    /// Both directions are asserted because both fail quietly. One copy arriving as
    /// `SendMultiple { amount: 1 }` would work — pahoa accepts it — and would move every ordinary
    /// send onto the other verb, so the command history, the metrics and the journal would all stop
    /// saying `send_item` with nothing visibly wrong. Several copies arriving as `SendItem` would
    /// send **one** and report success, which is the failure the old no-default rule existed to
    /// prevent and is the thing a default has to be checked against.
    #[test]
    fn the_number_of_copies_decides_which_send_command_this_is() {
        let send = |amount: Option<i64>| {
            let mut f = form("send_item");
            f.slot = Some(3);
            f.item = Some("Bow".into());
            f.amount = amount;
            build(&f)
        };

        let one = RoomCommand::SendItem {
            slot: 3,
            item: "Bow".into(),
        };
        // Absent and 1 are the same request. Absent is what the no-script console posts if the
        // field is ever cleared, and what an old bookmarked link carries.
        assert_eq!(send(None).unwrap(), one);
        assert_eq!(send(Some(1)).unwrap(), one);

        assert_eq!(
            send(Some(5)).unwrap(),
            RoomCommand::SendMultiple {
                slot: 3,
                item: "Bow".into(),
                amount: 5
            }
        );

        // pahoa's cap, answered here so a typo is a sentence rather than a round trip. Zero and
        // negatives fall out of the same range: a request for no copies is not a send.
        assert!(send(Some(101)).is_err());
        assert!(send(Some(0)).is_err());
        assert!(send(Some(-1)).is_err());
    }

    fn a_console(preselect_kind: Option<&str>, is_organizer: bool) -> ConsoleTemplate {
        ConsoleTemplate {
            base: TplContext::new(&crate::auth::Session::default()),
            room_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            room_name: "A room".into(),
            room_state: "running".into(),
            slots: vec![(1, "Alice".into()), (2, "Bob".into()), (3, "Carol".into())],
            history: Vec::new(),
            outcome: None,
            still_running: false,
            stored: false,
            uncertain: false,
            is_organizer,
            preselect_kind: preselect_kind.map(str::to_string),
            preselect_slot: Some(3),
            gameplay_options: vec![
                ("hint_cost".to_string(), "10".to_string()),
                ("release_mode".to_string(), "auto".to_string()),
            ],
            gameplay_options_at: Some("2026-08-30 12:00:00 UTC".to_string()),
        }
    }

    /// **The page is fifteen forms, and every one of them has to be a whole form.**
    ///
    /// The lints above read the template as text, which is what catches a command that drifts out of
    /// one list or the other. None of them renders it — and every failure this test covers survives
    /// a text scan intact:
    ///
    /// * a `{% call slot_picker(…) %}` that silently produced nothing would leave a form posting no
    ///   slot at all, which `build()` refuses with "choose a slot" for a command the operator plainly
    ///   chose a slot for;
    /// * `self.chosen()` losing its leading space renders `class="cmdchosen"`, which matches no rule,
    ///   so the marked form is simply not marked — the exact failure the helper's doc comment
    ///   describes, asserted rather than argued;
    /// * the organizer gate is checked as source above; here it is checked as *output*, which is the
    ///   thing that actually reaches a helper.
    #[test]
    fn the_console_renders_one_form_per_command_and_marks_the_chosen_one() {
        let html = a_console(Some("alias"), true).render().expect("renders");

        // One form per command, plus none besides. Counted through the hidden field rather than
        // `<form`, because that is the thing that has to be one-per-command.
        assert_eq!(
            html.matches(r#"name="kind""#).count(),
            MENU.len(),
            "the page does not carry exactly one command field per command"
        );
        for kind in MENU {
            assert!(
                html.contains(&format!("id=\"cmd-{kind}\"")),
                "{kind} did not render"
            );
        }

        // The chosen form, with the space that separates the two classes intact.
        assert!(
            html.contains(r#"class="cmd chosen" id="cmd-alias""#),
            "the command the roster linked to is not marked"
        );
        assert_eq!(
            html.matches("cmd chosen").count(),
            1,
            "more than one form is marked as the one that was linked to"
        );
        assert!(
            !html.contains("cmdchosen"),
            "the chosen class lost the space before it, so it matches no rule"
        );

        // The slot picker rendered, once per per-slot command, with the linked slot preselected in
        // each. Eleven of the fifteen commands take a slot.
        let slot_pickers = html.matches(r#"<select name="slot""#).count();
        assert_eq!(
            slot_pickers, 11,
            "the slot picker did not render everywhere"
        );
        assert_eq!(
            html.matches(r#"<option value="3" selected>"#).count(),
            slot_pickers,
            "the slot the roster linked to is not preselected in every picker"
        );

        // The three choices that are radios rather than dropdowns, which is what makes each option's
        // consequence readable without opening anything.
        for group in ["allowed", "locked", "status"] {
            assert!(
                html.contains(&format!(r#"type="radio" name="{group}""#)),
                "{group} is not a radio group"
            );
            assert!(
                !html.contains(&format!(r#"<select name="{group}""#)),
                "{group} is still a dropdown"
            );
        }

        // A helper gets fourteen and never sees the fifteenth.
        let helper = a_console(None, false).render().expect("renders");
        assert!(
            !helper.contains(r#"value="option""#),
            "a helper is offered option"
        );
        assert_eq!(helper.matches(r#"name="kind""#).count(), MENU.len() - 1);
        assert!(
            !helper.contains("cmd chosen"),
            "nothing was linked to, yet something is marked"
        );
    }

    /// **The room's rules are shown to everybody who can reach the console, not only to whoever may
    /// change them.**
    ///
    /// The natural mistake is to render the values inside the organizer gate, beside the form —
    /// they belong together on screen, and the form is organizer-only. But changing a rule is an
    /// organizer's and *knowing what the rules are* is not: a helper fielding "why did my release
    /// do nothing" is answering a question about `release_mode`, and making them find an organizer
    /// to read a value aloud is the helper tier being useless rather than careful.
    ///
    /// Also that the reading is dated. The console keeps its last one for a stopped room, which is
    /// the whole reason these are stored rather than fetched per page load, and an undated table
    /// would present a week-old answer as the current one.
    #[test]
    fn the_rooms_rules_render_for_a_helper_as_well_as_an_organizer() {
        for is_organizer in [true, false] {
            let html = a_console(None, is_organizer).render().expect("renders");
            assert!(
                html.contains(r#"id="room-options""#),
                "the rules section is missing for is_organizer={is_organizer}"
            );
            for fragment in ["hint_cost", "release_mode", "auto"] {
                assert!(
                    html.contains(fragment),
                    "{fragment} is not shown for is_organizer={is_organizer}"
                );
            }
            assert!(
                html.contains("2026-08-30 12:00:00 UTC"),
                "the reading is undated for is_organizer={is_organizer}"
            );
        }
    }

    /// A room that has never answered says so, rather than rendering an empty table.
    ///
    /// **Absent is not empty**, and the difference is the whole content of the sentence: a room with
    /// no rules is not a thing that exists, so a blank table would state something false. What this
    /// actually means is that nobody has managed to ask — a room that has never run, or one on an
    /// image too old to answer.
    #[test]
    fn a_room_that_has_never_reported_its_rules_says_so() {
        let mut page = a_console(None, true);
        page.gameplay_options = Vec::new();
        page.gameplay_options_at = None;
        let html = page.render().expect("renders");

        assert!(
            html.contains("has not reported its rules yet"),
            "an unreported room renders no explanation"
        );
        assert!(
            !html.contains("<table class=\"gameplay-options\">"),
            "an empty rules table rendered, which says a room has no rules"
        );
    }

    /// **The console's commands and the capability table have to agree**, and the one command that
    /// is an organizer's has to be the one the template gates.
    ///
    /// Asserted through the form rather than against the enum, so the route's check and the page's
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
            "the tiering moved; console.html's `{{% if is_organizer %}}` has to match"
        );

        // The hidden field, not the heading: that is the string the browser posts, so a gate that
        // stops covering it is a gate that stops mattering.
        let template = include_str!("../../templates/rooms/console.html");
        let marker = r#"<input type="hidden" name="kind" value="option">"#;
        assert!(
            template.contains(marker),
            "the option command left the page"
        );

        // The gate, and that it is the one *immediately* above the form it gates -- searched
        // backwards from the marker rather than forwards from the top, because the template has
        // several `is_organizer` blocks and a naive `rfind` matched the last one on the page, which
        // sits below this and would have passed with the gate deleted.
        let at = template.find(marker).expect("checked above");
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
