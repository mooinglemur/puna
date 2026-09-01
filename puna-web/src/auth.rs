//! Discord OAuth, the session cookie, and the layered request guards.
//!
//! Adapted from `Archipelago-lobby/community-ap-tools/src/auth.rs`. The flow is deliberately the
//! same -- same Discord application, same `/oauth2/@me` lookup, same immediate token revoke -- so
//! that a user who has already authorized the app sees a redirect rather than a consent dialog.
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
//!     follows a room link and claims a slot -- so authentication and authorization are separate:
//!     anyone may log in, and `CanCreateRoom` (M4) plus `RoomRole` (M5) decide what they may do.
//!
//!   * `is_admin` is re-derived from Puna's own `admins` list on every login and never inherited.
//!     Trusting a flag minted elsewhere would mean adopting another app's admin policy.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use puna_core::db::Pool;
use puna_core::model::user;
use puna_core::model::user::UserStatus;
use reqwest::Url;
use reqwest::header::HeaderValue;
use rocket::figment::value::Dict;
use rocket::figment::{Figment, Profile, Provider};
use rocket::http::{Cookie, CookieJar, Method, SameSite, Status};
use rocket::outcome::Outcome as RocketOutcome;
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
    /// Set only while an administrator is viewing the site as somebody else.
    ///
    /// **While this is set, `user_id`, `username` and `is_admin` describe the person being viewed,
    /// not the person looking**, so every guard, every query and every template resolves to them
    /// with no special case anywhere. `is_admin` is forced to `false`, which is what stops an
    /// administrator seeing admin-only affordances through somebody else's eyes and mistaking them
    /// for what that person sees.
    ///
    /// The real identity lives here, and is used for exactly two things: rendering the banner, and
    /// restoring the session when they stop.
    #[serde(default)]
    pub view_as: Option<ViewAs>,
}

/// Who is really looking, while [`Session::view_as`] is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewAs {
    pub admin_id: i64,
    pub admin_username: String,
}

impl Session {
    pub fn from_request_sync(request: &Request<'_>) -> Self {
        Self::from_cookies(request.cookies())
    }

    /// The session as the cookie carries it, with no guard in the way.
    ///
    /// For the one route that must work *while* impersonating: `Session`'s request guard refuses
    /// every non-`GET` in that state, so "stop viewing as" could not be reached through it. Reading
    /// the jar directly is the deliberate hole, and it is safe because the only thing that route
    /// can do with the result is give a capability up.
    pub fn from_cookies(cookies: &CookieJar<'_>) -> Self {
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
            // --- SET EXPLICITLY. Rocket will NOT set it for us here. ---------------------------
            // Rocket only defaults `Secure` on when Rocket itself terminates TLS
            // (`CookieJar::set_defaults`: `if cookie.secure().is_none() && config.tls_enabled()`).
            // Every Puna deployment terminates TLS upstream -- Envoy for the UI -- so
            // `tls_enabled()` is false and the attribute would never be added. The reasonable
            // assumption that "Rocket handles this" is exactly what makes it invisible.
            //
            // It matters more than the usual amount because the UI and the rooms SHARE A HOSTNAME,
            // differing only by port, and cookies have no port isolation: this cookie is sent to
            // rooms.example.com:41234 -- a pahoa room -- exactly as it is to :443. pahoa neither
            // parses nor logs cookies, and the value is AEAD-encrypted, so it cannot read it. But
            // it is still a bearer credential: anyone who captures it can replay it here. Without
            // `Secure`, a single plaintext request to a room port puts it on the wire in the
            // clear, and there is no HSTS on the shared gateway to upgrade that request.
            //
            // Local development over `http://localhost` is unaffected: browsers treat localhost as
            // a secure context and accept `Secure` cookies there. Developing against a bare LAN
            // address instead would break login, and that is the intended trade.
            .secure(true)
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
        let session = Session::from_request_sync(request);

        // **View-as is READ-ONLY, and this is the whole enforcement.**
        //
        // It lives on the base guard rather than on `LoggedInSession` because it has to be TOTAL:
        // `POST /room/<id>/start` takes a plain `Session` -- an anonymous visitor may start an idle
        // room, which is D8's whole design -- so a check one rung up would leave exactly that route
        // open. Every other guard in this crate composes on this one, so refusing here refuses
        // everywhere, and a write route added tomorrow inherits it without anybody remembering.
        //
        // The alternative was a per-route `NotImpersonating` guard, which is the shape this
        // codebase has been bitten by twice: a check you have to remember is a check somebody
        // forgets, and the failure is a row attributed to a person who did not write it.
        //
        // `/admin/users/stop-view-as` is the one write that must work while impersonating, so it
        // takes no session guard at all and reads the cookie itself.
        if session.view_as.is_some() && !matches!(request.method(), Method::Get | Method::Head) {
            return Outcome::Error((
                Status::Forbidden,
                forbidden(
                    "You are viewing the site as somebody else, which is read-only. \
                     Stop viewing as them to make changes.",
                ),
            ));
        }

        Outcome::Success(session)
    }
}

