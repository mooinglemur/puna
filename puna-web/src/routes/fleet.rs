//! `/admin/rooms`: what every room is running, and the control that changes it.
//!
//! The page exists because a spec change deliberately does **not** disturb a running room. An image
//! bump lands on the whole environment at once, and a room with people in it is not something a
//! `git push` gets to interrupt, so drift accumulates on purpose, and somebody has to be able to
//! see it and decide.

use puna_core::Environment;
use puna_core::db::Pool;
use puna_core::ids::RoomId;
use puna_core::model::fleet::{self, FleetRoom, Overview, Scope};
use rocket::http::Status;
use rocket::request::FlashMessage;
use rocket::response::content::RawHtml;
use rocket::response::{Flash, Redirect};
use rocket::{State, get, post, routes, uri};

use crate::auth::AdminSession;
use crate::error::{Error, Result};
use crate::flash::Notice;
use crate::params::RoomParam;
use crate::tpl::TplContext;

use askama::Template;
use askama_web::WebTemplate;

/// One row, with everything already decided so the template only formats.
///
/// The template does not compute drift or ages: a condition in markup is a condition nothing can
/// test, and this one has to agree exactly with what the bulk action operates on.
pub struct Row {
    pub id: RoomId,
    pub name: String,
    pub state: String,
    /// `running`, `stopped` or `closed`: what somebody ASKED for, which is what the resting table
    /// reports and what it is split on. See `fleet::Scope`.
    pub desired_state: String,
    /// Who opened it, already resolved to something printable: a username, `"never logged in"` for
    /// an id with no login yet, or `None` where there is nobody to name. The raw Discord id is
    /// deliberately not rendered: it identifies a person and reads as noise in a column.
    pub created_by: Option<String>,
    pub running_image: Option<String>,
    /// Just the tag, where the image has one. A full registry path repeated down a column is noise
    /// that hides the one part that differs between rows.
    pub running_tag: Option<String>,
    pub drift: Option<&'static str>,
    pub deployed_ago: Option<String>,
    /// Present only when the process is meaningfully younger than its Deployment: i.e. the pod
    /// restarted **without Puna asking**, its spec unchanged. An eviction, a drain, or a container
    /// restart in place.
    ///
    /// This is the column the word *restart* belongs to, and the only one. What Puna does to a room
    /// on purpose is a **redeploy**, and the two are one word apart with different causes: a
    /// redeploy is somebody pressing a button, a restart is Kubernetes moving the room underneath
    /// everybody. Both reload the save and disconnect every client, which is why an organizer
    /// notices the second one and cannot otherwise account for it.
    pub restarted_ago: Option<String>,
    /// The two ages again as seconds, for `data-value`.
    ///
    /// **The column cannot sort on what it displays.** "6d 2h" and "40m" compare as text with the
    /// 4 before the 6, so the oldest room in the fleet sorts into the middle, and a sort that is
    /// subtly wrong is worse than no sort, because it looks like it worked. `table.js` reads
    /// `data-value` in preference to the cell's text for exactly this.
    pub deployed_secs: Option<i64>,
    pub restarted_secs: Option<i64>,
    /// The same instants as epoch milliseconds, for the `title` `localtime.js` renders in the
    /// reader's own timezone. The shorthand answers "how long ago"; this answers "when", which is
    /// the question somebody has once they are correlating with a log line.
    pub deployed_at_ms: Option<i64>,
    pub restarted_at_ms: Option<i64>,
    pub clients: Option<i32>,
    pub redeploy_pending: bool,
    /// How long the room has been quiet, and the same span as a sort key. `None` for a room that is
    /// not running: nothing is idling on a room that is already off.
    pub idle: Option<String>,
    pub idle_secs: Option<i64>,
    pub idle_at_ms: Option<i64>,
    /// Whether the reaper is barred from this room, and who barred it.
    pub pinned: bool,
    pub pinned_by: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "admin/rooms.html")]
