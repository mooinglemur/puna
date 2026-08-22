//! `/admin/users` -- who Puna knows, what standing they are in, and the two controls that change
//! it.
//!
//! ## Sanctions never delete
//!
//! `restricted` and `banned` withhold what somebody may *do*; neither touches a room they opened, a
//! slot they hold, or a membership they were given. That is deliberate rather than lazy: an async
//! multiworld is other people's game as much as it is theirs, and a sanction that emptied their
//! slots would punish everybody sharing the room. Removing them from a specific room is a roster
//! action in that room, by its organizers, which is a different decision made by different people.
//!
//! ## Viewing as somebody is READ-ONLY, and that is enforced in one place
//!
//! [`view_as`] rewrites the caller's own session so every guard, query and template resolves to the
//! other person with no special case anywhere -- and `Session`'s request guard refuses any
//! non-`GET` while it is set. See `auth::Session::from_request`, which is where the rule lives and
//! why it lives there rather than here.
//!
//! The value is answering "what does this person actually see", which is the question behind most
//! support conversations and one an admin cannot answer from their own account: they see more.

use puna_core::db::Pool;
use puna_core::model::user::{self, AdminUser, UserStatus};
use rocket::form::Form;
use rocket::http::{CookieJar, Status};
use rocket::request::FlashMessage;
use rocket::response::{Flash, Redirect};
use rocket::{FromForm, State, get, post, routes, uri};

use crate::auth::{AdminSession, Session, ViewAs};
use crate::error::{Error, Result};
use crate::flash::Notice;
use crate::tpl::TplContext;

use askama::Template;
use askama_web::WebTemplate;

/// One row, with every decision already made so the template only formats.
pub struct Row {
    /// Rendered as text, not a number: a Discord snowflake exceeds 2^53 and JavaScript would round
    /// it -- and this table is sorted and filtered in the browser.
    pub id: String,
    pub username: String,
    /// `true` when the account has a row but has never signed in, which the lobby push produces.
    pub never_logged_in: bool,
    pub status: &'static str,
    pub is_active: bool,
    pub note: Option<String>,
    pub changed_by: Option<String>,
    pub joined: String,
    pub joined_secs: i64,
    /// The instants behind the two ages, as epoch milliseconds. See `fleet::at_ms`.
    pub joined_at_ms: i64,
    pub last_seen: String,
    pub last_seen_secs: i64,
    pub last_seen_at_ms: i64,
    pub rooms_created: i64,
    pub slots_held: i64,
    /// Whether the "view as" control is offered. Not for yourself: it would be a session rewrite
    /// that changes nothing, with a banner claiming otherwise.
    pub can_view_as: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "admin/users.html")]
pub struct UsersTemplate {
    base: TplContext,
    rows: Vec<Row>,
    restricted: usize,
    banned: usize,
    notice: Option<Notice>,
}

fn ago(at: chrono::DateTime<chrono::Utc>) -> String {
    let elapsed = chrono::Utc::now() - at;
    let days = elapsed.num_days();
    if days >= 365 {
        format!("{}y {}d", days / 365, days % 365)
    } else if days > 0 {
        format!("{days}d")
    } else if elapsed.num_hours() > 0 {
        format!("{}h", elapsed.num_hours())
    } else {
        format!("{}m", elapsed.num_minutes().max(0))
    }
}

fn elapsed_secs(at: chrono::DateTime<chrono::Utc>) -> i64 {
    (chrono::Utc::now() - at).num_seconds().max(0)
}

fn rows_of(users: Vec<AdminUser>, viewer: i64) -> Vec<Row> {
    users
        .into_iter()
        .map(|u| Row {
            id: u.id.to_string(),
            never_logged_in: user::is_placeholder(&u.username),
            status: u.status().as_sql(),
            is_active: u.status() == UserStatus::Active,
            note: u.status_note.clone(),
            changed_by: u.changed_by_name.clone(),
            joined: ago(u.first_seen_at),
            joined_secs: elapsed_secs(u.first_seen_at),
            joined_at_ms: u.first_seen_at.timestamp_millis(),
            last_seen_at_ms: u.last_seen_at.timestamp_millis(),
            last_seen: ago(u.last_seen_at),
            last_seen_secs: elapsed_secs(u.last_seen_at),
            rooms_created: u.rooms_created,
            slots_held: u.slots_held,
            can_view_as: u.id != viewer,
            username: u.username,
        })
        .collect()
}

#[get("/admin/users")]
async fn show(
    session: AdminSession,
    pool: &State<Pool>,
    flash: Option<FlashMessage<'_>>,
) -> Result<UsersTemplate> {
    let mut conn = pool.get().await?;
    let users = user::list(&mut conn).await?;

    let restricted = users
        .iter()
        .filter(|u| u.status() == UserStatus::Restricted)
        .count();
    let banned = users
        .iter()
        .filter(|u| u.status() == UserStatus::Banned)
        .count();

    Ok(UsersTemplate {
        rows: rows_of(users, session.user_id()),
        restricted,
        banned,
        notice: Notice::take(flash),
        base: TplContext::new(session.session()),
    })
}

#[derive(FromForm)]
struct StatusForm {
    /// Text, because a Discord snowflake exceeds 2^53 and would lose precision as a number.
    user_id: String,
    status: String,
    note: Option<String>,
}

