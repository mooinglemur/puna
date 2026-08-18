//! Discord OAuth, the session cookie, and the layered request guards.
//!
//! Adapted from `Archipelago-lobby/community-ap-tools/src/auth.rs`. The flow is deliberately the
//! same -- same Discord application, same `/oauth2/@me` lookup, same immediate token revoke -- so
//! that a user who has already authorised the app sees a redirect rather than a consent dialog.
//!
//! Three things differ from the original, each for a reason:
//!
//!   * The cookie is `punasession`. The lobby uses `session` and community-ap-tools uses
//!     `apsession`; a third name is required, not cosmetic. These apps can share a parent domain
//!     and `SameSite=Lax`, and two apps writing one cookie name with different secret keys
//!     produces a logout loop that looks like random session loss.
//!
//!   * Login is NOT gated. community-ap-tools rejects anyone not already in a team, because every
//!     one of its pages is staff-only. Puna's public surface is genuinely public -- a player
//!     follows a room link and claims a slot -- so authentication and authorisation are separate:
//!     anyone may log in, and `CanCreateRoom` (M4) plus `RoomRole` (M5) decide what they may do.
//!
//!   * `is_admin` is re-derived from Puna's own `admins` list on every login and never inherited.
//!     Trusting a flag minted elsewhere would mean adopting another app's admin policy.

use std::str::FromStr;

use puna_core::db::Pool;
use puna_core::model::user;
use reqwest::Url;
use reqwest::header::HeaderValue;
use rocket::figment::value::Dict;
use rocket::figment::{Figment, Profile, Provider};
use rocket::http::{Cookie, CookieJar, SameSite, Status};
use rocket::request::{FromRequest, Outcome};
use rocket::response::Redirect;
use rocket::time::OffsetDateTime;
use rocket::time::ext::NumericalDuration;
use rocket::{Request, State, get, routes};
use rocket_oauth2::{OAuth2, TokenResponse};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, forbidden, unauthorized};

/// Marker type keying the `rocket_oauth2` fairing to the `[default.oauth.discord]` config block.
pub struct Discord;

const COOKIE_NAME: &str = "punasession";

/// What the encrypted session cookie carries.
///
/// Stateless by design: no server-side session store, so a request costs no database round trip
/// to identify its caller and the two web replicas share nothing.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Session {
    pub user_id: Option<i64>,
    pub username: Option<String>,
    pub is_logged_in: bool,
    pub redirect_on_login: Option<String>,
    #[serde(default)]
    pub is_admin: bool,
}

impl Session {
    pub fn from_request_sync(request: &Request<'_>) -> Self {
        let cookies = request.cookies();
        let Some(raw) = cookies.get_private(COOKIE_NAME) else {
            return Session::default();
        };

        match serde_json::from_str::<Session>(raw.value()) {
            Ok(session) => session,
            Err(_) => {
                // A cookie we cannot decode is a cookie from another key or an older shape.
                // Dropping it turns a permanent failure into one extra login.
                cookies.remove_private(COOKIE_NAME);
                Session::default()
            }
        }
    }

    pub fn save(&self, cookies: &CookieJar<'_>) -> Result<()> {
        let serialized = serde_json::to_string(self)?;
        let cookie = Cookie::build((COOKIE_NAME, serialized))
            .expires(OffsetDateTime::now_utc() + 31.days())
            .same_site(SameSite::Lax)
            .build();
        cookies.add_private(cookie);
        Ok(())
    }
}

/// A session that is definitely authenticated. Yields 401 when it is not.
pub struct LoggedInSession(Session);

impl LoggedInSession {
    pub fn user_id(&self) -> i64 {
        self.0
            .user_id
            .expect("LoggedInSession is only constructed with a user_id")
    }

    pub fn username(&self) -> &str {
        self.0.username.as_deref().unwrap_or("unknown")
    }

    pub fn is_admin(&self) -> bool {
        self.0.is_admin
    }

    pub fn session(&self) -> &Session {
        &self.0
    }
}

/// A session belonging to a global Puna admin. Yields 401 when anonymous, 403 when merely a user.
pub struct AdminSession(LoggedInSession);