pub struct RoomsTemplate {
    base: TplContext,
    /// The tag the fleet is configured for, or the whole reference when it has no tag.
    desired_tag: Option<String>,
    desired_image: Option<String>,
    rows: Vec<Row>,
    drifted: usize,
    /// How many stopped or closed rooms exist, for the collapsed heading. They are **not** loaded
    /// with this page: see [`resting`].
    resting: i64,
    /// The sentence the last POST left behind, shown once. See [`crate::flash`].
    notice: Option<Notice>,
}

/// The stopped-and-closed table, which the main page does not carry.
///
/// **One template, two entry points**, the same shape `rooms/panel.html` uses: [`resting`] renders
/// it bare for the `fetch` that fills the collapsed section, and [`RestingPageTemplate`] wraps it
/// in the site chrome for the `<noscript>` link. A second copy of this table in JavaScript would be
/// a second set of columns to keep in step with the first.
#[derive(Template, WebTemplate)]
#[template(path = "admin/resting.html")]
pub struct RestingTemplate {
    rows: Vec<Row>,
}

#[derive(Template, WebTemplate)]
#[template(path = "admin/resting_page.html")]
pub struct RestingPageTemplate {
    base: TplContext,
    rows: Vec<Row>,
}

/// `registry/host/image:tag` -> `tag`, leaving anything without one alone.
///
/// Split on the LAST colon and only past the final slash, so a registry with a port in it
/// (`host:5000/image`) is not mistaken for a tag.
fn tag_of(image: &str) -> Option<String> {
    let after_host = image.rsplit('/').next()?;
    after_host.rsplit_once(':').map(|(_, tag)| tag.to_string())
}

/// The same span `ago` renders, as a number a column can be ordered by.
/// The instant itself, for a tooltip the browser renders in its own timezone.
///
/// Sent rather than reconstructed from the age beside it: the server knows the moment exactly, and
/// `now - age` in the browser would fold in page latency and any clock skew for no reason.
fn at_ms(at: chrono::DateTime<chrono::Utc>) -> i64 {
    at.timestamp_millis()
}

fn elapsed_secs(at: chrono::DateTime<chrono::Utc>) -> i64 {
    (chrono::Utc::now() - at).num_seconds().max(0)
}

fn ago(at: chrono::DateTime<chrono::Utc>) -> String {
    let elapsed = chrono::Utc::now() - at;
    let (days, hours, minutes) = (
        elapsed.num_days(),
        elapsed.num_hours() % 24,
        elapsed.num_minutes() % 60,
    );
    if days > 0 {
        format!("{days}d {hours}h")
    } else if elapsed.num_hours() > 0 {
        format!("{}h {minutes}m", elapsed.num_hours())
    } else {
        format!("{}m", elapsed.num_minutes().max(0))
    }
}

/// Who opened a room, in words.
///
/// Three cases and they are genuinely different: nobody recorded (an early row, or an account that
/// is gone), somebody with a row but no login yet (the lobby-push case, where a slot is assigned
/// to a Discord id that has never been here), and an ordinary username. The stand-in is spelled
/// out rather than shown raw, for the same reason the room page does it: a bare snowflake in a
/// column is not an answer to "who made this".
fn creator(room: &FleetRoom) -> Option<String> {
    let name = room.created_by_name.as_deref()?;
    Some(if puna_core::model::user::is_placeholder(name) {
        "never logged in".to_string()
    } else {
        name.to_string()
    })
}

