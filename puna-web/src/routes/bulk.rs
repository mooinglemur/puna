//! Applying one moderation action to many slots at once.
//!
//! The case is a sync with hundreds of slots, where the roster's per-row controls are the wrong
//! tool: releasing forty worlds one glyph at a time is forty confirmations, and the roster is a
//! table that filters and sorts, so a selection held in it means something different after a
//! re-sort. This is a separate page holding its own selection.
//!
//! ## Its own page rather than an overlay on the room
//!
//! Two lists of hundreds of slots want vertical room, but the deciding reason is the **result**. A
//! bulk action is long-running and partially failing by nature, so its answer needs an address
//! somebody can reload, come back to, or send to a co-organizer. `moderation.js` chose a `<dialog>`
//! over a `popover` because an answer must not light-dismiss; here the answer outlives the whole
//! interaction, which is a stronger version of the same argument.
//!
//! ## Every action here is already a helper's
//!
//! Including the roster's claim release. So this page needs no tier of its own beyond the console's
//! floor — worth stating, because a bulk panel that quietly widened one would be the worst possible
//! place to do it. The per-command check still runs, so a command that changes tier later is caught
//! here by the same expression that catches it everywhere else.
//!
//! ## One action is not a room action at all
//!
//! **Release Claims unbinds `room_slots.owner_id` and never touches the room**, so it does not go
//! through the queue: it is a database write that completes inside the request, and queueing it
//! would put a row in front of the orchestrator that the orchestrator has nothing to do with. It
//! answers with a flash rather than a batch page, and the panel says so.

use puna_core::db::Pool;
use puna_core::ids::BatchId;
use puna_core::model::command::{self, BatchOutcome, RoomCommand, SlotStatus};
use rocket::form::Form;
use rocket::http::Status;
use rocket::request::FlashMessage;
use rocket::response::{Flash, Redirect};
use rocket::{FromForm, State, get, post, routes};

use askama::Template;
use askama_web::WebTemplate;

use crate::error::{Error, Result, forbidden};
use crate::flash::Notice;
use crate::guards::{Helper, RoomAccess};
use crate::params::RoomParam;
use crate::tpl::TplContext;

/// One slot as the two panes render it.
pub struct SlotChoice {
    pub slot_number: i32,
    pub player_name: String,
    pub game: String,
    /// Drives the "Unclaimed slots" selector, and is rendered so an operator can see what they are
    /// about to act on without going back to the roster.
    pub claimed: bool,
    /// The claimant's name, for the "Slots claimed by…" selector's suggestions. Never a raw Discord
    /// id: the roster's own rule, for the same reason.
    pub owner_name: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "rooms/bulk.html")]
pub struct BulkTemplate {
    base: TplContext,
    room_id: String,
    room_name: String,
    room_state: String,
    slots: Vec<SlotChoice>,
    /// Distinct games and claimant names, for the selector's autocomplete. Sent as lists rather
    /// than derived in the browser so the suggestions are the same set the selectors match against.
    games: Vec<String>,
    claimants: Vec<String>,
    /// The rule vocabulary, for the shared `rooms/_rule_table.html`. Sent from the model's own
    /// `ALL` lists so the panel offers exactly what the per-slot editor does.
    directions: Vec<(&'static str, &'static str)>,
    /// See `Vocabulary::kinds` for what each element is.
    kinds: Vec<(
        &'static str,
        &'static str,
        Option<&'static str>,
        &'static str,
    )>,
    tag_suggestions: Vec<&'static str>,
    subtype_suggestions: Vec<&'static str>,
    /// **Always just the blank row, and that is the whole difference from the per-slot editor.**
    /// The panel builds a ruleset to impose rather than editing one that exists, because the staged
    /// slots do not share a filter to start from — showing any one of theirs would be showing a
    /// state most of them are not in.
    rules: Vec<crate::routes::filters::RuleRow>,
    has_rules: bool,
    empty_means_choice: bool,
    empty_choice: Option<&'static str>,
    /// False, unlike the editors: this form carries eight other action buttons and a `required`
    /// control blocks every submit in its form, not only the one it belongs to.
    empty_choice_required: bool,
    /// Whether this room filters at all — the panel says what a slot filter costs only when there
    /// is something to lose, the same rule the per-slot editor follows.
    room_filters: bool,
    notice: Option<Notice>,
}

