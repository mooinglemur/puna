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
//! ## One rule per submission, and no JavaScript
//!
//! The editor lists what is stored and adds or removes one rule at a time. A dynamic multi-rule
//! form would be a better experience and a worse first cut: this works with scripting off, every
//! state is a plain POST, and the shape is the same for a room, a slot and — when it lands — the
//! bulk panel, which is what Troy asked for.

use puna_core::db::Pool;
use puna_core::model::filter::{self, Direction, Effective, Kind, Rule, SlotFilter};
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

/// One rule as the page renders it: the stored form plus the sentence that says what it does.
pub struct RuleView {
    pub index: usize,
    /// The effect in words. **Never the bare probability** — `p` is the fraction dropped and the
    /// opposite reading is equally natural, so the number alone invites whichever meaning the
    /// reader arrived with.
    pub describes: String,
    pub direction: &'static str,
}

fn views(rules: &[Rule]) -> Vec<RuleView> {
    rules
        .iter()
        .enumerate()
        .map(|(index, rule)| RuleView {
            index,
            describes: rule.describe(),
            direction: rule.direction.as_str(),
        })
        .collect()
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
    /// `None` for the room's own filter; the slot number otherwise. The template branches on it for
    /// every difference between the two scopes, so there is one page rather than two that drift.
    slot: Option<i32>,
    slot_player: String,
    /// What is stored at this scope.
    rules: Vec<RuleView>,
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
    kinds: Vec<(&'static str, Option<&'static str>)>,
    refused: Vec<(&'static str, &'static str)>,
    notice: Option<Notice>,
}

/// Add one rule, remove one by index, or set a slot's state outright.
#[derive(FromForm)]
pub struct FilterForm {
    /// `add`, `remove`, `follow`, `exempt`, or `clear`.
    action: String,
    /// For `remove`. An index into the stored list rather than a matcher, because the list on the
    /// page is what the operator is pointing at.
    index: Option<usize>,
    direction: Option<String>,
    kind: Option<String>,
    tag: Option<String>,
    subtype: Option<String>,
    /// The percentage **dropped**, 0–100, because a percentage is what the label says and a
    /// fraction is what pahoa takes. Empty means always.
    percent: Option<String>,
}