fn rows_of(overview: &Overview) -> Vec<Row> {
    let fleet_image = overview.pahoa_image.as_deref();
    overview
        .rooms
        .iter()
        .map(|room: &FleetRoom| Row {
            id: room.id,
            name: room.name.clone(),
            state: room.state.clone(),
            desired_state: room.desired_state.clone(),
            created_by: creator(room),
            running_image: room.running_image.clone(),
            running_tag: room.running_image.as_deref().and_then(tag_of),
            drift: room.drift(fleet_image).map(|d| d.label()),
            deployed_ago: room.deployment_created_at.map(ago),
            restarted_ago: room.restarted_since_deploy().map(ago),
            deployed_at_ms: room.deployment_created_at.map(at_ms),
            restarted_at_ms: room.restarted_since_deploy().map(at_ms),
            deployed_secs: room.deployment_created_at.map(elapsed_secs),
            restarted_secs: room.restarted_since_deploy().map(elapsed_secs),
            clients: room.clients_connected,
            redeploy_pending: room.redeploy_requested_at.is_some(),
            idle: room.idle_since().map(ago),
            idle_secs: room.idle_since().map(elapsed_secs),
            idle_at_ms: room.idle_since().map(at_ms),
            pinned: room.pinned_at.is_some(),
            pinned_by: room.pinned_by_name.clone(),
        })
        .collect()
}

async fn render(
    session: AdminSession,
    pool: &State<Pool>,
    environment: Environment,
    notice: Option<Notice>,
) -> Result<RoomsTemplate> {
    let mut conn = pool.get().await?;
    // `Active` only. Every room anybody stopped stays in the database forever, so this table would
    // otherwise grow without bound while answering a question about the ones that are supposed to
    // be up.
    let overview = fleet::overview(&mut conn, environment, Scope::Active).await?;
    Ok(RoomsTemplate {
        base: TplContext::new(session.session()),
        desired_tag: overview.pahoa_image.as_deref().and_then(tag_of),
        desired_image: overview.pahoa_image.clone(),
        drifted: overview.drifted().count(),
        rows: rows_of(&overview),
        resting: overview.resting,
        notice,
    })
}

#[get("/admin/rooms")]
async fn show(
    session: AdminSession,
    pool: &State<Pool>,
    environment: &State<Environment>,
    flash: Option<FlashMessage<'_>>,
) -> Result<RoomsTemplate> {
    render(session, pool, **environment, Notice::take(flash)).await
}

/// The rooms somebody stopped or closed, fetched when the section is opened.
///
/// `fragment` selects the bare table over the full page. Both render the same partial, so the
/// scripted path and the `<noscript>` link cannot show different columns.
#[get("/admin/rooms/resting?<fragment>")]
async fn resting(
    session: AdminSession,
    pool: &State<Pool>,
    environment: &State<Environment>,
    fragment: Option<bool>,
) -> Result<RawHtml<String>> {
    let mut conn = pool.get().await?;
    let overview = fleet::overview(&mut conn, **environment, Scope::Resting).await?;
    let rows = rows_of(&overview);

    let html = if fragment.unwrap_or(false) {
        RestingTemplate { rows }.render()
    } else {
        RestingPageTemplate {
            base: TplContext::new(session.session()),
            rows,
        }
        .render()
    }
    .map_err(|e| Error::new(Status::InternalServerError, e.into()))?;

    Ok(RawHtml(html))
}

#[post("/admin/rooms/<id>/redeploy")]
async fn redeploy(
    _session: AdminSession,
    pool: &State<Pool>,
    id: RoomParam,
) -> Result<Flash<Redirect>> {
    let mut conn = pool.get().await?;
    let marked = fleet::request_redeploy(&mut conn, &[id.0]).await?;

    // "Already queued" is a real answer and a different one from "queued", because the whole
    // question an operator has after pressing this is whether anything is going to happen.
    let notice = if marked == 1 {
        "Redeploy queued. The room stops on the next reconcile and is back on the same address \
         under a minute later."
    } else {
        "That room already had a redeploy queued; its place in the queue is unchanged."
    };
    Ok(Flash::success(Redirect::to(uri!(show)), notice))
}

