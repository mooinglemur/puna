//! The web binary. One image, two roles.
//!
//! `PUNA_ROLE=web` serves rooms, admin, auth, artifact ingest and the console. `PUNA_ROLE=tracker`
//! serves only the tracker surface, and runs as its own Deployment so that a spike on the most
//! public, least-authenticated part of Puna cannot degrade room creation or the OAuth callback.
//!
//! M3 scope: the shell -- Discord OAuth, session guards, health and readiness, an empty landing
//! page and admin page. Rooms, artifacts, the console and the tracker arrive in M4 onwards.

mod auth;
mod commands;
mod cookies;
mod digest;
mod error;
mod gate;
mod guards;
mod metrics_listener;
mod params;
mod routes;
mod tpl;
mod upstream;

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

/// The DNS name rooms are advertised on, e.g. `mw.ionium-dev.us`.
///
/// The web tier needs it for one thing: **embedding the address into a patch**. A name rather than
/// the literal VIP because it is also the name on the room certificate, so a patch that carried the
/// address instead would fail TLS verification the day the address moves.
pub struct AdvertiseHost(pub String);

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

/// Rocket answers 422 when a route matches but a parameter or form field will not parse.
///
/// The common way to reach it is a truncated room link -- `/room/<half a uuid>` matches the route
/// and then fails `FromParam` -- so a bare status code would leave someone staring at a number.
/// The status is left alone rather than remapped to 404: form submissions land here too, and
/// telling someone their input was not found would be worse than telling them nothing.
#[catch(422)]
fn unprocessable() -> &'static str {
    "That link or form could not be read. If you followed a link, it may have been truncated."
}

/// The startup inputs that are not Rocket's own.
///
/// A struct rather than six more parameters: every field here is read from the environment once and
/// then only put into Rocket's state, so threading them individually buys nothing and makes the
/// order of two `String`s something a caller could get wrong.
pub struct Settings {
    pub data_dir: std::path::PathBuf,
    pub advertise_host: String,
    pub upstream: upstream::Upstream,
    pub tracker_cache_max: usize,
}

fn build(
    role: Role,
    environment: Environment,
    figment: Figment,
    pool: Pool,
    waiters: std::sync::Arc<commands::Waiters>,
    settings: Settings,
) -> Rocket<Build> {
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
        .manage(environment)
        .manage(figment)
        .manage(pool)
        .manage(DataDir(settings.data_dir))
        .manage(AdvertiseHost(settings.advertise_host))
        .manage(settings.upstream)
        .manage(routes::tracker::Memo::default())
        .manage(routes::tracker::NameCache::default())
        .manage(routes::tracker::TrackerCacheMax(settings.tracker_cache_max))
        .manage(UploadLimit(wire_limit))
        .manage(waiters)
        .register(
            "/",
            catchers![unauthorized, forbidden, not_found, unprocessable],
        )
        // Served by both roles: liveness, readiness and the embedded assets.
        .mount("/", routes![health, readyz, static_file]);

    let rocket = match role {
        Role::Web => rocket
            .mount("/", routes![index, admin, whoami, metrics])
            .mount("/", routes::downloads::routes())
            .mount("/", routes::generations::routes())
            .mount("/", routes::console::routes())
            .mount("/", routes::gates::routes())
            .mount("/", routes::rooms::routes())
            .mount("/auth", auth::routes())
            .attach(rocket_oauth2::OAuth2::<auth::Discord>::fairing("discord")),
        // The tracker tier deliberately gets no OAuth fairing and no Discord credentials: it
        // never initiates a login. It still reads the session cookie (for `members` policy), which
        // needs only the shared ROCKET_SECRET_KEY -- and its 401 catcher redirects to the web
        // tier's login on the same hostname, which is what makes that split work.
        Role::Tracker => rocket.mount("/", routes::tracker::routes()),
    };

    // --- ATTACHED LAST, AND THAT IS NOT COSMETIC ------------------------------------------------
    // Response fairings run in attach order, so this has to come after the OAuth2 fairing -- the
    // whole point is to reach `rocket_oauth2_state`, which that fairing sets and offers no way to
    // configure. Attached before it, this would run first, see no cookie, and do nothing.
    //
    // Both roles, though only the web role sets cookies today: the tracker reads the session but
    // never writes one. Symmetry costs nothing and means a cookie added to the tracker's 401 path
    // later is covered without anyone remembering this.
    rocket.attach(cookies::SecureCookies)
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

    // Scoped to the role, so this process exports only what it can actually compute. A web replica
    // publishing `puna_orchestrator_leader 0` is not a harmless extra series -- it is a zero that
    // alert expressions have to know to exclude.
    puna_core::metrics::init(match role {
        Role::Web => puna_core::metrics::Component::Web,
        Role::Tracker => puna_core::metrics::Component::Tracker,
    });

    // Started before the pool, and before Rocket, so a tier that is slow to reach Postgres is still
    // scrapeable while it tries. It registers nothing itself -- `init()` above owns the registry --
    // so ordering against anything else here does not matter.
    tokio::spawn(metrics_listener::serve());

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

    let advertise_host = std::env::var("PUNA_ADVERTISE_HOST")
        .map_err(|_| anyhow::anyhow!("PUNA_ADVERTISE_HOST must be set"))?;

    // How the tracker tier reaches a room. In-cluster by default -- no hairpin through the public
    // address, and the room's traffic never leaves -- with the public route as a switch for running
    // this outside a cluster. TLS is verified against `advertise_host` either way, because that is
    // the only name the room certificate carries.
    let upstream = upstream::Upstream {
        advertise_host: advertise_host.clone(),
        route: match std::env::var("PUNA_ROOM_ROUTE").as_deref() {
            Ok("public") => upstream::Route::Public,
            _ => upstream::Route::Service {
                namespace: std::env::var("PUNA_NAMESPACE")
                    .unwrap_or_else(|_| "puna-dev".to_string()),
            },
        },
        // A room that does not answer promptly is a room that is down, and this request is holding
        // a worker while it waits. The cached document is the fallback and it is a better answer
        // than a long spinner.
        timeout: std::time::Duration::from_secs(5),
    };

    let tracker_cache_max = std::env::var("PUNA_TRACKER_CACHE_MAX")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(2 * 1024 * 1024);

    // One `LISTEN` for the whole process, feeding every console request waiting on a result.
    // **Only the web role**: the tracker tier mounts no console, so a connection held there would
    // be a Postgres session per replica for notifications nothing consumes.
    let waiters = std::sync::Arc::new(commands::Waiters::default());
    if role == Role::Web {
        tokio::spawn(commands::listen(
            database_url.clone(),
            std::sync::Arc::clone(&waiters),
        ));
    }

    build(
        role,
        environment,
        figment,
        pool,
        waiters,
        Settings {
            data_dir,
            advertise_host,
            upstream,
            tracker_cache_max,
        },
    )
    .launch()
    .await?;
    Ok(())
}