#[derive(Template, WebTemplate)]
#[template(path = "rooms/batch.html")]
pub struct BatchTemplate {
    base: TplContext,
    /// The sentence the enqueue left behind -- notably the release-claims counts, which are Puna's
    /// own work rather than anything the batch rows record. Dropped until 2026-08-28.
    notice: Option<crate::flash::Notice>,
    room_id: String,
    room_name: String,
    batch_id: String,
    action: String,
    outcome: BatchOutcome,
    rows: Vec<BatchRow>,
}

/// One command's answer, as the result table renders it.
pub struct BatchRow {
    pub slot: Option<i32>,
    pub player_name: String,
    /// `succeeded`, `refused`, `failed` or `outstanding` — the stylesheet keys off it and so does
    /// the reader. **Not the raw `command_state`**, which calls a refusal `ok`.
    pub bucket: &'static str,
    pub lines: Vec<String>,
}

/// What the panel can do, and how each one becomes work.
///
/// A table rather than a match scattered through the route, so adding an action means naming its
/// command here — the same shape `MENU` gives the console.
const ACTIONS: &[(&str, &str)] = &[
    ("rotate_passwords", "Rotate Passwords"),
    ("release_claims", "Release Claims"),
    ("lock", "Lock"),
    ("unlock", "Unlock"),
    ("kick", "Kick"),
    ("release_items", "Release Items"),
    ("collect_items", "Collect Items"),
    ("set_goaled", "Set as Goaled"),
    // **Filters, which are not commands and not the roster write either.** This sets the same state
    // on every staged slot and then queues an `ApplyFilters` per slot so the running room hears
    // about it -- so the batch page reports it exactly like any other bulk action.
    //
    // **One action rather than three.** It used to be Apply / Exempt / Follow, because a single
    // rule was typed into a row of fields and the two ways of saying "no rules" had nowhere to
    // live but their own buttons. With the table here, an empty table IS those two cases and the
    // radios under it are the choice between them -- so the button says what it does and the table
    // says what it does it with, and the panel offers exactly what a slot's own page does.
    ("filter", "Set the filter"),
];

/// Whether an action sets a slot's traffic filter rather than sending a command.
fn is_filter_action(action: &str) -> bool {
    action == "filter"
}

/// The command one action produces for one slot, or `None` when it is not a room action.
fn command_for(action: &str, slot: i32) -> Option<RoomCommand> {
    Some(match action {
        "rotate_passwords" => RoomCommand::RotatePassword { slot },
        "lock" => RoomCommand::LockSlot { slot, locked: true },
        // The same verb pointed the other way, which is why it is a second ACTION rather
        // than a checkbox: the panel's buttons ARE the action, so a direction carried in a
        // field beside them would be one more thing that can disagree with the one pressed.
        "unlock" => RoomCommand::LockSlot {
            slot,
            locked: false,
        },
        "kick" => RoomCommand::Kick { slot, reason: None },
        "release_items" => RoomCommand::Release { slot },
        "collect_items" => RoomCommand::Collect { slot },
        "set_goaled" => RoomCommand::SetStatus {
            slot,
            status: SlotStatus::Goal,
        },
        // `release_claims` is Puna's own roster write, and anything unrecognized is refused by the
        // caller rather than silently doing nothing.
        _ => return None,
    })
}

/// What setting this state did, as a sentence's opening.
///
/// The three cases are genuinely different outcomes rather than three spellings of one, so they are
/// reported as what happened rather than as the label of the button that covers all three.
fn describe_state(state: &puna_core::model::filter::SlotFilter) -> &'static str {
    use puna_core::model::filter::SlotFilter;
    match state {
        SlotFilter::Own(_) => "Filtered",
        SlotFilter::Exempt => "Exempted from every filter",
        SlotFilter::Follows => "Returned to the room's filter",
    }
}

fn label_for(action: &str) -> &str {
    ACTIONS
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, label)| *label)
        .unwrap_or(action)
}

