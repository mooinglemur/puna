//! Editing traffic filters: the room's, and one slot's.
//!
//! ## Two scopes, two tiers
//!
//! A **room-wide** filter changes what every player in the room experiences and persists into the
//! save, so it outlives whoever set it — which is the `option` argument almost word for word, and
//! `option` is the one organizer-only command. A **per-slot** filter is one slot's traffic, which
//! is the `lock` argument, so it is a helper's.
//!
//! That is more restrictive on the room half than pahoa suggested ("something at or above
//! `RoomRole::Helper`"), deliberately: thinning one noisy slot's DeathLinks is day-to-day
//! moderation, and thinning everybody's is a room setting.
//!
//! ## The two warnings, which are the whole reason this is not just a form
//!
//! pahoa replaces rather than merges, and Puna does not paper over that — so both directions of the
//! consequence have to be said at the moment of editing, because neither is visible in the rule
//! being typed:
//!
//! * **Editing the ROOM's filter** does not reach any slot that has a state of its own. Those slots
//!   are listed by name, because "this change affects 197 of 200 slots" is the fact and a silent
//!   success is not.
//! * **Giving a SLOT its own rules** stops the room's rules reaching it, and **removing them** makes
//!   the room's apply at once — a subtraction that adds something, which is the more surprising of
//!   the two.
//!
//! ## One table, saved in one go
//!
//! The first cut added and removed one rule per POST, with a separate pair of buttons for the two
//! meanings of an empty ruleset. It worked and it was clunky: three rules meant three round trips,
//! and the state controls sat in a different form from the rules they were an alternative to.
//!
//! Now the whole ruleset is one table, edited in place and applied together. Two things fall out of
//! that, and the second is the interesting one:
//!
//! * **An empty table has to be told apart**, for a slot. No rules means either "follows the room"
//!   or "exempt from everything", and those are opposites — so when the table empties, the page
//!   asks which, and refuses to save until it is answered. That question used to be two buttons
//!   that were reachable whatever the table held; now it is asked exactly when it is ambiguous.
//! * **Nothing here needs JavaScript.** A blank row is always rendered, so a rule can be added per
//!   save; a row's remove button is a submit carrying its own `rules[N].remove` field, so the
//!   clicked row names itself with no index plumbing; and the disabled state of the tag and subtype
//!   cells is rendered by the server from the same `Kind::narrows_with` the script reads. What the
//!   script adds is several rows at once, removal without a round trip, and the unsaved-changes
//!   notice.

use puna_core::db::Pool;
use puna_core::model::filter::{self, Direction, Effective, Kind, Rule, SlotFilter, Subject};
use rocket::form::Form;
use rocket::request::FlashMessage;
use rocket::response::{Flash, Redirect};
use rocket::{FromForm, State, get, post, routes};

use askama::Template;
use askama_web::WebTemplate;

use crate::error::{Error, Result};
use crate::flash::Notice;
use crate::guards::{Helper, Organizer, RoomAccess};
use crate::params::RoomParam;
use crate::tpl::TplContext;

/// One rule as the page **states** it: the sentence that says what it does.
///
/// Used for what is in force rather than for what is being edited — the editor is a table of
/// fields, and this is the prose above it.
pub struct RuleView {
    /// The effect in words. **Never the bare probability** — `p` is the fraction dropped and the
    /// opposite reading is equally natural, so the number alone invites whichever meaning the
    /// reader arrived with.
    pub describes: String,
}

/// **The subject comes from the scope, not from the rule.** A room-wide rule described as "sent by
/// this slot" reads as though one slot had been singled out, on a page with no slot on it — and the
/// room's page names its own exceptions underneath, so "any slot" is not an overclaim there.
fn views(rules: &[Rule], subject: Subject) -> Vec<RuleView> {
    rules
        .iter()
        .map(|rule| RuleView {
            describes: rule.describe(subject),
        })
        .collect()
}

/// One rule as the editor **renders** it: one row of form fields.
///
/// Shared with the bulk panel through `rooms/_rule_table.html`, so both pages offer the same knobs
/// spelled the same way.
pub struct RuleRow {
    /// The `rules[N]` index. Only ever has to be distinct between neighbors — Rocket starts a new
    /// element when the index changes — but it is distinct throughout, so removing a row never
    /// renumbers the ones after it.
    pub index: usize,
    pub direction: &'static str,
    pub kind: &'static str,
    pub tag: String,
    pub subtype: String,
    /// The percentage **dropped**, as text, so a rule with no `p` renders an empty cell rather than
    /// a `100` nobody typed.
    pub percent: String,
    /// Whether this row's kind is narrowed by a tag, and by a subtype. The cell for the one that
    /// does not apply is rendered **disabled**, which is not decoration: a disabled input is not
    /// submitted, so a tag left over from when the row was a bounce cannot ride along.
    pub tag_enabled: bool,
    pub subtype_enabled: bool,
    /// The blank row at the end, which is what makes adding a rule work with no script — and which
    /// `filters.js` clones when it adds more.
    ///
    /// **Built here rather than written a second time in the template.** The two were separate
    /// markup at first, differing only in a `value=` and which cells started disabled, and a
    /// mutation proved the cost immediately: renaming a field in one of them left the other
    /// spelling it correctly, so the lint checking the name still passed while every existing row
    /// posted a field the route does not read.
    pub is_new: bool,
}