/// Account standing, remembered briefly so a ban does not cost a query on every request.
///
/// **The session is deliberately stateless, and this is the one thing that reaches past it.** A
/// cookie lasts 31 days, so a ban enforced only at login would let a banned account keep acting for
/// a month, which is not a ban. The cost is one primary-key lookup per authenticated request, and
/// the cache is what keeps it off the hot path.
///
/// The TTL is the honest cost of the design: a ban takes effect within [`STANDING_TTL`] rather than
/// instantly. Seconds, not minutes, and it is stated rather than hidden because somebody banning an
/// account mid-incident needs to know whether to wait.
/// What was read, and when, so the entry can expire.
type CachedStanding = (UserStatus, Option<String>, Instant);

static STANDING: LazyLock<Mutex<HashMap<i64, CachedStanding>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

const STANDING_TTL: Duration = Duration::from_secs(10);

/// Forget one account's cached standing, so a change takes effect on the next request.
///
/// Called by the admin route that sets it. **The cache is per process and there are two web
/// replicas**, so this makes it immediate for the replica that served the form and leaves the other
/// to expire, which is why the TTL is short rather than long, and why this is an optimization
/// rather than the mechanism.
pub fn forget_standing(discord_id: i64) {
    if let Ok(mut cache) = STANDING.lock() {
        cache.remove(&discord_id);
    }
}