#[get("/room/<_id>/bulk")]
async fn show(
    _id: RoomParam,
    access: RoomAccess<Helper>,
    pool: &State<Pool>,
    flash: Option<FlashMessage<'_>>,
) -> Result<BulkTemplate> {
    let mut conn = pool.get().await?;
    let room = &access.room;

    let rows = puna_core::model::slot::list(&mut conn, room.id).await?;
    // The roster's own resolver, so a claimant is named here exactly as they are named there --
    // including the "never logged in" stand-in, which `is_placeholder` filters out below rather
    // than offering a raw Discord id as an autocomplete suggestion.
    let owners = puna_core::model::slot::owner_names(&mut conn, room.id).await?;

    let slots: Vec<SlotChoice> = rows
        .into_iter()
        .map(|s| SlotChoice {
            slot_number: s.slot_number,
            player_name: s.player_name,
            game: s.game,
            claimed: s.owner_id.is_some(),
            owner_name: s
                .owner_id
                .and_then(|id| owners.get(&id).cloned())
                .filter(|name| !puna_core::model::user::is_placeholder(name)),
        })
        .collect();

    let mut games: Vec<String> = slots.iter().map(|s| s.game.clone()).collect();
    games.sort();
    games.dedup();
    let mut claimants: Vec<String> = slots.iter().filter_map(|s| s.owner_name.clone()).collect();
    claimants.sort();
    claimants.dedup();

    let vocabulary = crate::routes::filters::vocabulary();
    Ok(BulkTemplate {
        base: TplContext::new(access.session.session()),
        room_id: room.id.to_string(),
        room_name: room.name.clone(),
        room_state: room.state.clone(),
        slots,
        games,
        claimants,
        directions: vocabulary.directions,
        kinds: vocabulary.kinds,
        tag_suggestions: vocabulary.tag_suggestions,
        subtype_suggestions: vocabulary.subtype_suggestions,
        rules: crate::routes::filters::editor_rows(&[]),
        has_rules: false,
        empty_means_choice: true,
        empty_choice: None,
        empty_choice_required: false,
        room_filters: !puna_core::model::filter::room_filter(&mut conn, room.id)
            .await?
            .unwrap_or_default()
            .is_empty(),
        notice: Notice::take(flash),
    })
}

#[derive(FromForm)]
pub struct BulkForm {
    action: String,
    /// For `filter`: the same table the per-slot editor renders, read by the same code. Several
    /// rules rather than the one the first cut allowed, so a set built here and a set built on a
    /// slot's own page are the same thing.
    rules: Vec<crate::routes::filters::RuleFields>,
    /// What an empty table means, when it is empty. Not `required` in the markup here, because
    /// this form carries eight other action buttons and a required control blocks all of them.
    state: Option<String>,
    /// The staged slots. A repeated field rather than a delimited string, so the browser and Rocket
    /// agree about the shape without a parser in between.
    slots: Vec<i32>,
}