/// `p` as a percentage, without the float noise.
///
/// `0.07 * 100.0` is `7.000000000000001` and `0.29 * 100.0` is `28.999999999999996` — a cell
/// reading either is a cell somebody retypes, and a value that walks a little further from where it
/// started every time the form is saved. Rounded to four decimal places, which is finer than any
/// percentage a person enters and coarse enough to absorb the representation error.
///
/// **Most values do not show it**, which is why the test names those two: `0.75 * 100.0` is exactly
/// `75.0`, so a round-trip test built from round numbers passes against no rounding at all.
fn percent_text(p: Option<f64>) -> String {
    match p {
        None => String::new(),
        Some(p) => {
            let percent = (p * 100.0 * 10_000.0).round() / 10_000.0;
            format!("{percent}")
        }
    }
}

/// The stored rules as editable rows, **plus the blank one at the end**.
///
/// Shared with the bulk panel, which passes no rules and gets the blank row alone.
pub(crate) fn editor_rows(rules: &[Rule]) -> Vec<RuleRow> {
    let mut rows: Vec<RuleRow> = rules
        .iter()
        .enumerate()
        .map(|(index, rule)| RuleRow {
            index,
            direction: rule.direction.as_str(),
            kind: rule.kind.as_str(),
            tag: rule.tag.clone().unwrap_or_default(),
            subtype: rule.subtype.clone().unwrap_or_default(),
            percent: percent_text(rule.p),
            tag_enabled: rule.kind == Kind::Bounce,
            subtype_enabled: rule.kind == Kind::PrintJson,
            is_new: false,
        })
        .collect();

    rows.push(RuleRow {
        index: rules.len(),
        // No kind chosen yet, so the row names nothing to drop and the route skips it entirely —
        // an untouched blank row costs nothing. **Both narrowing cells stay enabled**, because the
        // row is not a rule yet and disabling a field before knowing whether it applies would leave
        // somebody unable to type the tag they came to type.
        direction: "",
        kind: "",
        tag: String::new(),
        subtype: String::new(),
        percent: String::new(),
        tag_enabled: true,
        subtype_enabled: true,
        is_new: true,
    });
    rows
}

/// A slot this room's filter will not reach, for the room editor's warning.
pub struct MissedSlot {
    pub slot_number: i32,
    pub player_name: String,
    /// `has its own rules` or `is exempt from everything` — opposite facts, and a warning that said
    /// only "diverges" would leave an operator unable to tell which.
    pub because: &'static str,
}