#[post("/admin/rooms/redeploy-drifted")]
async fn redeploy_drifted(
    _session: AdminSession,
    pool: &State<Pool>,
    environment: &State<Environment>,
) -> Result<Flash<Redirect>> {
    let mut conn = pool.get().await?;
    // `All`, deliberately, even though a resting room can never drift: `drift()` returns `None`
    // without a running image. Scanning the narrower set would give the same answer today and make
    // the bulk action's reach depend on an invariant stated somewhere else.
    let overview = fleet::overview(&mut conn, **environment, Scope::All).await?;

    // Filtered in Rust against the same `drift()` the table rendered, deliberately, rather than
    // re-expressed as a `WHERE` clause. Two definitions would eventually disagree, and the failure
    // is a button that acts on a set the operator was never shown.
    let ids: Vec<RoomId> = overview.drifted().map(|room| room.id).collect();
    let marked = fleet::request_redeploy(&mut conn, &ids).await?;

    let notice = format!(
        "Queued {marked} of {} drifted rooms. They redeploy one per tick, oldest request first, so \
         the rollout is gradual rather than all at once.",
        ids.len()
    );
    Ok(Flash::success(Redirect::to(uri!(show)), notice))
}

/// Exempt a room from the idle reaper, or stop exempting it.
///
/// **It changes nothing else.** A pinned room still stops, closes, redeploys and deletes on
/// request; the pin withholds exactly one thing, which is the orchestrator's decision to take it
/// down for being quiet.
#[post("/admin/rooms/<id>/pin?<pinned>")]
async fn pin(
    session: AdminSession,
    pool: &State<Pool>,
    id: RoomParam,
    pinned: bool,
) -> Result<Flash<Redirect>> {
    let mut conn = pool.get().await?;
    let moved = fleet::set_pinned(&mut conn, id.0, pinned, session.user_id()).await?;

    tracing::info!(
        room = %id,
        pinned,
        moved,
        by = session.user_id(),
        "room pin changed"
    );

    let notice = match (pinned, moved) {
        (true, true) => "Pinned. The idle reaper will leave this room alone until it is unpinned.",
        (true, false) => "That room was already pinned; who pinned it and when are unchanged.",
        (false, true) => "Unpinned. This room can be taken down again once it goes quiet.",
        (false, false) => "That room was not pinned.",
    };
    Ok(Flash::success(Redirect::to(uri!(show)), notice))
}