#[post("/room/<_id>/bulk", data = "<form>")]
async fn apply(
    _id: RoomParam,
    form: Form<BulkForm>,
    access: RoomAccess<Helper>,
    pool: &State<Pool>,
) -> std::result::Result<Flash<Redirect>, Error> {
    let room_id = access.room.id.to_string();
    let back = format!("/room/{room_id}/bulk");

    if form.slots.is_empty() {
        return Ok(Flash::warning(
            Redirect::to(back),
            "Nothing was staged, so nothing was done.",
        ));
    }

    let mut conn = pool.get().await?;

    // **The roster action, done here and finished here.** No queue, no orchestrator, no room: this
    // unbinds an owner and mints a fresh claim token, which is a database write and nothing else.
    if form.action == "release_claims" {
        let mut released = 0usize;
        for slot in &form.slots {
            // **Checked before released**, because `slot::release` mints a fresh claim token
            // whether or not anybody held it -- so releasing an unclaimed slot would silently
            // invalidate a claim link somebody had already been sent.
            let held = puna_core::model::slot::get(&mut conn, access.room.id, *slot)
                .await?
                .is_some_and(|s| s.owner_id.is_some());
            if held {
                puna_core::model::slot::release(&mut conn, access.room.id, *slot).await?;
                released += 1;
            }
        }
        puna_core::model::event::record(
            &mut conn,
            access.room.id,
            puna_core::model::event::Actor::User(access.user_id()),
            "slots_released",
            serde_json::json!({ "slots": form.slots, "released": released }),
        )
        .await?;

        // The difference is worth stating rather than rounding off: a staged slot nobody held was
        // not a failure, it was already in the state being asked for.
        let skipped = form.slots.len() - released;
        return Ok(Flash::success(
            Redirect::to(back),
            // Both sentences written out rather than stitched from four `plural` calls. Past about
            // two agreements the interpolated form stops being readable as a sentence, which is the
            // thing being got right here.
            if skipped == 0 {
                match released {
                    1 => "Released 1 claim. That slot is unclaimed and has a fresh claim link."
                        .to_string(),
                    n => format!(
                        "Released {n} claims. Those slots are unclaimed and have fresh claim links."
                    ),
                }
            } else {
                format!(
                    "Released {}; {skipped} of the staged slots were already unclaimed.",
                    puna_core::text::count(released, "claim"),
                )
            },
        ));
    }

    // **Filters: the same state on every staged slot, then one command per slot so the running room
    // hears it.** Set apart from the command actions above because the durable half is Puna's own
    // tables: the command carries only a scope and the dispatcher reads them back.
    if is_filter_action(&form.action) {
        // **The same table, read by the same code as a slot's own page.** Every staged slot ends up
        // identical, so this REPLACES whatever each of them had rather than merging into it,
        // which is what "identically per affected slot" can honestly promise, since adding to each
        // would leave them in different end states depending on where they started.
        let state =
            match crate::routes::filters::slot_state_from(&form.rules, form.state.as_deref()) {
                Ok(state) => state,
                Err(message) => return Ok(Flash::error(Redirect::to(back), message)),
            };

        for slot in &form.slots {
            puna_core::model::filter::set_slot_filter(
                &mut conn,
                access.room.id,
                *slot,
                &state,
                access.user_id(),
            )
            .await?;
        }
        puna_core::model::event::record(
            &mut conn,
            access.room.id,
            puna_core::model::event::Actor::User(access.user_id()),
            "slot_filter_changed",
            serde_json::json!({ "slots": form.slots, "state": describe_state(&state) }),
        )
        .await?;

        // **What was done, not which button was pressed.** One button now covers three outcomes,
        // and "Set the filter applied to 12 slots" would leave the reader to remember what was in
        // the table -- which is the thing they were about to check.
        let did = format!(
            "{} {}",
            describe_state(&state),
            puna_core::text::count(form.slots.len(), "slot")
        );

        // Nothing to tell a room that is not running: `reapply_filters` asserts everything at the
        // next start, and queueing would only produce rows saying the room is down.
        if access.room.state != "running" {
            return Ok(Flash::success(
                Redirect::to(back),
                format!(
                    "{did}, and stored. This room is not running, so it takes effect the next time \
                     it starts."
                ),
            ));
        }

        let pushes: Vec<RoomCommand> = form
            .slots
            .iter()
            .map(|slot| RoomCommand::ApplyFilters { slot: Some(*slot) })
            .collect();
        let batch = command::enqueue_batch(
            &mut conn,
            access.room.id,
            access.user_id(),
            access.role(),
            &pushes,
        )
        .await?;

        return Ok(match batch {
            Some(batch) => Flash::success(
                Redirect::to(format!("/room/{room_id}/bulk/{batch}")),
                format!("{did}."),
            ),
            None => Flash::warning(
                Redirect::to(back),
                "Nothing was staged, so nothing was done.",
            ),
        });
    }

    let commands: Vec<RoomCommand> = form
        .slots
        .iter()
        .filter_map(|slot| command_for(&form.action, *slot))
        .collect();

    if commands.len() != form.slots.len() {
        return Err(Error::new(
            Status::BadRequest,
            anyhow::anyhow!("no such bulk action"),
        ));
    }

    // The per-command tier, checked once: every command in a batch is the same verb, so one check
    // is the whole check. Kept rather than dropped because a command that changes tier later must
    // be caught here by the same expression that catches it in the console.
    if let Some(command) = commands.first()
        && access.role() < command.required_role()
    {
        return Err(forbidden(
            "that action needs an organizer, and you are a helper here",
        ));
    }

    // **Refused up front rather than queued to be rejected.** The console enqueues against a
    // stopped room and lets the dispatcher answer, which is right for one command and wrong for two
    // hundred: it would produce two hundred `rejected` rows all saying the same thing, and a batch
    // page whose every line is the room being down. One sentence is the better answer.
    //
    // Rotation is the exception, because its durable half is Puna's own: it writes the slot's
    // password and marks the Secret stale, which a start then renders. That is handled below.
    if access.room.state != "running" && form.action != "rotate_passwords" {
        return Ok(Flash::warning(
            Redirect::to(back),
            format!(
                "This room is {}. Bulk actions need a running room, so nothing was done.",
                access.room.state
            ),
        ));
    }

    // **Puna's own half of every command, through the console's single entry point.**
    //
    // Not a rotation special case, which is what this was and what made it wrong: it called only
    // the credential half, so a bulk lock reached pahoa with `room_slots.locked_at` unwritten --
    // no "locked" chip on the roster, and the lock silently gone at the next restart, because that
    // column is what `reapply_locks` re-asserts. `prepare_command` is the one place that decides
    // which side effects a command needs, so a verb that grows a Puna-side half later gets it here
    // for free.
    //
    // A room-level refusal inside it -- a room mid-transition, a rotation outside per-slot mode --
    // fails identically for every slot, so hitting one fails the request on the first command
    // before anything has moved. A later failure would leave the batch half-prepared: recoverable,
    // since every one of these actions is safe to repeat, and rare enough not to be worth nesting a
    // transaction inside `enqueue_batch`'s.
    let mut stored_only = false;
    for command in &commands {
        match crate::routes::console::prepare_command(
            &mut conn,
            &access.room,
            command,
            access.user_id(),
        )
        .await?
        {
            // `NotApplicable` is every passthrough command, and it is not "stored": those need the
            // room, and a room that is not running was refused above.
            crate::routes::console::Prepared::NotApplicable
            | crate::routes::console::Prepared::Live(_) => {}
            _ => stored_only = true,
        }
    }

    // A durable change with no process to tell is not a failure, and queueing it would only
    // produce rows saying the room is down. Same call the console makes for one command.
    if stored_only {
        return Ok(Flash::success(
            Redirect::to(back),
            format!(
                "{} applied to {} and stored. This room is not running, so there was \
                 nothing to tell; it takes effect the next time it starts.",
                label_for(&form.action),
                puna_core::text::count(commands.len(), "slot")
            ),
        ));
    }

    let batch = command::enqueue_batch(
        &mut conn,
        access.room.id,
        access.user_id(),
        // What authorized it, frozen now: the roster can change and "an organizer did this" has to
        // stay true afterwards.
        access.role(),
        &commands,
    )
    .await?;

    match batch {
        Some(batch) => Ok(Flash::success(
            Redirect::to(format!("/room/{room_id}/bulk/{batch}")),
            format!(
                "{} queued for {}.",
                label_for(&form.action),
                puna_core::text::count(commands.len(), "slot")
            ),
        )),
        None => Ok(Flash::warning(
            Redirect::to(back),
            "Nothing was staged, so nothing was done.",
        )),
    }
}