fn blank(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Build one rule from the form, naming what is wrong rather than answering a bare 400.
fn build_rule(form: &FilterForm) -> std::result::Result<Rule, String> {
    let kind_name = blank(&form.kind).ok_or_else(|| "choose what to drop".to_string())?;

    // **Refused by name, with pahoa's reason.** These parse and are things an operator genuinely
    // reaches for while trying to help a broken client, so "unknown kind" would send them looking
    // for a spelling mistake instead of telling them why it cannot work.
    if let Some((_, why)) = filter::REFUSED_KINDS
        .iter()
        .find(|(name, _)| *name == kind_name)
    {
        return Err(format!("\"{kind_name}\" cannot be filtered: {why}"));
    }

    let kind = Kind::parse(&kind_name).ok_or_else(|| format!("no such kind: {kind_name}"))?;
    let direction = blank(&form.direction)
        .and_then(|d| Direction::parse(&d))
        .ok_or_else(|| "choose a direction".to_string())?;

    // A percentage on the form, a fraction on the wire. The label says "% dropped" so the number
    // typed and the number stored mean the same thing to the person typing it.
    let p = match blank(&form.percent) {
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
        tag: blank(&form.tag),
        subtype: blank(&form.subtype),
        p,
    };
    rule.validate()?;
    Ok(rule)
}

/// Apply one form submission to a stored ruleset.
///
/// Pure, so every state transition is testable without a database — which matters more here than
/// usual, because the states are the part people get wrong.
fn apply_to(rules: Vec<Rule>, form: &FilterForm) -> std::result::Result<Option<Vec<Rule>>, String> {
    match form.action.as_str() {
        "add" => {
            let rule = build_rule(form)?;
            let mut rules = rules;
            // **Keyed on the matcher, as pahoa keys it**, so re-adding a rule with a different
            // probability replaces it rather than storing a second entry the room would collapse.
            let matcher = rule.matcher();
            rules.retain(|existing| existing.matcher() != matcher);
            rules.push(rule);
            Ok(Some(rules))
        }
        "remove" => {
            let index = form.index.ok_or_else(|| "which rule?".to_string())?;
            let mut rules = rules;
            if index >= rules.len() {
                return Err("that rule is no longer there".to_string());
            }
            rules.remove(index);
            Ok(Some(rules))
        }
        // A slot with no ruleset follows the room's; `None` is the delete that expresses it.
        "follow" => Ok(None),
        // An explicitly empty ruleset: filtered by nothing, even when the room filters.
        "exempt" => Ok(Some(Vec::new())),
        // For a room, empty and absent are the same thing, and `set_room_filter` treats them so.
        "clear" => Ok(Some(Vec::new())),
        other => Err(format!("no such action: {other}")),
    }
}

/// The rule vocabulary a form offers: what pahoa accepts, and what it refuses by name.
///
/// Built from the model's own `ALL` lists rather than restated in the template, so a kind pahoa
/// gains appears in the picker by being added once.
struct Vocabulary {
    directions: Vec<(&'static str, &'static str)>,
    kinds: Vec<(&'static str, Option<&'static str>)>,
    refused: Vec<(&'static str, &'static str)>,
}

fn vocabulary() -> Vocabulary {
    Vocabulary {
        directions: Direction::ALL
            .iter()
            .map(|d| (d.as_str(), d.label()))
            .collect(),
        kinds: Kind::ALL
            .iter()
            .map(|k| (k.as_str(), k.narrows_with()))
            .collect(),
        refused: filter::REFUSED_KINDS.to_vec(),
    }
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
        slot: None,
        slot_player: String::new(),
        effective: views(&rules),
        effective_from_room: false,
        rules: views(&rules),
        slot_state: None,
        room_rules: Vec::new(),
        missed,
        directions: vocabulary.directions,
        kinds: vocabulary.kinds,
        refused: vocabulary.refused,
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

    let current = filter::room_filter(&mut conn, access.room.id)
        .await?
        .unwrap_or_default();
    let next = match apply_to(current, &form) {
        Ok(next) => next.unwrap_or_default(),
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

    // The count is in the sentence because the warning above the form is easy to read past, and
    // this is the moment it becomes true rather than hypothetical.
    let missed = filter::slot_filters(&mut conn, access.room.id).await?.len();
    Ok(Flash::success(
        Redirect::to(back),
        if missed == 0 {
            "Saved. It takes effect on the running room at once.".to_string()
        } else {
            format!(
                "Saved, and it does not reach {missed} slot(s) that have a filter of their own — \
                 they are listed below."
            )
        },
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
        slot: Some(n),
        slot_player: slot.player_name,
        rules: views(match &state {
            SlotFilter::Own(rules) => rules,
            _ => &[],
        }),
        slot_state: Some(match state {
            SlotFilter::Follows => "follows",
            SlotFilter::Exempt => "exempt",
            SlotFilter::Own(_) => "own",
        }),
        effective: views(&effective.rules),
        effective_from_room: effective.from_room,
        room_rules: views(&room_rules),
        missed: Vec::new(),
        directions: vocabulary.directions,
        kinds: vocabulary.kinds,
        refused: vocabulary.refused,
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

    let state = filter::slot_filter(&mut conn, access.room.id, n).await?;
    let current = match &state {
        SlotFilter::Own(rules) => rules.clone(),
        _ => Vec::new(),
    };

    let next = match apply_to(current, &form) {
        Ok(next) => SlotFilter::from_stored(next),
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

    Ok(Flash::success(Redirect::to(back), message.to_string()))
}

pub fn routes() -> Vec<rocket::Route> {
    routes![show_room, edit_room, show_slot, edit_slot]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(action: &str) -> FilterForm {
        FilterForm {
            action: action.into(),
            index: None,
            direction: Some("from_slot".into()),
            kind: Some("bounce".into()),
            tag: Some("DeathLink".into()),
            subtype: None,
            percent: Some("75".into()),
        }
    }

    /// **A percentage on the form, a fraction on the wire**, and the direction of the conversion is
    /// the thing worth pinning: 75 typed means 0.75 stored means three quarters DROPPED.
    #[test]
    fn a_percentage_becomes_the_fraction_dropped() {
        let rule = build_rule(&form("add")).expect("builds");
        assert_eq!(rule.p, Some(0.75));
        assert!(
            rule.describe().contains("25% still get through"),
            "the page says what survives: {}",
            rule.describe()
        );

        let mut always = form("add");
        always.percent = Some("  ".into());
        assert_eq!(
            build_rule(&always).expect("builds").p,
            None,
            "blank is always"
        );

        let mut over = form("add");
        over.percent = Some("140".into());
        assert!(build_rule(&over).unwrap_err().contains("percentage"));

        let mut nonsense = form("add");
        nonsense.percent = Some("a lot".into());
        assert!(build_rule(&nonsense).unwrap_err().contains("not a number"));
    }

    /// pahoa's reason, before the round trip rather than after it.
    #[test]
    fn a_progression_kind_is_refused_by_name_with_its_reason() {
        let mut refused = form("add");
        refused.kind = Some("received_items".into());
        let message = build_rule(&refused).unwrap_err();
        assert!(message.contains("desynchronizes"), "{message}");
        assert!(
            !message.contains("no such kind"),
            "refused is not the same as unknown: {message}"
        );

        let mut unknown = form("add");
        unknown.kind = Some("banana".into());
        assert!(build_rule(&unknown).unwrap_err().contains("no such kind"));
    }

    /// Adding the same matcher twice replaces rather than accumulating — pahoa keys on the matcher,
    /// so two entries here would be one there and the page would show a rule that does not exist.
    #[test]
    fn re_adding_a_rule_replaces_it_rather_than_duplicating() {
        let first = apply_to(Vec::new(), &form("add"))
            .expect("add")
            .expect("rules");
        assert_eq!(first.len(), 1);

        let mut heavier = form("add");
        heavier.percent = Some("90".into());
        let second = apply_to(first, &heavier).expect("add").expect("rules");
        assert_eq!(second.len(), 1, "one matcher, one rule");
        assert_eq!(second[0].p, Some(0.9), "the newer probability wins");
    }

    /// **The three states, through the form.** `follow` and `exempt` are different submissions
    /// producing different storage, which is the distinction the whole feature rests on.
    #[test]
    fn follow_and_exempt_are_different_submissions() {
        let rules = apply_to(Vec::new(), &form("add"))
            .expect("add")
            .expect("rules");

        assert_eq!(
            apply_to(rules.clone(), &form("follow")).expect("follow"),
            None
        );
        assert_eq!(
            apply_to(rules.clone(), &form("exempt")).expect("exempt"),
            Some(Vec::new())
        );

        // And those map onto the two states a slot can be in without rules of its own.
        assert_eq!(SlotFilter::from_stored(None), SlotFilter::Follows);
        assert_eq!(SlotFilter::from_stored(Some(vec![])), SlotFilter::Exempt);
    }

    #[test]
    fn removing_a_rule_that_is_gone_says_so() {
        let mut remove = form("remove");
        remove.index = Some(4);
        assert!(
            apply_to(Vec::new(), &remove)
                .unwrap_err()
                .contains("no longer")
        );

        let mut ok = form("remove");
        ok.index = Some(0);
        let rules = apply_to(Vec::new(), &form("add"))
            .expect("add")
            .expect("rules");
        assert_eq!(apply_to(rules, &ok).expect("remove"), Some(Vec::new()));
    }
}
