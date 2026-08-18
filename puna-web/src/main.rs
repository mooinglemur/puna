//! The web binary. One image, two roles.
//!
//! `PUNA_ROLE=web` serves rooms, admin, auth, artifact ingest and the console. `PUNA_ROLE=tracker`
//! serves only the tracker surface, and runs as its own Deployment so that a spike on the most
//! public, least-authenticated part of Puna cannot degrade room creation or the OAuth callback.
//!
//! M0 scope: prove the dependency set links and boots. Routes arrive from M3 onward.

use std::str::FromStr;

use askama::Template;
use askama_web::WebTemplate;
use puna_core::Role;
use rocket::{Build, Rocket, get, routes};

#[derive(rust_embed::RustEmbed)]
#[folder = "./static/"]
struct Assets;

/// Set by `build.rs` from a hash of `static/`, for cache-busting asset URLs.
const STATIC_VERSION: &str = env!("STATIC_VERSION");

#[derive(Template, WebTemplate)]
#[template(path = "health.html")]
struct HealthTemplate {
    role: &'static str,
    version: &'static str,
    static_version: &'static str,
}

/// Liveness. Deliberately touches no database: this answers "is the process alive", and a
/// readiness probe (M3) answers "can it serve", which are different questions with different
/// correct responses to a database blip.
#[get("/health")]
fn health(role: &rocket::State<Role>) -> HealthTemplate {
    HealthTemplate {
        role: role.as_str(),
        version: puna_core::VERSION,
        static_version: STATIC_VERSION,
    }
}

/// Assets are compiled into the binary, so the runtime image carries one file and there is no
/// asset/binary version skew to reason about during a rollout.
#[get("/static/<file..>")]
fn static_file(file: std::path::PathBuf) -> Option<(rocket::http::ContentType, Vec<u8>)> {
    let path = file.to_str()?;
    let asset = Assets::get(path)?;
    let content_type = file
        .extension()
        .and_then(|e| e.to_str())
        .and_then(rocket::http::ContentType::from_extension)
        .unwrap_or(rocket::http::ContentType::Bytes);
    Some((content_type, asset.data.into_owned()))
}

fn build(role: Role) -> Rocket<Build> {
    let rocket = rocket::build()
        .manage(role)
        .mount("/", routes![health, static_file]);

    match role {
        Role::Web => rocket,
        Role::Tracker => rocket,
    }
}

#[rocket::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
        Ok(v) => Role::from_str(&v)?,
        Err(_) => Role::Web,
    };
    tracing::info!(
        role = role.as_str(),
        version = puna_core::VERSION,
        "starting"
    );

    build(role).launch().await?;
    Ok(())
}
