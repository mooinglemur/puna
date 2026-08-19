//! Uploading a generation zip.
//!
//! Ingest runs **synchronously, in the request**, which is a deliberate trade of latency for
//! diagnosability: a malformed zip becomes a 400 on this form, with the reason on screen, rather
//! than a room whose pod crashloops minutes later with the cause buried in a container log.
//!
//! The order is validate, then write, then index. Nothing reaches the filesystem until
//! `inspect` has accepted it, so a rejected upload leaves no trace at all -- which matters
//! because the volume is shared and quota'd across every room in the environment.

use puna_core::artifact::{self, IngestError};
use puna_core::model::generation;
use rocket::form::Form;
use rocket::fs::TempFile;
use rocket::http::Status;
use rocket::response::Redirect;
use rocket::{FromForm, State, get, post, routes};

use crate::auth::LoggedInSession;
use crate::error::{Error, Result};
use crate::gate::{CanCreateRoom, Direct};
use crate::tpl::TplContext;
use crate::{DataDir, UploadLimit};

use askama::Template;
use askama_web::WebTemplate;

#[derive(Template, WebTemplate)]
#[template(path = "generations/upload.html")]
pub struct UploadTemplate {
    base: TplContext,
    error: Option<String>,
    unmatched: Vec<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "generations/show.html")]
pub struct ShowTemplate {
    base: TplContext,
    generation: generation::Generation,
    slots: Vec<generation::Slot>,
    /// True when these bytes were already on file, so the page can say "already uploaded"
    /// rather than implying this upload created it.
    deduplicated: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "generations/list.html")]
pub struct ListTemplate {
    base: TplContext,
    generations: Vec<generation::Generation>,
}

#[derive(FromForm)]
pub struct UploadForm<'r> {
    zip: TempFile<'r>,
}

/// The upload form. Guarded, so someone who may not create a room never sees it.
#[get("/generations/new")]
fn new_form(gate: CanCreateRoom<Direct>) -> UploadTemplate {
    UploadTemplate {
        base: TplContext::new(gate.session().session()),
        error: None,
        unmatched: Vec::new(),
    }
}

/// Your own generations. Deliberately not a global list: generations are content-addressed and
/// shared, so listing every row would show one user's uploads to another.
#[get("/generations")]
async fn list(session: LoggedInSession, pool: &State<puna_core::db::Pool>) -> Result<ListTemplate> {
    let mut conn = pool.get().await?;
    let generations = generation::list_for_user(&mut conn, session.user_id(), 50).await?;
    Ok(ListTemplate {
        base: TplContext::new(session.session()),
        generations,
    })
}

/// `dedup` is set by the upload redirect when the bytes were already on file, so the page can say
/// "already uploaded" rather than implying this upload created it. It rides in the URL because the
/// POST redirects here (so a refresh cannot re-upload tens of megabytes), and a redirect carries
/// nothing else forward.
#[get("/generations/<id>?<dedup>")]
async fn show(
    id: &str,
    dedup: Option<bool>,
    session: LoggedInSession,
    pool: &State<puna_core::db::Pool>,
) -> Result<ShowTemplate> {
    let id = id
        .parse()
        .map_err(|_| Error::new(Status::NotFound, anyhow::anyhow!("not a generation id")))?;
    let mut conn = pool.get().await?;

    let generation = generation::get(&mut conn, id)
        .await?
        .ok_or_else(|| Error::new(Status::NotFound, anyhow::anyhow!("no such generation")))?;
    let slots = generation::slots(&mut conn, id).await?;

    Ok(ShowTemplate {
        base: TplContext::new(session.session()),
        generation,
        slots,
        deduplicated: dedup.unwrap_or(false),
    })
}

#[post("/generations", data = "<form>")]
async fn upload(
    gate: CanCreateRoom<Direct>,
    mut form: Form<UploadForm<'_>>,
    pool: &State<puna_core::db::Pool>,
    data_dir: &State<DataDir>,
    limit: &State<UploadLimit>,
) -> std::result::Result<Redirect, UploadTemplate> {
    let base = TplContext::new(gate.session().session());
    let reject = |message: String, unmatched: Vec<String>| UploadTemplate {
        base: base.clone(),
        error: Some(message),
        unmatched,
    };

    let bytes = match read_upload(&mut form).await {
        Ok(bytes) => bytes,
        Err(message) => return Err(reject(message, Vec::new())),
    };

    // Validate BEFORE anything touches the volume. A rejected upload leaves no trace.
    let meta = match artifact::inspect(&bytes, limit.0) {
        Ok(meta) => meta,
        Err(e) => {
            // A banned file is worth its own log line: it is the one rejection that is about
            // what the archive contains rather than whether it parses.
            if matches!(e, IngestError::BannedFile { .. }) {
                tracing::warn!(user_id = gate.user_id(), "upload refused: {e}");
            } else {
                tracing::info!(user_id = gate.user_id(), "upload rejected: {e}");
            }
            return Err(reject(e.to_string(), Vec::new()));
        }
    };

    // Reported, not fatal: a patch nobody can download is a player who cannot join, and the
    // uploader is the only person able to fix it -- but the rest of the seed is still usable, so
    // refusing the whole upload would be worse than saying so.
    let unmatched = meta.unmatched_patches.clone();

    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let (_, promotion) = match artifact::promote(&data_dir.0, &bytes, &meta, &nonce) {
        Ok(result) => result,
        Err(e) => {
            tracing::error!(error = ?e, "promoting a generation failed");
            return Err(reject(
                "the upload could not be stored; this is a server-side fault and has been logged"
                    .to_string(),
                unmatched,
            ));
        }
    };

    let mut conn = match pool.get().await {
        Ok(conn) => conn,
        Err(e) => {
            tracing::error!(error = ?e, "no database connection for an upload");
            return Err(reject("the database is unavailable".to_string(), unmatched));
        }
    };

    let inserted = match generation::insert(&mut conn, &meta, gate.user_id()).await {
        Ok(inserted) => inserted,
        Err(e) => {
            tracing::error!(error = ?e, "indexing a generation failed");
            return Err(reject(
                "the upload was stored but could not be indexed; this has been logged".to_string(),
                unmatched,
            ));
        }
    };

    tracing::info!(
        user_id = gate.user_id(),
        grant = ?gate.grant(),
        generation = %inserted.id,
        seed = %meta.seed_name,
        slots = meta.slot_count,
        created = inserted.created,
        promotion = ?promotion,
        unmatched = unmatched.len(),
        "generation ingested"
    );

    // POST-redirect-GET, so a refresh does not re-upload tens of megabytes.
    Ok(Redirect::to(format!(
        "/generations/{}?dedup={}",
        inserted.id, !inserted.created
    )))
}

/// Pull the whole upload into memory.
///
/// Rocket has already spilled it to a temp file under its own `limits.data-form` cap, so this is
/// bounded by configuration rather than by the client. Held in memory because hashing, zip
/// parsing and extraction all want random access to the same bytes, and a generation is tens of
/// megabytes rather than gigabytes.
async fn read_upload(form: &mut Form<UploadForm<'_>>) -> std::result::Result<Vec<u8>, String> {
    let path = form
        .zip
        .path()
        .ok_or_else(|| "the upload was empty".to_string())?;
    tokio::fs::read(path)
        .await
        .map_err(|e| format!("the upload could not be read: {e}"))
}

pub fn routes() -> Vec<rocket::Route> {
    routes![new_form, list, show, upload]
}