impl std::ops::Deref for AdminSession {
    type Target = LoggedInSession;
    fn deref(&self) -> &LoggedInSession {
        &self.0
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Session {
    type Error = Error;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        Outcome::Success(Session::from_request_sync(request))
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for LoggedInSession {
    type Error = Error;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let session = Session::from_request_sync(request);
        if session.is_logged_in && session.user_id.is_some() {
            return Outcome::Success(LoggedInSession(session));
        }
        Outcome::Error((Status::Unauthorized, unauthorized("not logged in")))
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for AdminSession {
    type Error = Error;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let session = Session::from_request_sync(request);
        if !session.is_logged_in {
            return Outcome::Error((Status::Unauthorized, unauthorized("not logged in")));
        }
        if !session.is_admin {
            return Outcome::Error((Status::Forbidden, forbidden("not an administrator")));
        }
        Outcome::Success(AdminSession(LoggedInSession(session)))
    }
}

/// Pull `[default.oauth.discord]` out of the figment.
///
/// This reads the RAW figment rather than a typed Rocket config, and that is why Puna's Discord
/// credentials MUST arrive in a mounted `Rocket.toml` with `ROCKET_CONFIG` pointing at it, not as
/// `ROCKET_*` environment variables: those merge into the GLOBAL profile, while this looks in
/// `Profile::Default`, so an env-supplied client id is simply not here.
pub fn discord_config(figment: &Figment) -> anyhow::Result<Dict> {
    Ok(figment
        .data()?
        .get(&Profile::Default)
        .ok_or_else(|| anyhow::anyhow!("no default profile in the Rocket config"))?
        .get("oauth")
        .ok_or_else(|| anyhow::anyhow!("no [default.oauth] section; is Rocket.toml mounted?"))?
        .as_dict()
        .ok_or_else(|| anyhow::anyhow!("[default.oauth] is not a table"))?
        .get("discord")
        .ok_or_else(|| anyhow::anyhow!("no [default.oauth.discord] section"))?
        .as_dict()
        .ok_or_else(|| anyhow::anyhow!("[default.oauth.discord] is not a table"))?
        .clone())
}

/// Is this Discord id in Puna's own `admins` list?
pub fn is_admin(user_id: i64, config: &Dict) -> bool {
    config
        .get("admins")
        .and_then(|v| v.as_array())
        .is_some_and(|admins| admins.contains(&user_id.into()))
}

#[get("/login?<redirect>")]
fn login(
    oauth2: OAuth2<Discord>,
    mut session: Session,
    redirect: Option<String>,
    cookies: &CookieJar<'_>,
) -> Result<Redirect> {
    // Only relative paths. An absolute URL here would make the login endpoint an open redirect:
    // /auth/login?redirect=https://evil.example sends the user somewhere else after a real
    // Discord login, which is exactly the shape a phishing link wants. `//host` is rejected too,
    // since browsers read it as protocol-relative and therefore absolute.
    if let Some(target) = redirect
        && target.starts_with('/')
        && !target.starts_with("//")
    {
        session.redirect_on_login = Some(target);
    }

    session.save(cookies)?;
    Ok(oauth2.get_redirect(cookies, &["identify"])?)
}

#[get("/logout")]
fn logout(cookies: &CookieJar<'_>) -> Redirect {
    cookies.remove_private(COOKIE_NAME);
    Redirect::to("/")
}

#[get("/oauth")]
async fn oauth_callback(
    mut session: Session,
    token: TokenResponse<Discord>,
    cookies: &CookieJar<'_>,
    figment: &State<Figment>,
    pool: &State<Pool>,
) -> Result<Redirect> {
    let access_token = token.access_token();
    let client = reqwest::Client::new();
    let discord_user = fetch_discord_user(&client, access_token).await?;

    let config = discord_config(figment)?;
    let client_id = config
        .get("client_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("client_id missing from the discord config"))?;
    let client_secret = config
        .get("client_secret")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("client_secret missing from the discord config"))?;

    // Revoke immediately: Puna needs the identity, not ongoing access, so holding the token would
    // be storing a credential it has no use for.
    revoke_token(&client, client_id, client_secret, access_token).await?;

    let discord_id: i64 = discord_user.id.parse()?;

    let mut conn = pool.get().await.map_err(|e| anyhow::anyhow!(e))?;
    user::upsert(&mut conn, discord_id, &discord_user.username).await?;

    session.user_id = Some(discord_id);
    session.username = Some(discord_user.username);
    session.is_logged_in = true;
    // Re-derived here, every login, from Puna's own list.
    session.is_admin = is_admin(discord_id, &config);

    let destination = session
        .redirect_on_login
        .take()
        .unwrap_or_else(|| "/".into());
    session.save(cookies)?;

    Ok(Redirect::to(destination))
}

#[derive(Deserialize)]
struct DiscordMeResponse {
    user: DiscordUser,
}

#[derive(Deserialize)]
struct DiscordUser {
    id: String,
    username: String,
}

/// `/oauth2/@me`, not `/users/@me`: it answers for the token itself, so it confirms the token is
/// ours rather than merely valid somewhere.
async fn fetch_discord_user(client: &reqwest::Client, token: &str) -> Result<DiscordUser> {
    let mut request = reqwest::Request::new(
        reqwest::Method::GET,
        Url::from_str("https://discord.com/api/oauth2/@me")?,
    );
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    let response = client.execute(request).await?.error_for_status()?;
    let body = response.text().await?;
    Ok(serde_json::from_str::<DiscordMeResponse>(&body)?.user)
}

async fn revoke_token(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    token: &str,
) -> Result<()> {
    #[derive(Serialize)]
    struct RevokeForm<'a> {
        token: &'a str,
    }

    client
        .post("https://discord.com/api/oauth2/token/revoke")
        .basic_auth(client_id, Some(client_secret))
        .form(&RevokeForm { token })
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

pub fn routes() -> Vec<rocket::Route> {
    routes![login, logout, oauth_callback]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocket::figment::value::Value;

    fn config_with_admins(ids: Vec<i64>) -> Dict {
        let mut dict = Dict::new();
        dict.insert(
            "admins".to_string(),
            Value::from(ids.into_iter().map(Value::from).collect::<Vec<_>>()),
        );
        dict
    }

    #[test]
    fn admins_are_matched_exactly() {
        let config = config_with_admins(vec![1234567890, 42]);
        assert!(is_admin(42, &config));
        assert!(is_admin(1234567890, &config));
        assert!(!is_admin(43, &config));
    }

    #[test]
    fn a_missing_admins_list_grants_nobody() {
        // A misrendered Rocket.toml must fail closed. Defaulting to "everyone is an admin" on a
        // public deployment is the worst possible reading of a missing key.
        assert!(!is_admin(42, &Dict::new()));
    }

    #[test]
    fn an_unparseable_session_is_treated_as_anonymous() {
        let session: std::result::Result<Session, _> = serde_json::from_str("{\"garbage\":true}");
        // Missing fields fall back to Default, which is anonymous -- never an authenticated
        // session with a null user.
        let session = session.unwrap_or_default();
        assert!(!session.is_logged_in);
        assert!(session.user_id.is_none());
        assert!(!session.is_admin);
    }
}