/// This account's standing, from the cache or the database.
///
/// **Fails CLOSED.** An `Err` means the answer could not be determined, and the caller refuses the
/// request rather than letting it through, the same rule `CanCreateRoom` states for the same
/// lookup, and a check that fails open is not a check. The first draft of this returned `Option`
/// and the caller skipped the ban on `None`, which meant a database blip briefly un-banned
/// everybody: a security control whose failure mode is "grant the thing".
///
/// It costs nothing real to fail closed here. Every route behind this guard reads the database
/// anyway, so a pool that cannot answer is a request that was going to fail regardless: the only
/// difference is which error it fails with.
async fn standing(
    request: &Request<'_>,
    user_id: i64,
) -> anyhow::Result<(UserStatus, Option<String>)> {
    if let Ok(cache) = STANDING.lock()
        && let Some((status, note, at)) = cache.get(&user_id)
        && at.elapsed() < STANDING_TTL
    {
        return Ok((*status, note.clone()));
    }

    let pool = request
        .rocket()
        .state::<Pool>()
        .ok_or_else(|| anyhow::anyhow!("no database pool in Rocket state"))?;
    let mut conn = pool.get().await.map_err(|e| anyhow::anyhow!(e))?;
    let found = puna_core::model::user::status_of(&mut conn, user_id).await?;

    // A session-bearing request with no `users` row should not happen -- login upserts one. Treated
    // as active because it is an anomaly in OUR bookkeeping rather than a sanction, and logged so
    // it is not silent; refusing here would lock somebody out over a missing row nobody meant.
    let (status, note) = found.unwrap_or_else(|| {
        tracing::warn!(user_id, "a session names a user with no row");
        (UserStatus::Active, None)
    });

    if let Ok(mut cache) = STANDING.lock() {
        cache.insert(user_id, (status, note.clone(), Instant::now()));
    }
    Ok((status, note))
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for LoggedInSession {
    type Error = Error;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        // Through the `Session` guard rather than `from_request_sync`, so the read-only rule above
        // applies to everything built on this one. Calling the sync constructor here would have
        // quietly exempted every authenticated write route from it.
        let session = match request.guard::<Session>().await {
            RocketOutcome::Success(session) => session,
            RocketOutcome::Error(e) => return RocketOutcome::Error(e),
            RocketOutcome::Forward(f) => return RocketOutcome::Forward(f),
        };

        let (true, Some(user_id)) = (session.is_logged_in, session.user_id) else {
            return Outcome::Error((Status::Unauthorized, unauthorized("not logged in")));
        };

        let (status, note) = match standing(request, user_id).await {
            Ok(pair) => pair,
            // Fail closed: unreadable standing is not permission to act.
            Err(e) => {
                tracing::error!(user_id, error = %e, "could not read account standing");
                return Outcome::Error((
                    Status::ServiceUnavailable,
                    Error::new(Status::ServiceUnavailable, e),
                ));
            }
        };

        if !status.may_act() {
            // The reason is shown, because a sanction nobody can be told the cause of is one
            // nobody can appeal. The note is written by an administrator, for this person to read.
            let message = match note {
                Some(why) => format!("This account is banned. {why}"),
                None => "This account is banned.".to_string(),
            };
            return Outcome::Error((
                Status::Forbidden,
                Error::new(Status::Forbidden, anyhow::anyhow!(message)),
            ));
        }

        Outcome::Success(LoggedInSession(session))
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

/// Whether this id is an administrator, going to the configured list rather than to any session.
///
/// Exists for one caller: restoring an administrator's own session when they stop viewing the site
/// as somebody else. The rest of that session is rebuilt from a value the cookie carried, and
/// `is_admin` deliberately is not -- it is re-derived here, the same way login does it, so the flag
/// can never be restored from something a previous request wrote.
pub fn is_admin_by_config(user_id: i64, figment: &Figment) -> Result<bool> {
    Ok(is_admin(user_id, &discord_config(figment)?))
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

    // **Refused before a cookie is minted.** The `LoggedInSession` guard also turns a banned
    // account away, so this is not the only defense -- it is the one that makes the refusal
    // legible. Without it a banned person logs in successfully, lands on the site, and is then
    // told no by every page they touch, which reads as the site being broken rather than as a
    // decision somebody made about them.
    //
    // The upsert above runs first on purpose: `last_seen_at` should record that they came back,
    // and the admin table showing a banned account still trying is worth more than the write costs.
    if let Some((status, note)) = user::status_of(&mut conn, discord_id).await?
        && !status.may_act()
    {
        tracing::info!(user_id = discord_id, "a banned account attempted to log in");
        let message = match note {
            Some(why) => format!("This account is banned. {why}"),
            None => "This account is banned.".to_string(),
        };
        return Err(Error::new(Status::Forbidden, anyhow::anyhow!(message)));
    }

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
    use rocket::local::blocking::Client;
    use rocket::{get, post};

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

    // Two shapes, and both matter. `/read` takes the base guard, which is what
    // `POST /room/<id>/start` does -- an anonymous visitor may start an idle room (D8), so a write
    // route genuinely reaches production holding only a `Session`. `/write-logged-in` stands for
    // everything else.
    #[get("/read")]
    fn read(session: Session) -> String {
        session.username.unwrap_or_default()
    }

    #[post("/write")]
    fn write(_session: Session) -> &'static str {
        "wrote"
    }

    #[post("/write-logged-in")]
    fn write_logged_in(_session: LoggedInSession) -> &'static str {
        "wrote"
    }

    fn client() -> Client {
        // A fixed key, so the private cookie this test writes is the one the guard reads back.
        let figment = rocket::Config::figment()
            .merge(("secret_key", "hPRYyVRiMyxpw5sBB1XeCMN1kFsDCqKvBi9JDMuCMkQ="));
        Client::untracked(rocket::custom(figment).mount("/", routes![read, write, write_logged_in]))
            .expect("a client")
    }

    fn cookie_for(session: &Session) -> Cookie<'static> {
        Cookie::new(COOKIE_NAME, serde_json::to_string(session).expect("json"))
    }

    fn ordinary() -> Session {
        Session {
            user_id: Some(7),
            username: Some("kai".into()),
            is_logged_in: true,
            ..Session::default()
        }
    }

    fn impersonated() -> Session {
        Session {
            view_as: Some(ViewAs {
                admin_id: 1,
                admin_username: "troy".into(),
            }),
            ..ordinary()
        }
    }

    /// **The property the whole feature rests on: viewing as somebody is READ-ONLY.**
    ///
    /// Asserted through a real router with a real private cookie, because the rule lives in a
    /// request guard and nothing short of dispatching a request exercises it. The `POST` taking a
    /// bare `Session` is the case that matters most: it is the shape `/room/<id>/start` has, and
    /// a rule enforced one guard higher would leave exactly that route open while looking correct.
    #[test]
    fn a_write_is_refused_while_viewing_as_somebody_else() {
        let client = client();

        // The same session, minus the impersonation, may read and write.
        for (method, path) in [("GET", "/read"), ("POST", "/write")] {
            let request = match method {
                "GET" => client.get(path),
                _ => client.post(path),
            };
            let response = request.private_cookie(cookie_for(&ordinary())).dispatch();
            assert_eq!(
                response.status(),
                Status::Ok,
                "an ordinary session was refused {method} {path}"
            );
        }

        // **And the ban check fails CLOSED**, which this rig proves for free: it mounts no database
        // pool, so `LoggedInSession` cannot determine standing -- and refuses rather than assuming
        // the account is fine. The first draft returned `Option` and let the request through on
        // `None`, which meant a database blip briefly un-banned everybody.
        let response = client
            .post("/write-logged-in")
            .private_cookie(cookie_for(&ordinary()))
            .dispatch();
        assert_eq!(
            response.status(),
            Status::ServiceUnavailable,
            "unreadable account standing let an authenticated write through"
        );

        // Reading as somebody else works -- that is the entire point.
        let response = client
            .get("/read")
            .private_cookie(cookie_for(&impersonated()))
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(
            response.into_string().as_deref(),
            Some("kai"),
            "the page should render as the person being viewed"
        );

        // Writing does not, through either guard shape -- and `/write-logged-in` answers 403 rather
        // than the 503 above, because the read-only refusal happens in the `Session` guard that
        // `LoggedInSession` calls first. That ordering is the point: impersonation is refused before
        // anything else is consulted.
        for path in ["/write", "/write-logged-in"] {
            let response = client
                .post(path)
                .private_cookie(cookie_for(&impersonated()))
                .dispatch();
            assert_eq!(
                response.status(),
                Status::Forbidden,
                "POST {path} was allowed while viewing as somebody else"
            );
        }
    }

    /// An impersonated session names the person being viewed, and carries no admin rights.
    ///
    /// Serialization matters on its own: the cookie is the only place this state lives, so a field
    /// that failed to round-trip would silently drop somebody back into their own identity, or
    /// worse, leave them impersonating with no way back and no banner saying so.
    #[test]
    fn the_impersonation_state_round_trips_through_the_cookie() {
        let raw = serde_json::to_string(&impersonated()).expect("serializes");
        let back: Session = serde_json::from_str(&raw).expect("parses");

        assert_eq!(back.user_id, Some(7));
        assert!(
            !back.is_admin,
            "an impersonated session must never be admin"
        );
        let view_as = back.view_as.expect("the way back survived");
        assert_eq!(view_as.admin_id, 1);
        assert_eq!(view_as.admin_username, "troy");

        // And an ordinary session still parses, including one written before this field existed --
        // `#[serde(default)]` is what stops a deploy logging everybody out.
        let old = r#"{"user_id":7,"username":"kai","is_logged_in":true,"is_admin":true}"#;
        let parsed: Session = serde_json::from_str(old).expect("an older cookie still parses");
        assert!(parsed.view_as.is_none());
        assert!(parsed.is_admin);
    }
}
