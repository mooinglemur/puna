//! The web binary. One image, two roles.
//!
//! `PUNA_ROLE=web` serves rooms, admin, auth, artifact ingest and the console. `PUNA_ROLE=tracker`
//! serves only the tracker surface, and runs as its own Deployment so that a spike on the most
//! public, least-authenticated part of Puna cannot degrade room creation or the OAuth callback.
//!
//! M3 scope: the shell -- Discord OAuth, session guards, health and readiness, an empty landing
//! page and admin page. Rooms, artifacts, the console and the tracker arrive in M4 onwards.

mod auth;
mod error;
mod gate;
mod routes;
mod tpl;

use std::str::FromStr;

use askama::Template;
use askama_web::WebTemplate;
use puna_core::db::Pool;
use puna_core::{Environment, Role};
use rocket::figment::Figment;
use rocket::http::ContentType;
use rocket::response::Redirect;
use rocket::{Build, Rocket, State, catch, catchers, get, routes};

use auth::{AdminSession, LoggedInSession, Session};
use error::Result;
use tpl::TplContext;

/// Rocket's own default `limits.data-form` is 2 MiB, which every real generation zip exceeds.
/// `Rocket.toml` raises it; this is the fallback when it does not, and it is deliberately large
/// enough to be useful rather than small enough to be safe -- the deployment sets the real number.
const DEFAULT_UPLOAD_LIMIT: u64 = 256 * 1024 * 1024;

#[derive(rust_embed::RustEmbed)]
#[folder = "./static/"]
struct Assets;

/// The root of the shared volume, in Rocket's state.
///
/// The web tier's mount is `generations/` and nothing else, by `subPath` -- it cannot reach a
/// room's state directory even though this is spelled as the volume root.
pub struct DataDir(pub std::path::PathBuf);

/// The largest generation zip `inspect` will look at, in bytes.
///
/// Distinct from Rocket's own `limits.data-form`, which caps what is read off the wire. This one
/// bounds what is decompressed, so the two want to move together: Rocket's cap must be the larger
/// of the pair, or an oversized upload is refused with Rocket's generic 413 instead of this
/// module's message naming the actual limit.
pub struct UploadLimit(pub u64);

#[derive(Template, WebTemplate)]
#[template(path = "index.html")]
struct IndexTemplate {
    base: TplContext,
}

#[derive(Template, WebTemplate)]
#[template(path = "admin.html")]
struct AdminTemplate {
    base: TplContext,
}

#[get("/")]
fn index(session: Session) -> IndexTemplate {
    IndexTemplate {
        base: TplContext::new(&session),
    }
}

#[get("/admin")]
fn admin(session: AdminSession) -> AdminTemplate {
    AdminTemplate {
        base: TplContext::new(session.session()),
    }
}

/// Who the session says you are.
///
/// Exercises the middle rung of the guard ladder, which nothing else does yet, and is the
/// quickest way to tell a cookie problem from a permissions problem on a live deployment:
/// anonymous gets redirected to Discord by the 401 catcher, authenticated gets JSON.
#[get("/whoami")]
fn whoami(session: LoggedInSession) -> rocket::serde::json::Json<serde_json::Value> {
    rocket::serde::json::Json(serde_json::json!({
        "user_id": session.user_id().to_string(),  // i64 as a string: JS loses 64-bit precision
        "username": session.username(),
        "is_admin": session.is_admin(),
    }))
}

/// Liveness. Deliberately touches no database.
///
/// This answers "is the process alive"; `/readyz` answers "can it serve". Conflating them means a
/// database blip restarts every web pod, which turns a recoverable outage into a longer one.
#[get("/health")]
fn health() -> &'static str {
    "ok"
}

/// Readiness: a live connection AND a schema this build understands.
///
/// The schema check is the interesting half. The orchestrator owns migrations, so a web pod
/// rolled out ahead of it would otherwise serve reads against columns that do not exist yet.
/// Failing readiness keeps it out of the Service until the orchestrator has caught up.
#[get("/readyz")]
async fn readyz(pool: &State<Pool>) -> Result<&'static str> {
    puna_core::db::assert_schema_current(pool, puna_core::MIGRATIONS).await?;
    Ok("ready")
}

/// Admin-gated, matching the lobby's posture: the metric names and label values describe the
/// fleet's shape, which is not something to publish unauthenticated on the open internet.
#[get("/metrics")]
fn metrics(_session: AdminSession) -> String {
    puna_core::metrics::gather()
}

