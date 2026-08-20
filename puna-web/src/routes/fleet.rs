//! `/admin/rooms` -- what every room is running, and the control that changes it.
//!
//! The page exists because a spec change deliberately does **not** disturb a running room. An image
//! bump lands on the whole environment at once, and a room with people in it is not something a
//! `git push` gets to interrupt -- so drift accumulates on purpose, and somebody has to be able to
//! see it and decide.

use puna_core::Environment;
use puna_core::db::Pool;
use puna_core::ids::RoomId;
use puna_core::model::fleet::{self, FleetRoom, Overview};
use rocket::response::Redirect;
use rocket::{State, get, post, routes, uri};

use crate::auth::AdminSession;
use crate::error::Result;
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
    pub running_image: Option<String>,
    /// Just the tag, where the image has one. A full registry path repeated down a column is noise
    /// that hides the one part that differs between rows.
    pub running_tag: Option<String>,
    pub drift: Option<&'static str>,
    pub deployed_ago: Option<String>,
    /// Present only when the process is meaningfully younger than its Deployment -- i.e. the room
    /// restarted under Puna without its spec changing.
    pub restarted_ago: Option<String>,
    pub clients: Option<i32>,
    pub redeploy_pending: bool,
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
    notice: Option<String>,
}

/// `registry/host/image:tag` -> `tag`, leaving anything without one alone.
///
/// Split on the LAST colon and only past the final slash, so a registry with a port in it
/// (`host:5000/image`) is not mistaken for a tag.
fn tag_of(image: &str) -> Option<String> {
    let after_host = image.rsplit('/').next()?;
    after_host.rsplit_once(':').map(|(_, tag)| tag.to_string())
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

fn rows_of(overview: &Overview) -> Vec<Row> {
    let fleet_image = overview.pahoa_image.as_deref();
    overview
        .rooms
        .iter()
        .map(|room: &FleetRoom| Row {
            id: room.id,
            name: room.name.clone(),
            state: room.state.clone(),
            running_image: room.running_image.clone(),
            running_tag: room.running_image.as_deref().and_then(tag_of),
            drift: room.drift(fleet_image).map(|d| d.label()),
            deployed_ago: room.deployment_created_at.map(ago),
            restarted_ago: room.restarted_since_deploy().map(ago),
            clients: room.clients_connected,
            redeploy_pending: room.redeploy_requested_at.is_some(),
        })
        .collect()
}

async fn render(
    session: AdminSession,
    pool: &State<Pool>,
    environment: Environment,
    notice: Option<String>,
) -> Result<RoomsTemplate> {
    let mut conn = pool.get().await?;
    let overview = fleet::overview(&mut conn, environment).await?;
    Ok(RoomsTemplate {
        base: TplContext::new(session.session()),
        desired_tag: overview.pahoa_image.as_deref().and_then(tag_of),
        desired_image: overview.pahoa_image.clone(),
        drifted: overview.drifted().count(),
        rows: rows_of(&overview),
        notice,
    })
}

#[get("/admin/rooms?<notice>")]
async fn show(
    session: AdminSession,
    pool: &State<Pool>,
    environment: &State<Environment>,
    notice: Option<String>,
) -> Result<RoomsTemplate> {
    render(session, pool, **environment, notice).await
}

#[post("/admin/rooms/<id>/redeploy")]
async fn redeploy(_session: AdminSession, pool: &State<Pool>, id: RoomParam) -> Result<Redirect> {
    let mut conn = pool.get().await?;
    let marked = fleet::request_redeploy(&mut conn, &[id.0]).await?;

    // "Already queued" is a real answer and a different one from "queued", because the whole
    // question an operator has after pressing this is whether anything is going to happen.
    let notice = if marked == 1 {
        "Queued. The room restarts within a tick or two and comes back on the same address."
    } else {
        "That room already had a restart queued; its place in the queue is unchanged."
    };
    Ok(Redirect::to(uri!(show(notice = Some(notice)))))
}

#[post("/admin/rooms/redeploy-drifted")]
async fn redeploy_drifted(
    _session: AdminSession,
    pool: &State<Pool>,
    environment: &State<Environment>,
) -> Result<Redirect> {
    let mut conn = pool.get().await?;
    let overview = fleet::overview(&mut conn, **environment).await?;

    // Filtered in Rust against the same `drift()` the table rendered, deliberately, rather than
    // re-expressed as a `WHERE` clause. Two definitions would eventually disagree, and the failure
    // is a button that acts on a set the operator was never shown.
    let ids: Vec<RoomId> = overview.drifted().map(|room| room.id).collect();
    let marked = fleet::request_redeploy(&mut conn, &ids).await?;

    let notice = format!(
        "Queued {marked} of {} drifted rooms. They restart one per tick, oldest request first, so \
         the rollout is gradual rather than all at once.",
        ids.len()
    );
    Ok(Redirect::to(uri!(show(notice = Some(notice)))))
}

pub fn routes() -> Vec<rocket::Route> {
    routes![show, redeploy, redeploy_drifted]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeDelta, Utc};
    use puna_core::model::fleet::Drift;

    const CONFIGURED: &str = "registry.example.com/g/pahoa:sha-new";

    fn room(running: Option<&str>) -> FleetRoom {
        FleetRoom {
            id: RoomId::new(),
            name: "midweek-async".into(),
            state: if running.is_some() { "running" } else { "idle" }.into(),
            running_image: running.map(str::to_string),
            deployment_created_at: running.map(|_| Utc::now() - TimeDelta::days(6)),
            process_started_at: running.map(|_| Utc::now() - TimeDelta::days(6)),
            clients_connected: running.map(|_| 4),
            spec_hash: Some("hash-1".into()),
            desired_spec_hash: None,
            redeploy_requested_at: None,
        }
    }

    /// **An idle room cannot drift**, and that is not a technicality: it is running nothing, so it
    /// picks up the current spec whenever it next starts. Counting it as drifted would put every
    /// stopped room in the environment into a bulk restart that would *start* them all.
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

    /// `None` on the desired hash means "the hourly lane has not computed one", never "unchanged" —
    /// the same rule the planner's backoff interrupt follows. Getting it backwards would flag every
    /// room in the environment as drifted for the first hour after a deploy.
    #[test]
    fn an_uncomputed_spec_hash_is_not_a_disagreement() {
        let mut it = room(Some(CONFIGURED));
        assert_eq!(it.drift(Some(CONFIGURED)), None);

        it.desired_spec_hash = Some("hash-2".into());
        assert_eq!(it.drift(Some(CONFIGURED)), Some(Drift::Spec));
    }

    /// The gap every healthy room has — pod scheduling plus restoring the save — must not fill the
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
    /// be reachable — a button nobody can find is the failure this codebase has already shipped
    /// twice.
    #[test]
    fn the_table_shows_drift_and_offers_a_restart() {
        let drifted = room(Some("registry.example.com/g/pahoa:sha-old"));
        let current = room(Some(CONFIGURED));
        let idle = room(None);
        let overview = Overview {
            pahoa_image: Some(CONFIGURED.into()),
            rooms: vec![drifted.clone(), current, idle.clone()],
        };
        assert_eq!(overview.drifted().count(), 1);

        let page = RoomsTemplate {
            base: TplContext {
                is_logged_in: true,
                is_admin: true,
                username: "troy".into(),
                version: "test",
                static_version: "test",
            },
            desired_tag: Some("sha-new".into()),
            desired_image: Some(CONFIGURED.into()),
            drifted: overview.drifted().count(),
            rows: rows_of(&overview),
            notice: None,
        };

        let html = page.render().expect("renders");

        // `whitespace = "suppress"` ate both spaces around the configured tag here and shipped
        // `runningsha-7bc9c967— registry...`. Asserting the rendered words rather than the markup,
        // because the bug is invisible in the source.
        assert!(
            html.contains("should be running <code>sha-new</code>"),
            "the space before the configured tag survives suppression"
        );
        assert!(
            html.contains("</code>\n&mdash;") || html.contains("</code> &mdash;"),
            "and so does the one after it"
        );

        assert!(html.contains("sha-old"), "the running tag is shown");
        assert!(html.contains("image drift"), "and flagged as drifted");
        assert!(
            html.contains(&format!("/admin/rooms/{}/redeploy", drifted.id)),
            "the restart control is reachable for a running room"
        );
        assert!(
            !html.contains(&format!("/admin/rooms/{}/redeploy", idle.id)),
            "and is not offered for a room with nothing to restart"
        );
        assert!(
            html.contains("Restart all drifted rooms"),
            "the bulk control appears when something has drifted"
        );
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