#[derive(Template, WebTemplate)]
#[template(path = "rooms/filter.html")]
pub struct FilterTemplate {
    base: TplContext,
    room_id: String,
    room_name: String,
    /// What this page edits, in words: `Room filter`, or `Filter for slot 3 (Troy)`. It is rendered
    /// **inside the `<h1>`**, because a slot named only in the dimmed line under the room name is a
    /// slot readers do not see.
    ///
    /// It is also the `<title>`, with the room name after it — so it carries no trailing
    /// punctuation of its own and nothing that would read as a separator twice.
    scope: String,
    /// `None` for the room's own filter; the slot number otherwise. The template branches on it for
    /// every difference between the two scopes, so there is one page rather than two that drift.
    slot: Option<i32>,
    /// What is stored at this scope, as editable rows, plus the blank one.
    rules: Vec<RuleRow>,
    /// Whether any rule is stored. **Not `rules.is_empty()`** — that list always carries the blank
    /// row, so asking it would say "this table has rules" about an empty editor and hide the
    /// question an empty table exists to ask.
    has_rules: bool,
    /// A slot's state, as one of three words. Absent for the room, which has only two.
    slot_state: Option<&'static str>,
    /// What actually applies here, which for a following slot is the room's rules.
    effective: Vec<RuleView>,
    effective_from_room: bool,
    /// The room's rules, for the slot editor's two warnings.
    room_rules: Vec<RuleView>,
    /// Slots the room's filter does not reach, for the room editor's warning.
    missed: Vec<MissedSlot>,
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
    /// `(wire value, the name a person reads, why it cannot be filtered)`.
    refused: Vec<(&'static str, &'static str, &'static str)>,
    /// Whether an empty table is a question (a slot) or a statement (a room).
    empty_means_choice: bool,
    /// Which answer to that question is already true, so a state somebody already chose is not
    /// asked again — and a table just emptied by hand, which is the genuinely open case, is.
    empty_choice: Option<&'static str>,
    /// Whether the radios carry `required`. True here, because this form has one submit button.
    empty_choice_required: bool,
    notice: Option<Notice>,
}

/// One row of the rule table.
///
/// Every field is optional because a blank trailing row is always rendered and an untouched one has
/// to cost nothing.
///
/// `remove` comes from the row's remove **button**, which is a submit carrying this field as its own
/// `name`/`value` — only the clicked submit contributes those, so the pressed row names itself and
/// nothing has to carry an index. With scripting the button never submits: the row leaves the table
/// and simply is not in the next save.
#[derive(FromForm, Default)]
pub struct RuleFields {
    direction: Option<String>,
    kind: Option<String>,
    tag: Option<String>,
    subtype: Option<String>,
    /// The percentage **dropped**, 0–100, because a percentage is what the label says and a
    /// fraction is what pahoa takes. Empty means always.
    percent: Option<String>,
    remove: bool,
}

/// The whole table, plus what an empty one means.
#[derive(FromForm)]
pub struct FilterForm {
    rules: Vec<RuleFields>,
    /// `follow` or `exempt`, for a slot whose table came back empty. Absent otherwise.
    state: Option<String>,
}

fn blank(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Build one rule from loose parts, naming what is wrong rather than answering a bare 400.
fn build_rule(fields: &RuleFields) -> std::result::Result<Rule, String> {
    let kind_name = blank(&fields.kind).ok_or_else(|| "choose what to drop".to_string())?;

    // **Refused by name, with pahoa's reason.** These parse and are things an operator genuinely
    // reaches for while trying to help a broken client, so "unknown kind" would send them looking
    // for a spelling mistake instead of telling them why it cannot work.
    if let Some((_, label, why)) = filter::REFUSED_KINDS
        .iter()
        .find(|(name, _, _)| *name == kind_name)
    {
        return Err(format!("{label} cannot be filtered: {why}"));
    }

    let kind = Kind::parse(&kind_name).ok_or_else(|| format!("no such kind: {kind_name}"))?;
    let direction = blank(&fields.direction)
        .and_then(|d| Direction::parse(&d))
        .ok_or_else(|| "choose a direction".to_string())?;

    // A percentage on the form, a fraction on the wire. The label says "% dropped" so the number
    // typed and the number stored mean the same thing to the person typing it.
    let p = match blank(&fields.percent) {
        None => None,
        Some(text) => {
            let percent: f64 = text
                .parse()
                .map_err(|_| format!("\"{text}\" is not a number"))?;
            if !(0.0..=100.0).contains(&percent) {
                return Err(format!("{percent} is not a percentage between 0 and 100"));
            }
            Some(percent / 100.0)
        }
    };

    let rule = Rule {
        direction,
        kind,
        tag: blank(&fields.tag),
        subtype: blank(&fields.subtype),
        p,
    };
    rule.validate()?;
    Ok(rule)
}

/// Whether a row is one nobody filled in.
///
/// **`direction` is deliberately not consulted.** It is a `<select>` with no blank option, so the
/// browser always submits one — a row is untouched when it names nothing to drop and carries no
/// narrowing or probability.
fn is_untouched(fields: &RuleFields) -> bool {
    blank(&fields.kind).is_none()
        && blank(&fields.tag).is_none()
        && blank(&fields.subtype).is_none()
        && blank(&fields.percent).is_none()
}

/// Turn a submitted table into the ruleset it describes.
///
/// **Shared with the bulk panel**, which renders the same table and must read it the same way — a
/// second reader would be a second set of refusals and a second place for the percentage-to-fraction
/// conversion to be got backwards.
///
/// Rows are numbered as the operator sees them, from 1, including the blank and struck-out ones:
/// an error naming row 4 has to mean the fourth row on the page.
pub(crate) fn collect_rules(rules: &[RuleFields]) -> std::result::Result<Vec<Rule>, String> {
    let mut collected: Vec<Rule> = Vec::new();

    for (position, fields) in rules.iter().enumerate() {
        let row = position + 1;
        if fields.remove {
            continue;
        }
        // **A half-filled row is an error, never a silent skip.** Somebody who typed a tag and
        // forgot the kind has written a rule they expect to exist, and dropping it quietly is the
        // one outcome that looks like success.
        if is_untouched(fields) {
            continue;
        }

        let rule = build_rule(fields).map_err(|why| format!("row {row}: {why}"))?;

        // pahoa keys rules on the matcher, so two rows naming the same thing are one rule there —
        // and the page would go on showing a rule the room does not have. Refused rather than
        // collapsed, because which of the two probabilities survived would be anybody's guess.
        let matcher = rule.matcher();
        if let Some(earlier) = collected.iter().position(|r| r.matcher() == matcher) {
            return Err(format!(
                "row {row} matches the same thing as row {} — the room keeps one rule per match, \
                 so give them different tags or subtypes, or remove one",
                earlier + 1
            ));
        }
        collected.push(rule);
    }

    Ok(collected)
}

/// What a slot's submission means, including the answer to an empty table.
///
/// **Shared with the bulk panel**, which renders the same table and the same pair of radios — so
/// "no rules" is read the same way whether it was typed for one slot or for two hundred.
pub(crate) fn slot_state_from(
    rules: &[RuleFields],
    state: Option<&str>,
) -> std::result::Result<SlotFilter, String> {
    let rules = collect_rules(rules)?;
    if !rules.is_empty() {
        return Ok(SlotFilter::Own(rules));
    }
    // The two meanings of nothing, and they are opposites — so this is asked rather than assumed.
    match state.map(str::trim).filter(|s| !s.is_empty()) {
        Some("follow") => Ok(SlotFilter::Follows),
        Some("exempt") => Ok(SlotFilter::Exempt),
        _ => Err(
            "with no rules, say whether this slot follows the room's filter or is exempt from \
                  every filter"
                .to_string(),
        ),
    }
}

/// The rule vocabulary a form offers: what pahoa accepts, and what it refuses by name.
///
/// Built from the model's own `ALL` lists rather than restated in the template, so a kind pahoa
/// gains appears in the picker by being added once.
pub(crate) struct Vocabulary {
    pub directions: Vec<(&'static str, &'static str)>,
    /// `(wire value, the name a person reads, what it narrows with, the directions it can travel)`.
    ///
    /// The second is [`Kind::label`] — a picker offering `print_json` names it in a spelling only
    /// pahoa's filter API uses, where every client log and the protocol document say `PrintJSON`.
    ///
    /// The last is space-separated, and it is what stops the editor building a rule that can never
    /// match: most kinds travel one way only, so offering both directions was offering a rule pahoa
    /// answers `400` to.
    pub kinds: Vec<(
        &'static str,
        &'static str,
        Option<&'static str>,
        &'static str,
    )>,
    pub tag_suggestions: Vec<&'static str>,
    pub subtype_suggestions: Vec<&'static str>,
    pub refused: Vec<(&'static str, &'static str, &'static str)>,
}

pub(crate) fn vocabulary() -> Vocabulary {
    Vocabulary {
        directions: Direction::ALL
            .iter()
            .map(|d| (d.as_str(), d.label()))
            .collect(),
        kinds: Kind::ALL
            .iter()
            .map(|k| (k.as_str(), k.label(), k.narrows_with(), k.travels_text()))
            .collect(),
        tag_suggestions: filter::BOUNCE_TAGS.to_vec(),
        subtype_suggestions: filter::PRINT_JSON_SUBTYPES.to_vec(),
        refused: filter::REFUSED_KINDS.to_vec(),
    }
}

/// Tell the running room, if there is one.
///
/// **The web tier cannot reach a room pod at all**, so this queues `ApplyFilters` and the
/// orchestrator does the pushing — the same shape a password rotation takes, and for the same
/// reason. The command carries only the scope; the dispatcher reads the tables this request has
/// just written, so the queue row cannot hold a ruleset that disagrees with the stored one.
///
/// A room that is not running is told nothing and that is not a failure: `reapply_filters` asserts
/// everything at the next start, so the durable half has already landed. Queueing anyway would
/// produce a `rejected` row saying the room is down — true, and nothing to act on.
async fn tell_the_room(
    conn: &mut diesel_async::AsyncPgConnection,
    room: &puna_core::model::room::Room,
    role: puna_core::model::member::RoomRole,
    by: i64,
    slot: Option<i32>,
) -> Result<bool> {
    if room.state != "running" {
        return Ok(false);
    }
    puna_core::model::command::enqueue(
        conn,
        room.id,
        by,
        role,
        &puna_core::model::command::RoomCommand::ApplyFilters { slot },
    )
    .await?;
    Ok(true)
}

#[get("/room/<_id>/filter")]
async fn show_room(
    _id: RoomParam,
    access: RoomAccess<Organizer>,
    pool: &State<Pool>,
    flash: Option<FlashMessage<'_>>,
) -> Result<FilterTemplate> {
    let mut conn = pool.get().await?;
    let room = &access.room;

    let rules = filter::room_filter(&mut conn, room.id)
        .await?
        .unwrap_or_default();
    let diverging = filter::slot_filters(&mut conn, room.id).await?;

    // Named, not counted. "This does not reach 3 slots" sends somebody to work out which.
    let names: std::collections::HashMap<i32, String> =
        puna_core::model::slot::list(&mut conn, room.id)
            .await?
            .into_iter()
            .map(|s| (s.slot_number, s.player_name))
            .collect();

    let missed = diverging
        .iter()
        .map(|(n, state)| MissedSlot {
            slot_number: *n,
            player_name: names.get(n).cloned().unwrap_or_default(),
            because: match state {
                SlotFilter::Exempt => "is exempt from every filter",
                _ => "has rules of its own",
            },
        })
        .collect();

    let vocabulary = vocabulary();
    Ok(FilterTemplate {
        base: TplContext::new(access.session.session()),
        room_id: room.id.to_string(),
        room_name: room.name.clone(),
        scope: "Room filter".to_string(),
        slot: None,
        // The room's own page, which is about everybody rather than about a slot.
        effective: views(&rules, Subject::AnySlot),
        effective_from_room: false,
        rules: editor_rows(&rules),
        has_rules: !rules.is_empty(),
        slot_state: None,
        room_rules: Vec::new(),
        missed,
        directions: vocabulary.directions,
        kinds: vocabulary.kinds,
        tag_suggestions: vocabulary.tag_suggestions,
        subtype_suggestions: vocabulary.subtype_suggestions,
        refused: vocabulary.refused,
        // A room has nothing above it to inherit from, so an empty ruleset and no ruleset are the
        // same thing and there is no question to ask.
        empty_means_choice: false,
        empty_choice: None,
        empty_choice_required: true,
        notice: Notice::take(flash),
    })
}

#[post("/room/<id>/filter", data = "<form>")]
async fn edit_room(
    id: RoomParam,
    form: Form<FilterForm>,
    access: RoomAccess<Organizer>,
    pool: &State<Pool>,
) -> std::result::Result<Flash<Redirect>, Error> {
    let _ = id;
    let back = format!("/room/{}/filter", access.room.id);
    let mut conn = pool.get().await?;

    let next = match collect_rules(&form.rules) {
        Ok(next) => next,
        Err(message) => return Ok(Flash::error(Redirect::to(back), message)),
    };

    filter::set_room_filter(&mut conn, access.room.id, &next, access.user_id()).await?;
    puna_core::model::event::record(
        &mut conn,
        access.room.id,
        puna_core::model::event::Actor::User(access.user_id()),
        "room_filter_changed",
        serde_json::json!({ "rules": next.len() }),
    )
    .await?;

    let told = tell_the_room(
        &mut conn,
        &access.room,
        access.role(),
        access.user_id(),
        None,
    )
    .await?;

    // The count is in the sentence because the warning above the form is easy to read past, and
    // this is the moment it becomes true rather than hypothetical.
    let missed = filter::slot_filters(&mut conn, access.room.id).await?.len();
    let saved = if next.is_empty() {
        "Saved. This room has no filter now.".to_string()
    } else {
        format!("Saved {}.", puna_core::text::count(next.len(), "rule"))
    };
    let reach = if missed == 0 {
        if told {
            " Applied to the running room.".to_string()
        } else {
            " This room is not running, so it applies the next time it starts.".to_string()
        }
    } else {
        format!(
            " It does not reach {} that {} a filter of their own — they are listed below.",
            puna_core::text::count(missed, "slot"),
            puna_core::text::plural(missed, "has", "have"),
        )
    };
    Ok(Flash::success(
        Redirect::to(back),
        format!("{saved}{reach}"),
    ))
}

#[get("/room/<_id>/slot/<n>/filter")]
async fn show_slot(
    _id: RoomParam,
    n: i32,
    access: RoomAccess<Helper>,
    pool: &State<Pool>,
    flash: Option<FlashMessage<'_>>,
) -> Result<FilterTemplate> {
    let mut conn = pool.get().await?;
    let room = &access.room;

    let Some(slot) = puna_core::model::slot::get(&mut conn, room.id, n).await? else {
        return Err(crate::error::not_found("no such slot"));
    };

    let room_rules = filter::room_filter(&mut conn, room.id)
        .await?
        .unwrap_or_default();
    let state = filter::slot_filter(&mut conn, room.id, n).await?;
    let effective = Effective::of(&room_rules, &state);

    let vocabulary = vocabulary();
    Ok(FilterTemplate {
        base: TplContext::new(access.session.session()),
        room_id: room.id.to_string(),
        room_name: room.name.clone(),
        // **Parenthesised rather than dashed**, for two reasons beyond the em dash itself: this
        // string is also the `<title>`, where the template puts the room name after it, so a dash
        // here produced `Filter for slot 3 — Troy — Friday async`. And it keeps "Filter for", which
        // is what makes it a sibling of `Room filter` above rather than a bare slot label on a page
        // whose own identity would then rest on the `<h2>`.
        scope: format!("Filter for slot {n} ({})", slot.player_name),
        slot: Some(n),
        rules: editor_rows(match &state {
            SlotFilter::Own(rules) => rules,
            _ => &[],
        }),
        has_rules: matches!(&state, SlotFilter::Own(rules) if !rules.is_empty()),
        slot_state: Some(match state {
            SlotFilter::Follows => "follows",
            SlotFilter::Exempt => "exempt",
            SlotFilter::Own(_) => "own",
        }),
        // A slot's page, where both lists are about what happens to THIS slot — including the
        // room's rules, which are shown here as what they would do to it.
        effective: views(&effective.rules, Subject::ThisSlot),
        effective_from_room: effective.from_room,
        room_rules: views(&room_rules, Subject::ThisSlot),
        missed: Vec::new(),
        directions: vocabulary.directions,
        kinds: vocabulary.kinds,
        tag_suggestions: vocabulary.tag_suggestions,
        subtype_suggestions: vocabulary.subtype_suggestions,
        refused: vocabulary.refused,
        empty_means_choice: true,
        // **A state already chosen is shown as chosen; a table just emptied by hand is not.** So
        // `required` does real work in exactly the case that is genuinely unanswered, rather than
        // being satisfied by a default nobody read.
        empty_choice: match state {
            SlotFilter::Follows => Some("follow"),
            SlotFilter::Exempt => Some("exempt"),
            SlotFilter::Own(_) => None,
        },
        empty_choice_required: true,
        notice: Notice::take(flash),
    })
}

#[post("/room/<id>/slot/<n>/filter", data = "<form>")]
async fn edit_slot(
    id: RoomParam,
    n: i32,
    form: Form<FilterForm>,
    access: RoomAccess<Helper>,
    pool: &State<Pool>,
) -> std::result::Result<Flash<Redirect>, Error> {
    let _ = id;
    let back = format!("/room/{}/slot/{n}/filter", access.room.id);
    let mut conn = pool.get().await?;

    if puna_core::model::slot::get(&mut conn, access.room.id, n)
        .await?
        .is_none()
    {
        return Err(crate::error::not_found("no such slot"));
    }

    let next = match slot_state_from(&form.rules, form.state.as_deref()) {
        Ok(next) => next,
        Err(message) => return Ok(Flash::error(Redirect::to(back), message)),
    };

    filter::set_slot_filter(&mut conn, access.room.id, n, &next, access.user_id()).await?;
    puna_core::model::event::record(
        &mut conn,
        access.room.id,
        puna_core::model::event::Actor::User(access.user_id()),
        "slot_filter_changed",
        serde_json::json!({ "slot": n, "state": match next {
            SlotFilter::Follows => "follows",
            SlotFilter::Exempt => "exempt",
            SlotFilter::Own(_) => "own",
        }}),
    )
    .await?;

    // **The consequence, said at the moment it happens.** Neither direction is visible in the rule
    // that was just edited, and both surprise people: rules of its own cut the room's off, and
    // removing them turns the room's back on.
    let told = tell_the_room(
        &mut conn,
        &access.room,
        access.role(),
        access.user_id(),
        Some(n),
    )
    .await?;

    let room_filters = !filter::room_filter(&mut conn, access.room.id)
        .await?
        .unwrap_or_default()
        .is_empty();
    let message = match (&next, room_filters) {
        (SlotFilter::Follows, true) => {
            "Saved. This slot now follows the room's filter, which applies to it again."
        }
        (SlotFilter::Follows, false) => {
            "Saved. This slot has no filter, and neither does the room."
        }
        (SlotFilter::Exempt, true) => {
            "Saved. This slot is exempt from everything, including the room's filter."
        }
        (SlotFilter::Exempt, false) => "Saved. This slot is exempt from everything.",
        (SlotFilter::Own(_), true) => {
            "Saved. These rules REPLACE the room's for this slot — the room's filter no longer \
             applies to it."
        }
        (SlotFilter::Own(_), false) => "Saved.",
    };

    // Said plainly rather than left to be discovered: the durable half always lands, and whether
    // the room heard about it now is a different fact.
    let tail = if told {
        " Applied to the running room."
    } else {
        " This room is not running, so it applies the next time it starts."
    };
    Ok(Flash::success(
        Redirect::to(back),
        format!("{message}{tail}"),
    ))
}

pub fn routes() -> Vec<rocket::Route> {
    routes![show_room, edit_room, show_slot, edit_slot]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(value: &str) -> Option<String> {
        Some(value.to_string())
    }

    /// A row for `kind`, travelling a direction that kind can actually travel.
    ///
    /// **Derived rather than hardcoded, and the first version was not**: it said `from_slot` for
    /// everything, so `row("print_json", …)` built the very pairing pahoa refuses — and once
    /// `validate` learned to refuse it too, these tests started asserting against a rule no editor
    /// can produce. An unknown kind keeps `from_slot`, since those are refused before direction is
    /// ever considered.
    fn row(kind: &str, tag: Option<&str>, percent: Option<&str>) -> RuleFields {
        let direction = Kind::parse(kind)
            .and_then(|k| k.directions().first().copied())
            .map(Direction::as_str)
            .unwrap_or("from_slot");
        RuleFields {
            direction: field(direction),
            kind: field(kind),
            tag: tag.map(str::to_string),
            subtype: None,
            percent: percent.map(str::to_string),
            remove: false,
        }
    }

    /// The blank trailing row, as the browser submits it: a direction it never asked for, and
    /// nothing else.
    fn blank_row() -> RuleFields {
        RuleFields {
            direction: field("from_slot"),
            ..RuleFields::default()
        }
    }

    /// **A percentage on the form, a fraction on the wire**, and the direction of the conversion is
    /// the thing worth pinning: 75 typed means 0.75 stored means three quarters DROPPED.
    #[test]
    fn a_percentage_becomes_the_fraction_dropped() {
        let rule = build_rule(&row("bounce", Some("DeathLink"), Some("75"))).expect("builds");
        assert_eq!(rule.p, Some(0.75));
        assert!(
            rule.describe(Subject::ThisSlot)
                .contains("25% still get through"),
            "the page says what survives: {}",
            rule.describe(Subject::ThisSlot)
        );

        assert_eq!(
            build_rule(&row("bounce", Some("DeathLink"), Some("  ")))
                .expect("builds")
                .p,
            None,
            "blank is always"
        );

        assert!(
            build_rule(&row("bounce", None, Some("140")))
                .unwrap_err()
                .contains("percentage")
        );
        assert!(
            build_rule(&row("bounce", None, Some("a lot")))
                .unwrap_err()
                .contains("not a number")
        );
    }

    /// **And back again, because the editor now round-trips through the cell.** A stored rule is
    /// rendered as a percentage and re-read from one on the next save, so a conversion that loses
    /// precision one way would walk a probability across every edit.
    #[test]
    fn a_stored_probability_renders_as_a_percentage_that_reads_back_the_same() {
        // **0.07 and 0.29 are the point of this test.** Round numbers survive the trip whatever
        // this function does — `0.75 * 100.0` is exactly `75.0` — so a test built from those passes
        // against no rounding at all. These two do not: they are `7.000000000000001` and
        // `28.999999999999996`, which is both a cell nobody would leave alone and a probability
        // that walks a little further from where it started on every save.
        for p in [
            None,
            Some(0.75),
            Some(0.07),
            Some(0.29),
            Some(0.335),
            Some(1.0),
            Some(0.0),
        ] {
            let text = percent_text(p);
            let back = build_rule(&RuleFields {
                percent: Some(text.clone()),
                ..row("bounce", Some("DeathLink"), None)
            })
            .expect("re-reads")
            .p;
            assert_eq!(
                back, p,
                "{p:?} rendered as {text:?} and came back as {back:?}"
            );
        }

        // The float noise the rounding exists for, spelled out so the cell is asserted and not only
        // the round trip.
        assert_eq!(percent_text(Some(0.07)), "7");
        assert_eq!(percent_text(Some(0.29)), "29");
        assert_eq!(percent_text(None), "", "no probability is an empty cell");
    }

    /// pahoa's reason, before the round trip rather than after it.
    #[test]
    fn a_progression_kind_is_refused_by_name_with_its_reason() {
        let message = build_rule(&row("received_items", None, None)).unwrap_err();
        assert!(message.contains("desynchronizes"), "{message}");
        assert!(
            !message.contains("no such kind"),
            "refused is not the same as unknown: {message}"
        );

        assert!(
            build_rule(&row("banana", None, None))
                .unwrap_err()
                .contains("no such kind")
        );
    }

    /// **The blank trailing row costs nothing.** It is submitted on every save, so reading it as a
    /// rule would make every save fail and reading it as anything but "skip" would store junk.
    #[test]
    fn an_untouched_row_is_skipped_and_a_half_filled_one_is_refused() {
        assert_eq!(
            collect_rules(&[row("bounce", None, None), blank_row()]).expect("collects"),
            vec![Rule {
                direction: Direction::FromSlot,
                kind: Kind::Bounce,
                tag: None,
                subtype: None,
                p: None,
            }]
        );

        // Somebody typed a tag and forgot the kind. They believe they wrote a rule, so skipping it
        // quietly is the one outcome that looks like success.
        let half = RuleFields {
            tag: field("DeathLink"),
            ..blank_row()
        };
        let message = collect_rules(&[half]).unwrap_err();
        assert!(message.starts_with("row 1: "), "{message}");
        assert!(message.contains("choose what to drop"), "{message}");
    }

    /// A struck-out row is gone, and the rows after it keep the numbers the operator can see.
    #[test]
    fn a_removed_row_is_dropped_and_numbering_still_counts_it() {
        let mut removed = row("bounce", Some("DeathLink"), None);
        removed.remove = true;

        let rules = collect_rules(&[removed, row("print_json", None, None)]).expect("collects");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].kind, Kind::PrintJson);

        // Row 2 on screen is row 2 in the message, even though row 1 was struck out.
        let mut bad = row("print_json", None, None);
        bad.percent = field("nonsense");
        let mut struck = row("bounce", None, None);
        struck.remove = true;
        assert!(
            collect_rules(&[struck, bad])
                .unwrap_err()
                .starts_with("row 2: ")
        );
    }

    /// **Two rows naming the same thing are one rule at the room**, so the table would go on
    /// showing a rule that does not exist. Refused rather than collapsed: which probability
    /// survived would be anybody's guess.
    #[test]
    fn two_rows_that_match_the_same_thing_are_refused() {
        let message = collect_rules(&[
            row("bounce", Some("DeathLink"), Some("50")),
            row("bounce", Some("deathlink"), Some("90")),
        ])
        .unwrap_err();
        assert!(message.contains("row 2"), "{message}");
        assert!(message.contains("row 1"), "{message}");

        // Case is the whole point of that test: pahoa matches case-insensitively, so these two are
        // one rule there. Differently narrowed rows are fine.
        assert_eq!(
            collect_rules(&[
                row("bounce", Some("DeathLink"), None),
                row("bounce", Some("TrapLink"), None),
            ])
            .expect("collects")
            .len(),
            2
        );
    }

    /// **The three states, through the form.** An empty table is ambiguous for a slot and the page
    /// refuses to guess; the two answers produce different storage, which is the distinction the
    /// whole feature rests on.
    #[test]
    fn an_empty_table_has_to_say_which_kind_of_empty_it_is() {
        let empty = [blank_row()];

        let message = slot_state_from(&empty, None).unwrap_err();
        assert!(message.contains("follows the room"), "{message}");
        assert!(message.contains("exempt"), "{message}");

        assert_eq!(
            slot_state_from(&empty, Some("follow")).expect("follows"),
            SlotFilter::Follows
        );
        assert_eq!(
            slot_state_from(&empty, Some("exempt")).expect("exempt"),
            SlotFilter::Exempt
        );

        // And those map onto the two states a slot can be in without rules of its own.
        assert_eq!(SlotFilter::from_stored(None), SlotFilter::Follows);
        assert_eq!(SlotFilter::from_stored(Some(vec![])), SlotFilter::Exempt);

        // With rules, the answer is not consulted at all — an unanswered radio group is only a
        // refusal when the table is genuinely empty.
        assert!(matches!(
            slot_state_from(&[row("bounce", None, None)], None).expect("own"),
            SlotFilter::Own(rules) if rules.len() == 1
        ));
    }

    fn page(slot: Option<i32>, stored: &[Rule], state: Option<&'static str>) -> FilterTemplate {
        let vocabulary = vocabulary();
        FilterTemplate {
            base: TplContext {
                is_logged_in: true,
                is_admin: false,
                username: "troy".into(),
                site_name: "puna",
                version: "test",
                static_version: "test",
                view_as: None,
            },
            room_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            room_name: "Initial Sync".into(),
            scope: match slot {
                Some(n) => format!("Filter for slot {n} (MooingYacht1)"),
                None => "Room filter".into(),
            },
            slot,
            rules: editor_rows(stored),
            has_rules: !stored.is_empty(),
            slot_state: slot.map(|_| "own"),
            effective: views(
                stored,
                if slot.is_some() {
                    Subject::ThisSlot
                } else {
                    Subject::AnySlot
                },
            ),
            effective_from_room: false,
            room_rules: Vec::new(),
            missed: Vec::new(),
            directions: vocabulary.directions,
            kinds: vocabulary.kinds,
            tag_suggestions: vocabulary.tag_suggestions,
            subtype_suggestions: vocabulary.subtype_suggestions,
            refused: vocabulary.refused,
            empty_means_choice: slot.is_some(),
            empty_choice: state,
            empty_choice_required: true,
            notice: None,
        }
    }

    fn a_deathlink_rule() -> Rule {
        Rule {
            direction: Direction::FromSlot,
            kind: Kind::Bounce,
            tag: Some("DeathLink".into()),
            subtype: None,
            p: Some(0.75),
        }
    }

    /// **The slot is named in the heading, not only in the line under it.**
    ///
    /// Reported after the first look at this page: the room name is the boldest thing on it, so a
    /// reader's eye stops there and the slot — in dimmed text below — gets skipped entirely. The
    /// fix is only a fix if it is inside the `<h1>`, which is what this pins.
    #[test]
    fn the_slot_being_edited_is_named_in_the_heading() {
        let html = page(Some(1), &[], Some("follow"))
            .render()
            .expect("renders");
        let heading = html
            .split_once("<h1")
            .expect("no <h1>")
            .1
            .split_once("</h1>")
            .expect("unterminated <h1>")
            .0;

        assert!(
            heading.contains("slot 1"),
            "the h1 does not name the slot: {heading}"
        );
        assert!(
            heading.contains("MooingYacht1"),
            "the h1 does not name the player: {heading}"
        );
        assert!(
            heading.contains("Initial Sync"),
            "the room is still named too: {heading}"
        );

        // And the room's own filter page says which page it is, in the same place.
        let room = page(None, &[], None).render().expect("renders");
        let heading = room
            .split_once("<h1")
            .unwrap()
            .1
            .split_once("</h1>")
            .unwrap()
            .0;
        assert!(heading.contains("Room filter"), "{heading}");
    }

    /// **The cell a kind does not narrow with is DISABLED, from the server**, before any script
    /// runs — which is what stops a tag left over from when a row was a bounce being submitted and
    /// refused. Greying it is the visible half; not submitting it is the half that matters.
    #[test]
    fn a_rules_narrowing_cells_arrive_matching_its_kind() {
        let html = page(Some(1), &[a_deathlink_rule()], None)
            .render()
            .expect("renders");

        let tag_cell = html
            .split_once("rules[0].tag")
            .expect("no tag field")
            .1
            .split_once('>')
            .unwrap()
            .0;
        let subtype_cell = html
            .split_once("rules[0].subtype")
            .expect("no subtype field")
            .1
            .split_once('>')
            .unwrap()
            .0;

        assert!(
            !tag_cell.contains("disabled"),
            "a bounce IS narrowed by a tag: {tag_cell}"
        );
        assert!(
            subtype_cell.contains("disabled"),
            "a bounce is not narrowed by a subtype, so the cell must not submit one: {subtype_cell}"
        );
        // Greyed by the server too, not only once the script runs — otherwise the table reads as
        // fully editable until JavaScript arrives and half of it goes flat.
        assert!(
            html.contains(
                "<td class=\"inapplicable\"><input type=\"text\" name=\"rules[0].subtype\""
            ),
            "the subtype CELL is not marked, so nothing shows it does not apply without a script"
        );

        // The stored values come back into the row, or an edit would silently rewrite the rule it
        // was opened on.
        assert!(
            html.contains("value=\"DeathLink\""),
            "the tag is not in its cell"
        );
        assert!(
            html.contains("value=\"75\""),
            "the percentage is not in its cell"
        );

        // And the blank row is there, numbered after the last stored one.
        assert!(
            html.contains("rules[1].kind"),
            "no blank row, so a rule cannot be added without a script"
        );
    }

    /// **The radios are disabled while the table has rules, not merely hidden.**
    ///
    /// They are `required`, and a required control inside a hidden fieldset blocks submission with
    /// a validation message the browser cannot point at anything — so this form would simply refuse
    /// to save, silently, for every slot that has a rule.
    #[test]
    fn the_empty_table_question_cannot_block_a_table_that_has_rules() {
        let with_rules = page(Some(1), &[a_deathlink_rule()], None)
            .render()
            .expect("renders");
        let fieldset = with_rules
            .split_once("data-empty-meaning")
            .expect("no empty-meaning fieldset")
            .1;
        assert!(
            fieldset.split_once('>').unwrap().0.contains("hidden"),
            "the question is asked over a table that has answers"
        );
        for radio in fieldset.split("type=\"radio\"").skip(1) {
            let attributes = radio.split_once('>').unwrap().0;
            assert!(
                attributes.contains("disabled"),
                "a required radio inside a hidden fieldset blocks every submit: {attributes}"
            );
        }

        // Empty, it is visible, enabled, and the state already in force is the one selected.
        let empty = page(Some(1), &[], Some("exempt"))
            .render()
            .expect("renders");
        let fieldset = empty.split_once("data-empty-meaning").unwrap().1;
        assert!(!fieldset.split_once('>').unwrap().0.contains("hidden"));
        let exempt = fieldset
            .split_once("value=\"exempt\"")
            .expect("no exempt radio")
            .1
            .split_once('>')
            .unwrap()
            .0;
        assert!(exempt.contains("checked"), "{exempt}");
        assert!(!exempt.contains("disabled"), "{exempt}");

        // A ROOM has nothing above it to inherit from, so it is told rather than asked.
        let room = page(None, &[], None).render().expect("renders");
        assert!(!room.contains("data-empty-meaning"));
        assert!(room.contains("data-empty-notice"));
    }

    /// **The form field names are the contract with the template**, and Rocket's indexing is the
    /// part that is easy to get subtly wrong: it starts a new element when the index changes, so a
    /// row whose fields were not grouped together would silently merge into its neighbor.
    ///
    /// Parsed from a real query string rather than hand-built structs, because that is the only
    /// assertion that fails when the template renders `rules.0.direction` or `rule[0]`.
    #[test]
    fn the_table_parses_back_out_of_a_real_submission() {
        let query = "rules[0].direction=from_slot&rules[0].kind=bounce&rules[0].tag=DeathLink\
                     &rules[0].percent=75\
                     &rules[1].direction=to_slot&rules[1].kind=print_json&rules[1].subtype=Chat\
                     &rules[1].remove=true\
                     &rules[2].direction=from_slot&rules[2].kind=\
                     &state=follow";
        let form: FilterForm = Form::parse(query).expect("parses");

        assert_eq!(
            form.rules.len(),
            3,
            "one element per index, not one per field"
        );
        assert_eq!(form.state.as_deref(), Some("follow"));
        assert!(form.rules[1].remove, "the checkbox is read as a bool");
        assert!(
            !form.rules[0].remove,
            "an absent checkbox is false, not missing"
        );

        let rules = collect_rules(&form.rules).expect("collects");
        assert_eq!(rules.len(), 1, "one struck out, one blank");
        assert_eq!(rules[0].tag.as_deref(), Some("DeathLink"));
        assert_eq!(rules[0].p, Some(0.75));
    }
}