/// Assets are compiled into the binary, so the runtime image carries one file and there is no
/// asset/binary version skew to reason about during a rollout.
#[get("/static/<file..>")]
fn static_file(file: std::path::PathBuf) -> Option<(ContentType, Vec<u8>)> {
    let path = file.to_str()?;
    let asset = Assets::get(path)?;
    let content_type = file
        .extension()
        .and_then(|e| e.to_str())
        .and_then(ContentType::from_extension)
        .unwrap_or(ContentType::Bytes);
    Some((content_type, asset.data.into_owned()))
}

/// Turn "not logged in" into a login round trip that comes back where the user was.
///
/// Every guarded route returns 401 rather than redirecting itself, so the redirect logic lives in
/// exactly one place and a new guarded route inherits it by existing.
#[catch(401)]
fn unauthorized(request: &rocket::Request<'_>) -> Redirect {
    Redirect::to(format!("/auth/login?redirect={}", request.uri().path()))
}

#[catch(403)]
fn forbidden() -> &'static str {
    "Forbidden"
}

#[catch(404)]
fn not_found() -> &'static str {
    "Not found"
}

fn build(role: Role, figment: Figment, pool: Pool, data_dir: std::path::PathBuf) -> Rocket<Build> {
    // Rocket's `limits.data-form` caps what is read off the wire; this caps what is decompressed.
    // Read the former so the two cannot silently disagree -- a decompression limit above the wire
    // limit is unreachable, and below it turns a legitimate upload into Rocket's generic 413.
    let wire_limit = figment
        .extract::<rocket::Config>()
        .ok()
        .and_then(|config| config.limits.get("data-form"))
        .map(|size| size.as_u64())
        .unwrap_or(DEFAULT_UPLOAD_LIMIT);

    let rocket = rocket::custom(figment.clone())
        .manage(role)
        .manage(figment)
        .manage(pool)
        .manage(DataDir(data_dir))
        .manage(UploadLimit(wire_limit))
        .register("/", catchers![unauthorized, forbidden, not_found])
        // Served by both roles: liveness, readiness and the embedded assets.
        .mount("/", routes![health, readyz, static_file]);

    match role {
        Role::Web => rocket
            .mount("/", routes![index, admin, whoami, metrics])
            .mount("/", routes::generations::routes())
            .mount("/", routes::gates::routes())
            .mount("/auth", auth::routes())
            .attach(rocket_oauth2::OAuth2::<auth::Discord>::fairing("discord")),
        // The tracker tier deliberately gets no OAuth fairing and no Discord credentials: it
        // never initiates a login. It still reads the session cookie (for `members` policy), which
        // needs only the shared ROCKET_SECRET_KEY. Its routes land in M8b.
        Role::Tracker => rocket,
    }
}

#[rocket::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--version") {
        println!("puna-web {}", puna_core::VERSION);
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,puna_web=debug,puna_core=debug".into()),
        )
        .init();

    let role = match std::env::var("PUNA_ROLE") {
        Ok(v) => Role::from_str(&v).map_err(|e| anyhow::anyhow!(e))?,
        Err(_) => Role::Web,
    };

    let database_url =
        std::env::var("DATABASE_URL").map_err(|_| anyhow::anyhow!("DATABASE_URL must be set"))?;
    let environment: Environment = std::env::var("PUNA_ENVIRONMENT")
        .map_err(|_| anyhow::anyhow!("PUNA_ENVIRONMENT must be set"))
        .and_then(|v| Environment::from_str(&v).map_err(|e| anyhow::anyhow!(e)))?;

    puna_core::metrics::init();

    // No migrations here: the orchestrator owns them. This process asserts the schema is current
    // at readiness instead, so two web replicas can never race each other applying them.
    let pool = puna_core::db::get_database_pool(&database_url, None).await?;

    tracing::info!(
        role = role.as_str(),
        environment = environment.as_str(),
        version = puna_core::VERSION,
        "starting"
    );

    // Rocket's own figment, which picks up Rocket.toml via ROCKET_CONFIG. The Discord credentials
    // live in that file and NOT in environment variables -- see the note on `auth::discord_config`.
    let figment = rocket::Config::figment();

    if role == Role::Web {
        // Fail at startup rather than at the first login attempt. A missing Rocket.toml is a
        // deployment mistake, and discovering it when a user clicks "log in" means discovering it
        // from a user.
        auth::discord_config(&figment)
            .map_err(|e| anyhow::anyhow!("Discord OAuth is not configured: {e}"))?;
    }

    let data_dir = std::path::PathBuf::from(
        std::env::var("PUNA_DATA_DIR").unwrap_or_else(|_| "/var/lib/puna".to_string()),
    );

    build(role, figment, pool, data_dir).launch().await?;
    Ok(())
}
