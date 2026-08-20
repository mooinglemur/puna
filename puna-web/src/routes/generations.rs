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
use puna_core::model::{generation, names};
use rocket::form::Form;
use rocket::fs::TempFile;
use rocket::http::Status;
use rocket::response::Redirect;
use rocket::{FromForm, State, get, post, routes};

use crate::auth::{AdminSession, LoggedInSession};
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

    // **Attempted even on a dedup**, which is what makes re-uploading a zip the repair for a
    // generation ingested before this cache existed. The write is an upsert, so doing it again
    // costs a statement and changes nothing.
    let names_cached = cache_names(&mut conn, &data_dir.0, &meta.sha256, inserted.id).await;

    tracing::info!(
        user_id = gate.user_id(),
        grant = ?gate.grant(),
        generation = %inserted.id,
        seed = %meta.seed_name,
        slots = meta.slot_count,
        created = inserted.created,
        promotion = ?promotion,
        unmatched = unmatched.len(),
        names_cached,
        "generation ingested"
    );

    // POST-redirect-GET, so a refresh does not re-upload tens of megabytes.
    Ok(Redirect::to(format!(
        "/generations/{}?dedup={}",
        inserted.id, !inserted.created
    )))
}

/// Fill the tracker's name cache for a generation, from the seed just written to disk.
///
/// **Never fatal, and that is the design rather than laziness.** A generation whose names did not
/// cache is a perfectly usable generation with a slightly worse tracker — the tracker renders raw
/// ids, exactly as the reference does for a name it cannot resolve — so refusing an upload over it
/// would trade something that matters for something that does not. Every failure is warned with
/// the generation id, and the repair is the admin rebuild.
///
/// Reads the **promoted** seed rather than reaching back into the zip in memory, so this is the
/// same path the rebuild takes. One reader, so a bug here is a bug in both rather than in whichever
/// one nobody exercised. The read is synchronous, like the zip work above it in this handler.
async fn cache_names(
    conn: &mut diesel_async::AsyncPgConnection,
    data_dir: &std::path::Path,
    sha256: &[u8; 32],
    generation_id: puna_core::ids::GenerationId,
) -> bool {
    let paths = artifact::GenerationPaths::new(data_dir, sha256);

    let seed = match std::fs::read(paths.seed()) {
        Ok(seed) => seed,
        Err(e) => {
            tracing::warn!(
                generation = %generation_id,
                error = %e,
                "could not read the promoted seed to build the tracker's name cache"
            );
            return false;
        }
    };

    let tables = match artifact::seed_names(&seed) {
        Ok(tables) => tables,
        Err(e) => {
            tracing::warn!(
                generation = %generation_id,
                error = %e,
                "could not extract name tables from the seed"
            );
            return false;
        }
    };

    let approximate_bytes = tables.approximate_bytes();
    match names::store(conn, generation_id, &tables).await {
        Ok(()) => {
            tracing::info!(
                generation = %generation_id,
                games = tables.games.len(),
                slots_with_locations = tables.slot_locations.len(),
                approximate_bytes,
                "cached the tracker's name tables"
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                generation = %generation_id,
                error = %e,
                "could not store the tracker's name cache"
            );
            false
        }
    }
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

/// Rebuild the tracker's name cache for every generation that has none.
///
/// **This is the backfill**, and it is a route rather than a migration for one reason: the names
/// come out of a file on a volume Postgres cannot see, so the repair has to run somewhere with the
/// mount. That is this tier and only this tier.
///
/// Safe to run repeatedly — it only touches generations with nothing cached, and the write is an
/// upsert. A generation whose seed is missing from disk is reported and skipped rather than failing
/// the run, because one unreadable seed must not stop the other forty being repaired.
#[post("/admin/generations/rebuild-names")]
async fn rebuild_all_names(
    _session: AdminSession,
    pool: &State<puna_core::db::Pool>,
    data_dir: &State<DataDir>,
) -> Result<String> {
    let mut conn = pool.get().await?;
    let pending = names::uncached(&mut conn).await?;

    let mut rebuilt = 0usize;
    let mut failed = Vec::new();
    for generation in &pending {
        if cache_names(&mut conn, &data_dir.0, &generation.sha256, generation.id).await {
            rebuilt += 1;
        } else {
            failed.push(generation.id.to_string());
        }
    }

    tracing::info!(
        considered = pending.len(),
        rebuilt,
        failed = failed.len(),
        "rebuilt the tracker's name cache"
    );

    Ok(format!(
        "{} generation(s) had no cached names; {rebuilt} rebuilt, {} failed{}\n",
        pending.len(),
        failed.len(),
        if failed.is_empty() {
            String::new()
        } else {
            format!(" ({})", failed.join(", "))
        }
    ))
}

/// Rebuild one generation's names whether or not it already has them.
///
/// The repair for a cache that is present but wrong — which the backfill above deliberately will
/// not touch, since from its side a wrong cache and a right one look the same.
#[post("/admin/generations/<id>/rebuild-names")]
async fn rebuild_names(
    _session: AdminSession,
    id: &str,
    pool: &State<puna_core::db::Pool>,
    data_dir: &State<DataDir>,
) -> Result<String> {
    // Parsed here rather than through a `FromParam`, matching `show` above: the generation page
    // already takes its id this way and one convention per route set is worth more than a type.
    let id: puna_core::ids::GenerationId = id
        .parse()
        .map_err(|_| Error::new(Status::NotFound, anyhow::anyhow!("not a generation id")))?;

    let mut conn = pool.get().await?;
    let generation = generation::get(&mut conn, id)
        .await?
        .ok_or_else(|| Error::new(Status::NotFound, anyhow::anyhow!("no such generation")))?;

    let sha256: [u8; 32] = generation.sha256.clone().try_into().map_err(|_| {
        Error::new(
            Status::InternalServerError,
            anyhow::anyhow!("this generation's stored hash is not 32 bytes"),
        )
    })?;

    if cache_names(&mut conn, &data_dir.0, &sha256, id).await {
        Ok(format!("rebuilt the name cache for {id}\n"))
    } else {
        Err(Error::new(
            Status::InternalServerError,
            anyhow::anyhow!("could not rebuild the name cache; the reason has been logged"),
        ))
    }
}

pub fn routes() -> Vec<rocket::Route> {
    routes![
        new_form,
        list,
        show,
        upload,
        rebuild_all_names,
        rebuild_names
    ]
}