#[get("/room/<_id>/bulk/<batch>")]
async fn results(
    _id: RoomParam,
    batch: &str,
    access: RoomAccess<Helper>,
    pool: &State<Pool>,
    flash: Option<rocket::request::FlashMessage<'_>>,
) -> Result<BatchTemplate> {
    let batch: BatchId = batch
        .parse()
        .map_err(|_| crate::error::not_found("no such batch"))?;

    let mut conn = pool.get().await?;
    let rows = command::batch(&mut conn, access.room.id, batch).await?;
    if rows.is_empty() {
        return Err(crate::error::not_found("no such batch"));
    }

    let names: std::collections::HashMap<i32, String> =
        puna_core::model::slot::list(&mut conn, access.room.id)
            .await?
            .into_iter()
            .map(|s| (s.slot_number, s.player_name))
            .collect();

    let outcome = BatchOutcome::of(&rows);
    let action = rows
        .first()
        .map(|r| r.command.name().to_string())
        .unwrap_or_default();

    let rows = rows
        .iter()
        .map(|row| {
            let mut lines: Vec<String> = row
                .result
                .as_ref()
                .map(|r| r.output.clone())
                .unwrap_or_default();
            if let Some(error) = &row.error {
                lines.push(error.clone());
            }
            BatchRow {
                slot: row.command.target_slot(),
                player_name: row
                    .command
                    .target_slot()
                    .and_then(|n| names.get(&n).cloned())
                    .unwrap_or_default(),
                // **The bucket, not the state.** `command_state` calls a refusal `ok`, which is
                // correct for the queue and misleading for a reader.
                bucket: match row.state.as_str() {
                    "ok" if row.result.as_ref().is_some_and(|r| r.ok) => "succeeded",
                    "ok" => "refused",
                    "failed" | "rejected" => "failed",
                    _ => "outstanding",
                },
                lines,
            }
        })
        .collect();

    Ok(BatchTemplate {
        notice: crate::flash::Notice::take(flash),
        base: TplContext::new(access.session.session()),
        room_id: access.room.id.to_string(),
        room_name: access.room.name.clone(),
        batch_id: batch.to_string(),
        action,
        outcome,
        rows,
    })
}