#[post("/admin/users/status", data = "<form>")]
async fn set_status(
    session: AdminSession,
    pool: &State<Pool>,
    form: Form<StatusForm>,
) -> Result<Flash<Redirect>> {
    let target: i64 = form
        .user_id
        .parse()
        .map_err(|_| Error::new(Status::BadRequest, anyhow::anyhow!("not a Discord id")))?;
    let status = UserStatus::parse(&form.status)
        .ok_or_else(|| Error::new(Status::BadRequest, anyhow::anyhow!("unknown status")))?;

    // **You cannot sanction yourself.** Not paternalism: an administrator who bans their own
    // account is locked out of the page that would undo it, and the repair is a psql session. The
    // admin list is config rather than a database row, so nobody else can necessarily grant it back.
    if target == session.user_id() {
        return Ok(Flash::warning(
            Redirect::to(uri!(show)),
            "You cannot change your own standing. Banning yourself would lock you out of the page \
             that undoes it.",
        ));
    }

    let mut conn = pool.get().await?;
    user::set_status(
        &mut conn,
        target,
        status,
        form.note.as_deref(),
        session.user_id(),
    )
    .await?;
    // So it bites on the next request for this replica rather than in ten seconds. The other
    // replica expires on its own; see `auth::forget_standing`.
    crate::auth::forget_standing(target);

    // WARN rather than INFO: this is a moderation decision about a person, and it is the line
    // somebody will go looking for months later.
    tracing::warn!(
        target,
        status = status.as_sql(),
        by = session.user_id(),
        by_name = session.username(),
        "account standing changed"
    );

    let notice = match status {
        UserStatus::Active => "Account restored. They can create rooms and upload again.",
        UserStatus::Restricted => {
            "Account restricted. They keep their slots and rooms, and can no longer create or \
             upload."
        }
        UserStatus::Banned => {
            "Account banned. Existing sessions stop working within a few seconds. Nothing of \
             theirs was deleted."
        }
    };
    Ok(Flash::success(Redirect::to(uri!(show)), notice))
}

#[derive(FromForm)]
struct ViewAsForm {
    user_id: String,
}

/// See the site as somebody else, read-only.
///
/// Rewrites the caller's own session so that `user_id`, `username` and `is_admin` describe the
/// target -- which is what makes every guard and template resolve to them with no branch anywhere
/// -- and stashes the real identity in `view_as` for the banner and the way back.
///
/// **`is_admin` goes to `false`**, and that is not merely tidy: leaving it set would render the
/// admin navigation and the admin-gated pages while claiming to show what an ordinary user sees,
/// which is the opposite of the question this exists to answer.
#[post("/admin/users/view-as", data = "<form>")]
async fn view_as(
    session: AdminSession,
    pool: &State<Pool>,
    cookies: &CookieJar<'_>,
    form: Form<ViewAsForm>,
) -> Result<Flash<Redirect>> {
    let target: i64 = form
        .user_id
        .parse()
        .map_err(|_| Error::new(Status::BadRequest, anyhow::anyhow!("not a Discord id")))?;

    if target == session.user_id() {
        return Ok(Flash::warning(
            Redirect::to(uri!(show)),
            "You are already yourself.",
        ));
    }

    let mut conn = pool.get().await?;
    let target_name = user::list(&mut conn)
        .await?
        .into_iter()
        .find(|u| u.id == target)
        .map(|u| u.username)
        .ok_or_else(|| Error::new(Status::NotFound, anyhow::anyhow!("no such user")))?;

    let impersonated = Session {
        user_id: Some(target),
        username: Some(target_name.clone()),
        is_logged_in: true,
        redirect_on_login: None,
        is_admin: false,
        view_as: Some(ViewAs {
            admin_id: session.user_id(),
            admin_username: session.username().to_string(),
        }),
    };
    impersonated.save(cookies)?;

    tracing::warn!(
        target,
        target_name = %target_name,
        by = session.user_id(),
        by_name = session.username(),
        "an administrator is viewing the site as another user"
    );

    Ok(Flash::success(
        Redirect::to("/"),
        format!("You are now viewing the site as {target_name}. This is read-only."),
    ))
}

/// Stop viewing as somebody, and become yourself again.
///
/// **Takes no session guard**, which is the point: `Session`'s guard refuses every non-`GET` while
/// impersonating, so a route reached through it could never run here. It reads the cookie itself
/// instead. That is safe because the only thing it can do is *drop* a capability -- the identity it
/// restores comes out of the encrypted cookie's `view_as`, which only [`view_as`] above can write,
/// and it is written there from an [`AdminSession`].
#[post("/admin/users/stop-view-as")]
async fn stop_view_as(
    cookies: &CookieJar<'_>,
    figment: &State<rocket::figment::Figment>,
) -> Result<Flash<Redirect>> {
    let current = Session::from_cookies(cookies);
    let Some(view_as) = current.view_as else {
        return Ok(Flash::warning(
            Redirect::to("/"),
            "You were not viewing the site as anybody.",
        ));
    };

    // **`is_admin` is re-derived from the configured list, never carried back from the cookie.**
    // The rest of the session is being rebuilt from a value the cookie supplied, and trusting it
    // for this one field would make the admin flag restorable from something a previous request
    // wrote -- which is exactly the shape the login path refuses for the same reason.
    let is_admin = crate::auth::is_admin_by_config(view_as.admin_id, figment)?;

    let restored = Session {
        user_id: Some(view_as.admin_id),
        username: Some(view_as.admin_username.clone()),
        is_logged_in: true,
        redirect_on_login: None,
        is_admin,
        view_as: None,
    };
    restored.save(cookies)?;

    tracing::warn!(
        by = view_as.admin_id,
        by_name = %view_as.admin_username,
        "an administrator stopped viewing the site as another user"
    );

    Ok(Flash::success(
        Redirect::to(uri!(show)),
        "You are yourself again.",
    ))
}

pub fn routes() -> Vec<rocket::Route> {
    routes![show, set_status, view_as, stop_view_as]
}
