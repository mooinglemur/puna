//! `/admin/gates`: the two creation switches and the allowlist.
//!
//! Every change is recorded with `updated_by` / `added_by`, because "who opened room creation"
//! is a question that only ever gets asked after something has gone wrong.

use puna_core::db::Pool;
use puna_core::model::settings::{self, AllowlistEntry, Gate, GateMode};
use rocket::form::Form;
use rocket::http::Status;
use rocket::response::Redirect;
use rocket::{FromForm, State, get, post, routes};

use crate::auth::AdminSession;
use crate::error::{Error, Result};
use crate::tpl::TplContext;

use askama::Template;
use askama_web::WebTemplate;

#[derive(Template, WebTemplate)]
#[template(path = "admin/gates.html")]
pub struct GatesTemplate {
    base: TplContext,
    gates: Vec<Gate>,
    allowlist: Vec<AllowlistEntry>,
}

#[get("/admin/gates")]
async fn show(session: AdminSession, pool: &State<Pool>) -> Result<GatesTemplate> {
    let mut conn = pool.get().await?;
    Ok(GatesTemplate {
        base: TplContext::new(session.session()),
        gates: settings::all(&mut conn).await?,
        allowlist: settings::allowlist(&mut conn).await?,
    })
}

#[derive(FromForm)]
struct SetGateForm {
    key: String,
    mode: String,
}

#[post("/admin/gates", data = "<form>")]
async fn set_gate(
    session: AdminSession,
    pool: &State<Pool>,
    form: Form<SetGateForm>,
) -> Result<Redirect> {
    let mode = GateMode::parse(&form.mode).ok_or_else(|| {
        Error::new(
            Status::BadRequest,
            anyhow::anyhow!("unknown gate mode {:?}", form.mode),
        )
    })?;

    // The key comes from the form, so it is checked against the set Puna actually owns rather
    // than written through: an unrecognized key would insert a row nothing ever reads, which
    // looks like a working change and silently is not.
    let key = known_gate(&form.key).ok_or_else(|| {
        Error::new(
            Status::BadRequest,
            anyhow::anyhow!("unknown gate {:?}", form.key),
        )
    })?;

    let mut conn = pool.get().await?;
    settings::set_mode(&mut conn, key, mode, session.user_id()).await?;

    tracing::info!(
        admin = session.user_id(),
        gate = key,
        mode = mode.as_sql(),
        "creation gate changed"
    );
    Ok(Redirect::to("/admin/gates"))
}

/// The gates Puna knows about, resolved from a form value to a `'static` key.
fn known_gate(key: &str) -> Option<&'static str> {
    use puna_core::model::RoomSource;
    [RoomSource::Direct, RoomSource::Lobby]
        .into_iter()
        .map(RoomSource::settings_key)
        .find(|known| *known == key)
}

#[derive(FromForm)]
struct AllowForm {
    /// A Discord snowflake, as text. It exceeds 2^53, so it travels as a string everywhere:
    /// a JSON number would lose precision in the browser.
    user_id: String,
    note: Option<String>,
}

#[post("/admin/gates/allow", data = "<form>")]
async fn allow(
    session: AdminSession,
    pool: &State<Pool>,
    form: Form<AllowForm>,
) -> Result<Redirect> {
    let user_id: i64 = form.user_id.trim().parse().map_err(|_| {
        Error::new(
            Status::BadRequest,
            anyhow::anyhow!("a Discord id is a number"),
        )
    })?;

    let note = form
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());

    let mut conn = pool.get().await?;
    // No `users` row is created: `creator_allowlist` has no foreign key precisely so that a
    // Discord id can be authorized before its owner has ever logged in, which is the normal case
    // when access is arranged in a Discord channel first.
    settings::allow(&mut conn, user_id, note, session.user_id()).await?;

    tracing::info!(
        admin = session.user_id(),
        user_id,
        "added to the creator allowlist"
    );
    Ok(Redirect::to("/admin/gates"))
}

#[derive(FromForm)]
struct RevokeForm {
    user_id: String,
}

#[post("/admin/gates/revoke", data = "<form>")]
async fn revoke(
    session: AdminSession,
    pool: &State<Pool>,
    form: Form<RevokeForm>,
) -> Result<Redirect> {
    let user_id: i64 = form.user_id.trim().parse().map_err(|_| {
        Error::new(
            Status::BadRequest,
            anyhow::anyhow!("a Discord id is a number"),
        )
    })?;

    let mut conn = pool.get().await?;
    let removed = settings::revoke(&mut conn, user_id).await?;

    tracing::info!(
        admin = session.user_id(),
        user_id,
        removed,
        "removed from the creator allowlist"
    );
    Ok(Redirect::to("/admin/gates"))
}

pub fn routes() -> Vec<rocket::Route> {
    routes![show, set_gate, allow, revoke]
}