pub fn routes() -> Vec<rocket::Route> {
    routes![show, apply, results]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every action the panel offers has to become work, or the button does nothing and says so
    /// nowhere. `release_claims` is the one deliberate exception, and it is named rather than
    /// skipped by a rule somebody could widen by accident.
    #[test]
    fn every_offered_action_becomes_a_command_or_is_the_roster_one() {
        for (name, label) in ACTIONS {
            assert!(!label.is_empty(), "{name} has no label");
            // The two shapes that are not `command_for` commands, named rather than skipped by a
            // rule somebody could widen: the roster write, which never reaches the room at all, and
            // the filters, whose durable half is Puna's tables and whose command is built per slot
            // from the stored state rather than from the button.
            if *name == "release_claims" || is_filter_action(name) {
                assert!(
                    command_for(name, 1).is_none(),
                    "{name} must not become a passthrough command"
                );
                continue;
            }
            assert!(
                command_for(name, 1).is_some(),
                "{name} is offered by the panel and becomes no command"
            );
        }
    }

    /// **Every bulk action is a helper's**, the roster one included, so this panel needs no tier of
    /// its own. Asserted as a property rather than trusted: a command promoted to organizer later
    /// would otherwise be offered here to somebody who cannot run it, and refused only on submit.
    #[test]
    fn no_bulk_action_needs_more_than_a_helper() {
        use puna_core::model::member::RoomRole;
        for (name, _) in ACTIONS {
            let Some(command) = command_for(name, 1) else {
                // A filter push is a helper's too -- `ApplyFilters { slot: Some(_) }` -- and that
                // is asserted where the tier lives, in `model::command`.
                continue;
            };
            assert_eq!(
                command.required_role(),
                RoomRole::Helper,
                "{name} is offered on a helper's panel and needs more than a helper"
            );
        }
    }

    /// **Every action's mapping, spelled out.** Four of these carry nothing but a slot, so a
    /// swapped pair compiles, runs, reports success and does the opposite thing: `release_items`
    /// for `collect_items` empties a world instead of filling it, and `unlock` built with
    /// `locked: true` locks the slots an operator was trying to let back in. None of that is
    /// visible in a type, so it is written down.
    #[test]
    fn every_action_maps_to_the_command_it_names() {
        let expected = [
            ("rotate_passwords", RoomCommand::RotatePassword { slot: 3 }),
            (
                "lock",
                RoomCommand::LockSlot {
                    slot: 3,
                    locked: true,
                },
            ),
            (
                "unlock",
                RoomCommand::LockSlot {
                    slot: 3,
                    locked: false,
                },
            ),
            (
                "kick",
                RoomCommand::Kick {
                    slot: 3,
                    reason: None,
                },
            ),
            ("release_items", RoomCommand::Release { slot: 3 }),
            ("collect_items", RoomCommand::Collect { slot: 3 }),
            (
                "set_goaled",
                RoomCommand::SetStatus {
                    slot: 3,
                    status: SlotStatus::Goal,
                },
            ),
        ];

        let named: Vec<&str> = expected.iter().map(|(a, _)| *a).collect();
        for (action, want) in expected {
            assert_eq!(
                command_for(action, 3),
                Some(want),
                "`{action}` builds the wrong command"
            );
        }

        // Every ACTION is covered above, or a new one could be added and mapped wrongly with
        // nothing here noticing.
        for (action, _) in ACTIONS {
            assert!(
                *action == "release_claims" || is_filter_action(action) || named.contains(action),
                "`{action}` is offered and its mapping is not asserted"
            );
        }
    }

    #[test]
    fn an_unknown_action_becomes_nothing() {
        assert!(command_for("delete_everything", 1).is_none());
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::testdb::{ACTOR, insert_generation, insert_room, insert_slot, insert_user, with_db};
    use puna_core::model::{room, slot};

    /// **A bulk lock has to write Puna's own record, not only reach pahoa.**
    ///
    /// This is the bug this test was written for. The panel built `LockSlot` commands and queued
    /// them without ever calling `record_lock`, so the slots locked in the room and
    /// `room_slots.locked_at` stayed `NULL`. Two consequences, and the second is the serious one:
    /// the roster showed no "locked" chip, because `slot_views` reads that column — and
    /// `steps::reapply_locks` re-asserts locks from that column on every transition to `running`,
    /// so the lock would have quietly lapsed at the next restart. pahoa's own copy lives in
    /// `room.save`, which a save reset takes with it.
    ///
    /// Asserted through `prepare_command`, which is the single entry point both routes now share —
    /// so this covers the console's path and the panel's at once, and any command that grows a
    /// Puna-side half later is covered by adding it there rather than here.
    #[tokio::test]
    async fn a_bulk_lock_and_unlock_keep_the_record_puna_is_the_authority_for() {
        with_db(|pool| async move {
            let mut conn = pool.get().await.expect("connection");
            insert_user(&mut conn, ACTOR).await;
            let generation = insert_generation(&mut conn).await;
            let id = insert_room(&mut conn, generation, "running", "per_slot").await;
            for n in 1..=3 {
                insert_slot(&mut conn, id, n, Some("aaaaa-bbbbb")).await;
            }
            let the_room = room::get(&mut conn, id).await.expect("read").expect("room");

            // Exactly what `apply` does for a staged set, through the shared entry point.
            for n in [1, 3] {
                crate::routes::console::prepare_command(
                    &mut conn,
                    &the_room,
                    &RoomCommand::LockSlot {
                        slot: n,
                        locked: true,
                    },
                    ACTOR,
                )
                .await
                .expect("prepare");
            }

            let slots = slot::list(&mut conn, id).await.expect("slots");
            let locked: Vec<i32> = slots
                .iter()
                .filter(|s| s.is_locked())
                .map(|s| s.slot_number)
                .collect();
            assert_eq!(
                locked,
                vec![1, 3],
                "a bulk lock must write `locked_at`, or the roster shows nothing and the next \
                 restart lets them back in"
            );

            // The audit trail names who, which is the half pahoa does not keep at all.
            let by: Vec<Option<i64>> = slots
                .iter()
                .filter(|s| s.is_locked())
                .map(|s| s.locked_by)
                .collect();
            assert!(
                by.iter().all(|who| *who == Some(ACTOR)),
                "the lock records who decided it"
            );

            // **And the mirror, which fails the same way pointing the other direction.** If unlock
            // reached pahoa without clearing `locked_at`, `reapply_locks` would put the lock back
            // on the next transition to `running` -- an operator who unlocked would watch the slot
            // re-lock itself with nothing in the room's history to explain it.
            crate::routes::console::prepare_command(
                &mut conn,
                &the_room,
                &RoomCommand::LockSlot {
                    slot: 1,
                    locked: false,
                },
                ACTOR,
            )
            .await
            .expect("unlock");

            let after = slot::list(&mut conn, id).await.expect("slots");
            let still_locked: Vec<i32> = after
                .iter()
                .filter(|s| s.is_locked())
                .map(|s| s.slot_number)
                .collect();
            assert_eq!(
                still_locked,
                vec![3],
                "unlock must clear the record, or the next start locks them again"
            );
        })
        .await;
    }
}