pub fn routes() -> Vec<rocket::Route> {
    routes![show, resting, redeploy, redeploy_drifted, pin]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeDelta, Utc};
    use puna_core::model::fleet::Drift;

    const CONFIGURED: &str = "registry.example.com/g/pahoa:sha-new";

    /// Fixture room ids: fixed, distinct, and **all hex letters with no digits**.
    ///
    /// A random uuid contains random digits, and the assertions below check that a Discord id is
    /// *absent* from the page, so a uuid happening to contain that digit run fails a correct
    /// build, at random, roughly never on the run you are watching. It did, once, immediately.
    /// `tracker/show.html`'s leak test already carries the same note for the same reason.
    const IDS: [&str; 4] = [
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        "bbbbbbbb-cccc-dddd-eeee-ffffffffffff",
        "cccccccc-dddd-eeee-ffff-aaaaaaaaaaaa",
        "dddddddd-eeee-ffff-aaaa-bbbbbbbbbbbb",
    ];

    /// **Ids are fixed and all-hex-letters, never `RoomId::new()`.**
    ///
    /// A random uuid contains random digits, and the assertions below check that a Discord id is
    /// *absent* from the page, so a uuid happening to contain that digit run fails a correct
    /// build, at random, roughly never on the run you are watching. It did. `tracker/show.html`'s
    /// leak test already carries the same note for the same reason: an all-letter id cannot collide
    /// with a number the page is asserted not to contain.
    fn id(n: usize) -> RoomId {
        IDS[n].parse().expect("a fixed id")
    }

    fn room(running: Option<&str>) -> FleetRoom {
        FleetRoom {
            // One id by default. A test that compares ids assigns its own from `IDS`: a shared
            // counter would hand out different ids depending on which other tests ran first, which
            // is a race dressed up as determinism.
            id: id(0),
            name: "midweek-async".into(),
            state: if running.is_some() { "running" } else { "idle" }.into(),
            desired_state: "running".into(),
            created_by: Some(4931),
            created_by_name: Some("troy".into()),
            running_image: running.map(str::to_string),
            deployment_created_at: running.map(|_| Utc::now() - TimeDelta::days(6)),
            process_started_at: running.map(|_| Utc::now() - TimeDelta::days(6)),
            clients_connected: running.map(|_| 4),
            // Checked something an hour ago, well inside any sane idle timeout.
            last_check_at: running.map(|_| Utc::now() - TimeDelta::hours(1)),
            started_at: running.map(|_| Utc::now() - TimeDelta::days(6)),
            pinned_at: None,
            pinned_by_name: None,
            spec_hash: Some("hash-1".into()),
            desired_spec_hash: None,
            redeploy_requested_at: None,
        }
    }

    /// **An idle room cannot drift**, and that is not a technicality: it is running nothing, so it
    /// picks up the current spec whenever it next starts. Counting it as drifted would put every
    /// stopped room in the environment into a bulk redeploy that would *start* them all.
    #[test]
    fn only_a_room_with_a_deployment_can_drift() {
        assert_eq!(
            room(Some("old")).drift(Some(CONFIGURED)),
            Some(Drift::Image)
        );
        assert_eq!(room(Some(CONFIGURED)).drift(Some(CONFIGURED)), None);
        assert_eq!(
            room(None).drift(Some(CONFIGURED)),
            None,
            "idle is not drift"
        );
        assert_eq!(
            room(Some("old")).drift(None),
            None,
            "nothing to compare against before the orchestrator has published"
        );
    }

    /// `None` on the desired hash means "the hourly lane has not computed one", never "unchanged":
    /// the same rule the planner's backoff interrupt follows. Getting it backwards would flag every
    /// room in the environment as drifted for the first hour after a deploy.
    #[test]
    fn an_uncomputed_spec_hash_is_not_a_disagreement() {
        let mut it = room(Some(CONFIGURED));
        assert_eq!(it.drift(Some(CONFIGURED)), None);

        it.desired_spec_hash = Some("hash-2".into());
        assert_eq!(it.drift(Some(CONFIGURED)), Some(Drift::Spec));
    }

    /// The gap every healthy room has (pod scheduling plus restoring the save) must not fill the
    /// column with noise, or the one case worth seeing is lost among them.
    #[test]
    fn a_normal_startup_gap_does_not_read_as_a_restart() {
        let mut it = room(Some(CONFIGURED));
        it.process_started_at = it
            .deployment_created_at
            .map(|at| at + TimeDelta::seconds(12));
        assert_eq!(it.restarted_since_deploy(), None, "12s is just starting up");

        it.process_started_at = it.deployment_created_at.map(|at| at + TimeDelta::hours(50));
        assert!(
            it.restarted_since_deploy().is_some(),
            "a process much younger than its spec means the pod moved under us"
        );
    }

    /// The page has to render, the drifted room has to be visibly drifted, and the control has to
    /// be reachable: a button nobody can find is the failure this codebase has already shipped
    /// twice.
    #[test]
    fn the_table_shows_drift_and_offers_a_redeploy() {
        // Distinct ids, because this test asserts which rows do and do not carry a control keyed
        // by id: with one shared id every such assertion is about all four rows at once.
        let mut drifted = room(Some("registry.example.com/g/pahoa:sha-old"));
        drifted.id = id(0);
        let mut current = room(Some(CONFIGURED));
        current.id = id(1);
        let mut idle = room(None);
        idle.id = id(2);
        // A room with a request already in flight, so the "redeploy queued" cell actually renders.
        // Without one that branch is never taken and every assertion about its wording passes
        // against markup nothing produced, which is exactly what happened until a mutation of
        // the tag failed to fail.
        let mut queued = room(Some(CONFIGURED));
        queued.id = id(3);
        queued.redeploy_requested_at = Some(Utc::now());
        let overview = Overview {
            pahoa_image: Some(CONFIGURED.into()),
            rooms: vec![drifted.clone(), current, queued.clone(), idle.clone()],
            resting: 2,
        };
        assert_eq!(overview.drifted().count(), 1);

        let page = RoomsTemplate {
            base: TplContext {
                is_logged_in: true,
                is_admin: true,
                username: "troy".into(),
                // Not "puna", so the assertion below is about the deployment's configured name
                // rather than about a default that happens to match the software's.
                site_name: "Example Multiworld",
                version: "test",
                static_version: "test",
                view_as: None,
            },
            desired_tag: Some("sha-new".into()),
            desired_image: Some(CONFIGURED.into()),
            drifted: overview.drifted().count(),
            rows: rows_of(&overview),
            resting: overview.resting,
            notice: None,
        };

        let html = page.render().expect("renders");

        // `whitespace = "suppress"` ate both spaces around the configured tag here and shipped
        // `runningsha-7bc9c967(registry...`. Asserting the rendered words rather than the markup,
        // because the bug is invisible in the source. The quoted failure follows the template: the
        // separator was an em dash until the sweep of 2026-08-31 made it a parenthesis.
        assert!(
            html.contains("should be running <code>sha-new</code>"),
            "the space before the configured tag survives suppression"
        );
        assert!(
            html.contains("</code>\n(") || html.contains("</code> ("),
            "and so does the one after it"
        );

        assert!(html.contains("sha-old"), "the running tag is shown");
        assert!(html.contains("image drift"), "and flagged as drifted");
        assert!(
            html.contains(&format!("/admin/rooms/{}/redeploy", drifted.id)),
            "the redeploy control is reachable for a running room"
        );
        assert!(
            !html.contains(&format!("/admin/rooms/{}/redeploy", idle.id)),
            "and is not offered for a room with nothing to redeploy"
        );
        assert!(
            html.contains("Redeploy all drifted rooms"),
            "the bulk control appears when something has drifted"
        );

        // One drifted room, so the singular branch. Asserted as a whole sentence because the two
        // branches differ in more than inflection: the plural one explains the per-tick pacing,
        // which describes a schedule of one when there is only one room to bounce.
        assert!(
            html.contains("1 room is not running the configured spec."),
            "one drifted room is described in the plural"
        );
        assert!(
            !html.contains("one per tick"),
            "the pacing explanation is offered for a single room, where it explains nothing"
        );

        // **"Redeploy" is Puna's action; "Restarted" is the column reporting that Kubernetes
        // replaced the pod without being asked.** They are one word apart and they mean different
        // things, so the page must not use the first word for the second event, which it did,
        // with a "restart queued" tag sitting two columns from a header reading "Restarted".
        assert!(
            html.contains("<th data-key=\"restarted\""),
            "the pod-restart column is still reported"
        );
        assert!(
            html.contains("redeploy queued"),
            "a room with a request in flight says so, and this is what gives the check below \
             something to check"
        );
        assert!(
            !html.contains(&format!("/admin/rooms/{}/redeploy\"", queued.id)),
            "a room already queued is not offered the control again"
        );
        for wrong in ["Restart all", ">Restart<", "restart queued"] {
            assert!(
                !html.contains(wrong),
                "{wrong:?} names Puna's own action \"restart\", which is the other column"
            );
        }

        // The corner link and the tab both carry the deployment's name, so a page cannot be
        // mistaken for the other environment's at a glance, which is the whole point of setting
        // it, and which a page rendering the hardcoded "puna" would silently defeat.
        assert!(
            html.contains(">Example Multiworld</a>"),
            "the brand link is the configured name"
        );
        // **The page leads and the name survives**, which is the shape every title took on
        // 2026-08-31: a tab is cut from the right, so "which page" has to come before "which
        // deployment". The admin pages are the ones that carry the site name at all: a room page
        // has a room name in that slot and never had one to lose.
        assert!(
            html.contains("<title>Rooms: Example Multiworld admin</title>"),
            "and so is the tab, after the page it names: {html:.400}"
        );

        // The name is a link to the room. An admin reading this table almost always wants the room
        // next, and the alternative is copying a uuid out of a cell to build the URL by hand.
        assert!(
            html.contains(&format!(
                "<a href=\"/room/{}\">midweek-async</a>",
                drifted.id
            )),
            "the room name links to the room"
        );

        // Who opened it, by name, never the raw snowflake, which identifies a person and answers
        // nobody's question.
        assert!(html.contains("<th data-key=\"created\">Created by</th>"));
        assert!(html.contains(">troy<"), "the creator's username is shown");
        assert!(!html.contains("4931"), "and their Discord id is not");

        // The section is present, labeled with its count, and EMPTY: the rooms behind it were
        // never loaded. A page that quietly rendered them would defeat the whole point.
        assert!(
            html.contains("Stopped and closed rooms (2)"),
            "the deferred section names how much is behind it"
        );
        assert!(
            html.contains("data-loads=\"/admin/rooms/resting?fragment=true\""),
            "and says where to get it"
        );
        assert!(
            html.contains("/admin/rooms/resting\">"),
            "with a plain link for scripting-off"
        );

        // **The page opts out of the prose measure.** `main` is 62rem because that is a readable
        // line length for prose; this page is eight columns of dense facts, and held to it the
        // table scrolls sideways on a wide screen with a third of the page empty beside it. The
        // opt-in is one line in the template, so losing it is silent: the page still renders, it
        // just goes back to scrolling.
        assert!(
            html.contains("<body class=\"wide\">"),
            "the rooms table is back inside the prose measure and will scroll sideways"
        );
    }

    /// Sorting reads `data-value` where the cell's text does not compare in the order it means.
    ///
    /// **This is the assertion that catches a silently wrong sort.** "6d 2h" against "40m" compares
    /// as text with the `4` before the `6`, so the oldest room in the fleet lands in the middle of
    /// an age-sorted column, and nothing about that looks broken.
    #[test]
    fn the_age_columns_carry_a_numeric_sort_key() {
        let overview = Overview {
            pahoa_image: Some(CONFIGURED.into()),
            rooms: vec![room(Some(CONFIGURED))],
            resting: 0,
        };
        let rows = rows_of(&overview);

        let secs = rows[0].deployed_secs.expect("a running room has an age");
        assert!(
            (secs - 6 * 86_400).abs() < 60,
            "the sort key is the age in seconds, got {secs}"
        );
        assert!(
            rows[0]
                .deployed_ago
                .as_deref()
                .is_some_and(|a| a.ends_with('h')),
            "while the cell still reads as a human duration: {:?}",
            rows[0].deployed_ago
        );

        // An idle room has neither, and a missing sort key must be missing rather than zero: a
        // zero would sort as "just deployed", which is the opposite of the truth.
        let none = rows_of(&Overview {
            pahoa_image: None,
            rooms: vec![room(None)],
            resting: 0,
        });
        assert_eq!(none[0].deployed_secs, None);
    }

    /// **The Idle column reports a running room's quiet, and nothing else's.**
    ///
    /// A stopped room's `last_activity_at` is whenever somebody last spoke *before it came down*,
    /// which grows forever, so reporting it would show every resting room as steadily more idle,
    /// which is both false and alarming. Nothing is idling on a room that is already off.
    ///
    /// The fallback to `started_at` matches the reaper exactly. If the two disagreed, this table
    /// would explain a decision the orchestrator did not make.
    #[test]
    fn idle_is_reported_for_running_rooms_and_measured_the_way_the_reaper_measures() {
        let mut up = room(Some(CONFIGURED));
        assert!(
            up.idle_since()
                .is_some_and(|at| at == up.last_check_at.unwrap()),
            "a running room reports when somebody last checked something"
        );

        // Nobody has checked anything yet: measured from the start, which is the clearest reap
        // candidate there is rather than a gap in the data.
        up.last_check_at = None;
        assert_eq!(up.idle_since(), up.started_at);

        // Stopped: no answer at all.
        let down = room(None);
        assert_eq!(
            down.idle_since(),
            None,
            "a room that is already off was reported as idling"
        );
    }

    /// Pinning replaces the number rather than sitting beside it, because for a pinned room the
    /// number is not what happens next.
    #[test]
    fn a_pinned_room_says_so_where_its_idle_time_would_be() {
        let mut pinned = room(Some(CONFIGURED));
        pinned.id = id(0);
        pinned.pinned_at = Some(Utc::now());
        pinned.pinned_by_name = Some("troy".into());

        let overview = Overview {
            pahoa_image: Some(CONFIGURED.into()),
            rooms: vec![pinned],
            resting: 0,
        };
        let page = RoomsTemplate {
            base: TplContext {
                is_logged_in: true,
                is_admin: true,
                username: "troy".into(),
                site_name: "puna",
                version: "test",
                static_version: "test",
                view_as: None,
            },
            desired_tag: None,
            desired_image: Some(CONFIGURED.into()),
            drifted: 0,
            rows: rows_of(&overview),
            resting: 0,
            notice: None,
        };
        let html = page.render().expect("renders");

        assert!(
            html.contains(">pinned<"),
            "the row does not say it is pinned"
        );
        assert!(html.contains("by troy"), "or who pinned it");
        // And the control offered is the one that undoes it.
        assert!(
            html.contains(&format!("/admin/rooms/{}/pin?pinned=false", id(0))),
            "a pinned room is not offered an unpin"
        );
        assert!(
            !html.contains("pinned=true"),
            "a pinned room was offered a pin, which would do nothing"
        );
    }

    /// A creator who has a row but has never logged in reads as words, not as a Discord id.
    ///
    /// The lobby push (M14) is what produces these: a slot assigned to somebody who has never been
    /// here. `ensure_exists` writes a stand-in username, and rendering it raw would put a bare
    /// snowflake in a column headed "Created by".
    #[test]
    fn a_creator_who_never_logged_in_is_named_as_such() {
        use puna_core::model::user::placeholder_username;

        let mut it = room(None);
        assert_eq!(creator(&it).as_deref(), Some("troy"));

        it.created_by_name = Some(placeholder_username(4931));
        assert_eq!(creator(&it).as_deref(), Some("never logged in"));

        it.created_by_name = None;
        assert_eq!(creator(&it), None, "nobody to name is not a name");
    }

    /// The notice is delivered out of band and rendered with its own severity class, so a failure
    /// cannot arrive in the color of a success.
    #[test]
    fn a_notice_renders_with_the_class_its_kind_names() {
        let page = |notice: Option<Notice>| RoomsTemplate {
            base: TplContext {
                is_logged_in: true,
                is_admin: true,
                username: "troy".into(),
                site_name: "puna",
                version: "test",
                static_version: "test",
                view_as: None,
            },
            desired_tag: None,
            desired_image: None,
            drifted: 0,
            rows: Vec::new(),
            resting: 0,
            notice,
        };

        let quiet = page(None).render().expect("renders");
        assert!(
            !quiet.contains("class=\"notice\""),
            "no notice when nothing was queued, which is what a refresh must look like"
        );

        let warned = page(Some(Notice {
            class: "warning",
            message: "Queued 0 of 3 drifted rooms.".into(),
        }))
        .render()
        .expect("renders");
        assert!(warned.contains("class=\"warning\">Queued 0 of 3 drifted rooms."));
    }

    #[test]
    fn a_registry_port_is_not_mistaken_for_a_tag() {
        assert_eq!(
            tag_of("registry.example.com/g/pahoa:sha-abc"),
            Some("sha-abc".into())
        );
        assert_eq!(tag_of("host:5000/pahoa"), None, "that colon is a port");
        assert_eq!(tag_of("host:5000/pahoa:sha-1"), Some("sha-1".into()));
        assert_eq!(tag_of("pahoa"), None);
    }
}
