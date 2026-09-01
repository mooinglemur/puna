//! Uploading a generation zip.
//!
//! Ingest runs **synchronously, in the request**, which is a deliberate trade of latency for
//! diagnosability: a malformed zip becomes a 400 on this form, with the reason on screen, rather
//! than a room whose pod crashloops minutes later with the cause buried in a container log.
//!
//! The order is validate, then write, then index. Nothing reaches the filesystem until
//! `inspect` has accepted it, so a rejected upload leaves no trace at all, which matters
//! because the volume is shared and quota'd across every room in the environment.

use puna_core::artifact::{self, IngestError};
use puna_core::model::{generation, names};
use rocket::form::Form;
use rocket::fs::TempFile;
use rocket::http::Status;
use rocket::request::FlashMessage;
use rocket::response::{Flash, Redirect};
use rocket::{FromForm, State, get, post, routes, uri};

use crate::auth::{AdminSession, LoggedInSession};
use crate::error::{Error, Result};
use crate::flash::Notice;
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
    /// A room name to start from: `<organizer>'s multiworld <YYYY-MM-DD>`.
    ///
    /// **Server-rendered rather than filled in by script**, so it is there before anything loads
    /// and there for somebody with scripting off. The date is the server's, in UTC: a name is a
    /// label rather than an instant, and one that disagreed with the creator's calendar by a few
    /// hours would be a worse kind of wrong than one that is simply the server's day.
    default_room_name: String,
    /// Which port this seed's size recommends leading with.
    ///
    /// **Computed from the same function the room is stored with**, so the radio that arrives
    /// preselected and the value written on submit cannot disagree: a form recommending one thing
    /// while creation did another would be worse than either.
    primary_port_default: puna_core::model::room::PrimaryPort,
    /// Whether this deployment has a lobby to import slot owners from.
    ///
    /// Decided in the route from configuration, so a deployment standing alone renders no field at
    /// all rather than one whose only possible answer is "no lobby is configured".
    has_lobby: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "generations/list.html")]
pub struct ListTemplate {
    base: TplContext,
    /// Uploads rather than generations: each carries the reader's OWN upload time, which for a
    /// generation somebody else uploaded first is not the generation's.
    generations: Vec<generation::Upload>,
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
    lobby: &State<crate::LobbyConfig>,
) -> Result<ShowTemplate> {
    let id = id
        .parse()
        .map_err(|_| Error::new(Status::NotFound, anyhow::anyhow!("not a generation id")))?;
    let mut conn = pool.get().await?;

    let generation = generation::get(&mut conn, id)
        .await?
        .ok_or_else(|| Error::new(Status::NotFound, anyhow::anyhow!("no such generation")))?;
    let slots = generation::slots(&mut conn, id).await?;
    // Read before the move into the template, and off `generations.slots` rather than the rows
    // above: that column is what `room::create` reads to make the same recommendation.
    let primary_port_default = puna_core::model::room::PrimaryPort::for_slots(generation.slots);

    Ok(ShowTemplate {
        base: TplContext::new(session.session()),
        generation,
        slots,
        deduplicated: dedup.unwrap_or(false),
        primary_port_default,
        has_lobby: lobby.0.is_some(),
        default_room_name: format!(
            "{}'s multiworld {}",
            session.session().username.as_deref().unwrap_or("a"),
            chrono::Utc::now().format("%Y-%m-%d")
        ),
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
        // Both, and they are worth having side by side: `created=false, first_for_this_user=true`
        // is the dedup-across-accounts case, which is invisible in the UI by design and therefore
        // only observable here.
        created = inserted.created,
        first_for_this_user = inserted.first_for_this_user,
        promotion = ?promotion,
        unmatched = unmatched.len(),
        names_cached,
        "generation ingested"
    );

    // POST-redirect-GET, so a refresh does not re-upload tens of megabytes.
    //
    // `first_for_this_user`, never `created`: the second is a fact about other people's uploads,
    // and rendering it tells this uploader that another account holds the same seed.
    Ok(Redirect::to(format!(
        "/generations/{}?dedup={}",
        inserted.id, !inserted.first_for_this_user
    )))
}

/// Fill the tracker's name cache for a generation, from the seed just written to disk.
///
/// **Never fatal, and that is the design rather than laziness.** A generation whose names did not
/// cache is a perfectly usable generation with a slightly worse tracker (the tracker renders raw
/// ids, exactly as the reference does for a name it cannot resolve), so refusing an upload over it
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

/// The admin view of the name cache, and where a rebuild is triggered from.
///
/// **A page rather than a documented curl**, because the repair is rare enough that nobody will
/// remember the path and important enough that the tracker shows raw ids until it runs. The
/// rebuild's summary rides back in a one-shot cookie. See [`crate::flash`] for why not the query
/// string.
#[derive(Template, WebTemplate)]
#[template(path = "admin/generations.html")]
pub struct AdminGenerationsTemplate {
    base: TplContext,
    generations: Vec<names::CacheStatus>,
    result: Option<Notice>,
}

#[get("/admin/generations")]
async fn admin_generations(
    session: AdminSession,
    flash: Option<FlashMessage<'_>>,
    pool: &State<puna_core::db::Pool>,
) -> Result<AdminGenerationsTemplate> {
    let mut conn = pool.get().await?;
    Ok(AdminGenerationsTemplate {
        base: TplContext::new(session.session()),
        generations: names::status(&mut conn).await?,
        result: Notice::take(flash),
    })
}

/// Rebuild the tracker's name cache for every generation that has none.
///
/// **This is the backfill**, and it is a route rather than a migration for one reason: the names
/// come out of a file on a volume Postgres cannot see, so the repair has to run somewhere with the
/// mount. That is this tier and only this tier.
///
/// Safe to run repeatedly: it only touches generations with nothing cached. A generation whose seed
/// is missing from disk is reported and skipped rather than failing the run, because one unreadable
/// seed must not stop the other forty being repaired.
#[post("/admin/generations/rebuild-names")]
async fn rebuild_all_names(
    _session: AdminSession,
    pool: &State<puna_core::db::Pool>,
    data_dir: &State<DataDir>,
) -> Result<Flash<Redirect>> {
    let mut conn = pool.get().await?;
    let pending = names::uncached(&mut conn).await?;

    let mut rebuilt = 0usize;
    let mut failed = 0usize;
    for generation in &pending {
        if cache_names(&mut conn, &data_dir.0, &generation.sha256, generation.id).await {
            rebuilt += 1;
        } else {
            failed += 1;
        }
    }

    tracing::info!(
        considered = pending.len(),
        rebuilt,
        failed,
        "rebuilt the tracker's name cache"
    );

    let back = Redirect::to(uri!(admin_generations));
    // A partial rebuild is reported as a warning rather than as a success, because "Rebuilt 39
    // generations; 1 failed" in the color of a success is the sentence somebody skims past.
    Ok(if pending.is_empty() {
        Flash::success(
            back,
            "Every generation already has cached names; nothing to do.",
        )
    } else if failed == 0 {
        Flash::success(
            back,
            format!("Rebuilt {}.", puna_core::text::count(rebuilt, "generation")),
        )
    } else {
        Flash::warning(
            back,
            format!(
                "Rebuilt {}. {failed} failed; see the log for which and why.",
                puna_core::text::count(rebuilt, "generation")
            ),
        )
    })
}

/// Rebuild one generation's names whether or not it already has them.
///
/// The repair for a cache that is present but wrong, which the backfill above deliberately will
/// not touch, since from its side a wrong cache and a right one look the same.
#[post("/admin/generations/<id>/rebuild-names")]
async fn rebuild_names(
    _session: AdminSession,
    id: &str,
    pool: &State<puna_core::db::Pool>,
    data_dir: &State<DataDir>,
) -> Result<Flash<Redirect>> {
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

    let back = Redirect::to(uri!(admin_generations));
    Ok(if cache_names(&mut conn, &data_dir.0, &sha256, id).await {
        Flash::success(
            back,
            format!("Rebuilt the names for {}.", generation.seed_name),
        )
    } else {
        // Not an error page: the reason is in the log with the generation id, and an operator who
        // just pressed a button is better served by the list plus a sentence than by a 500.
        Flash::error(
            back,
            format!(
                "Could not rebuild the names for {}; see the log for why.",
                generation.seed_name
            ),
        )
    })
}

pub fn routes() -> Vec<rocket::Route> {
    routes![
        new_form,
        list,
        show,
        upload,
        admin_generations,
        rebuild_all_names,
        rebuild_names
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use puna_core::ids::GenerationId;

    fn status(games: &[&str], games_cached: i64, slots_cached: i64) -> names::CacheStatus {
        names::CacheStatus {
            id: GenerationId::new(),
            seed_name: "70327325896653383029".into(),
            games: games.iter().map(|g| (*g).to_string()).collect(),
            slots: 4,
            games_cached,
            slots_cached,
            // Players only. The generation has four slots and one is a spectator, which owns no
            // locations and therefore no cache row -- so three is COMPLETE, not a shortfall.
            slots_total: 3,
        }
    }

    /// **The three states have to look different**, and "partial" is the one that matters: a
    /// half-finished rebuild renders *some* names, so the tracker looks fine until you hit an item
    /// from the game that is missing. Rounding it to "cached" would hide the only case that needs a
    /// person.
    #[test]
    fn the_three_cache_states_are_distinguishable() {
        let none = status(&["Yacht Dice Bliss"], 0, 0);
        let partial = status(&["Yacht Dice Bliss", "Timespinner"], 1, 3);
        let complete = status(&["Yacht Dice Bliss"], 1, 3);

        assert!(!none.cached() && !none.complete());
        assert!(partial.cached() && !partial.complete());
        assert!(complete.cached() && complete.complete());

        let page = AdminGenerationsTemplate {
            base: TplContext {
                is_logged_in: true,
                is_admin: true,
                username: "troy".into(),
                site_name: "puna",
                version: "test",
                static_version: "test",
                view_as: None,
            },
            generations: vec![none.clone(), partial.clone(), complete.clone()],
            result: Some(Notice {
                class: "notice",
                message: "Rebuilt 1 generation.".into(),
            }),
        };

        let html = page.render().expect("renders");

        // **The regression this fixes.** A one-game, one-slot generation reported "2 games, 1
        // slots" on this page: the cache legitimately holds the `Archipelago` pseudo-game, and the
        // spectator legitimately has no location row, so both counts were right and both counted
        // something other than the columns beside them.
        let one_game = names::CacheStatus {
            id: GenerationId::new(),
            seed_name: "77085767817399703051".into(),
            games: vec!["Minecraft Dig".into()],
            slots: 1,
            games_cached: 1,
            slots_cached: 1,
            slots_total: 1,
        };
        assert!(
            one_game.complete(),
            "a fully cached one-game generation must not read as partial"
        );

        // And a spectator-only shortfall is not a shortfall.
        let with_spectator = status(&["Yacht Dice Bliss"], 1, 3);
        assert!(with_spectator.complete(), "the spectator was counted");

        assert!(html.contains("none"), "the uncached state is not labeled");
        assert!(
            html.contains("partial"),
            "the half-rebuilt state is not called out"
        );
        // `1/2`, not "1 of 2": `askama.toml` sets `whitespace = "suppress"`, so a space written
        // between two expressions in the template vanishes. The template separates values with
        // entities and punctuation for that reason, and this asserts the form it actually renders.
        assert!(html.contains("1/2"), "partial does not say how much");
        assert!(html.contains("Rebuilt 1 generation."), "the result notice");

        // Every row offers its own rebuild, and the backfill is separate from them.
        for row in [none, partial, complete] {
            assert!(
                html.contains(&format!("/admin/generations/{}/rebuild-names", row.id)),
                "a row has no rebuild control"
            );
        }
        assert!(html.contains(r#"action="/admin/generations/rebuild-names""#));
    }

    /// An empty deployment says so rather than rendering an empty table with no explanation.
    #[test]
    fn no_generations_reads_as_a_sentence() {
        let page = AdminGenerationsTemplate {
            base: TplContext {
                is_logged_in: true,
                is_admin: true,
                username: "troy".into(),
                site_name: "puna",
                version: "test",
                static_version: "test",
                view_as: None,
            },
            generations: Vec::new(),
            result: None,
        };

        let html = page.render().expect("renders");
        assert!(html.contains("No generations have been uploaded yet."));
    }

    /// **A source lint: the dedup notice must be built from the PER-USER answer.**
    ///
    /// `Insertion::created` is global (were these bytes already indexed, by anyone), and rendering
    /// it tells a second uploader that another account holds the same seed. `record_upload`'s
    /// answer is about the caller alone. The two are both plain `bool`s sitting three lines apart,
    /// so nothing in the type system separates them, and swapping one for the other produces a
    /// page that works perfectly and leaks.
    ///
    /// A lint rather than a route test because the failure is about which of two values was
    /// written, and reaching the route needs a database, a volume and a real zip.
    #[test]
    fn the_dedup_notice_is_never_the_global_answer() {
        // Only the routes, not this module's own tests -- which necessarily name both values in
        // order to talk about them, and would otherwise be the thing the lint reports.
        let source = include_str!("generations.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("the file has a non-test half");

        assert!(
            source.contains("inserted.id, !inserted.first_for_this_user"),
            "the redirect no longer carries the per-user answer; if it was reworded, reword this \
             lint with it rather than deleting it"
        );

        // Once, in the log line, where BOTH answers are wanted precisely because the case where
        // they differ is invisible in the UI by design. A second occurrence means it reached
        // something a user can see.
        let global_uses = source.matches("inserted.created").count();
        assert_eq!(
            global_uses, 1,
            "`inserted.created` is used {global_uses} times; it belongs only in the log line. \
             Anything user-visible must use `record_upload`'s answer, or a second uploader is told \
             that somebody else already has their seed."
        );
    }

    /// The listing renders each reader's OWN upload time.
    ///
    /// The column is headed "Uploaded", and for anyone who uploaded a seed somebody else uploaded
    /// first, the generation's `created_at` is a different and older moment. Rendering it would
    /// both misdate their entry and tell them the seed predates them.
    #[test]
    fn the_listing_shows_your_upload_time_not_the_generations() {
        use chrono::TimeZone;

        let created = chrono::Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap();
        let uploaded = chrono::Utc
            .with_ymd_and_hms(2026, 8, 20, 17, 30, 0)
            .unwrap();

        let page = ListTemplate {
            base: TplContext {
                is_logged_in: true,
                is_admin: false,
                username: "bob".into(),
                site_name: "puna",
                version: "test",
                static_version: "test",
                view_as: None,
            },
            generations: vec![generation::Upload {
                generation: generation::Generation {
                    id: GenerationId::new(),
                    sha256: vec![0; 32],
                    size_bytes: 1,
                    seed_name: "77085767817399703051".into(),
                    slots: 4,
                    locations: 400,
                    games: vec!["Minecraft Dig".into()],
                    race_mode: false,
                    has_spoiler: false,
                    created_at: created,
                },
                uploaded_at: uploaded,
            }],
        };

        let html = page.render().expect("renders");
        assert!(
            html.contains("2026-08-20 17:30 UTC"),
            "the reader's own upload time is missing"
        );
        assert!(
            !html.contains("2026-07-01"),
            "the generation's creation date reached the page: it dates this entry to somebody \
             else's upload"
        );
    }

    /// **The creation form presents five decisions, each preselected and each explained.**
    ///
    /// Every setting here is one the room then lives with, and three of them cost a restart to
    /// change afterwards, so the panel's job is to make somebody choose rather than to be quick to
    /// get past. What this pins is the part that fails silently: a default that moves, a radio
    /// group whose value no longer matches what the route parses, or a hint that stops being
    /// rendered for one of the options.
    ///
    /// **The route parses every one of these strictly**, so a value renamed on one side and not
    /// the other is a `400` on a form that looks completely ordinary. Asserting the rendered
    /// `value=` against the same words `parse()` accepts is what keeps the two in step.
    #[test]
    fn the_creation_form_preselects_a_deliberate_default_for_every_choice() {
        use askama::Template;

        let html = ShowTemplate {
            base: crate::tpl::TplContext::new(&crate::auth::Session::default()),
            generation: a_generation(),
            slots: Vec::new(),
            deduplicated: false,
            has_lobby: true,
            primary_port_default: puna_core::model::room::PrimaryPort::Full,
            default_room_name: "troy's multiworld 2026-08-29".into(),
        }
        .render()
        .expect("renders");

        // The name arrives filled in, so the common case is one button rather than a naming
        // decision nobody wanted to make.
        let name_field = html
            .split_once(r#"id="name""#)
            .expect("a room name field")
            .1
            .split_once('>')
            .expect("a closed tag")
            .0;
        assert!(
            name_field.contains("multiworld 2026-08-29"),
            "the room name is not prefilled, so the common case is a naming decision nobody \
             wanted to make: {name_field}"
        );

        // Troy's defaults, one per group. Each is the answer somebody would otherwise have to
        // think about on a page they see once.
        for (name, value) in [
            ("slot_auth", "none"),
            ("patch_policy", "claimed"),
            ("journal_policy", "feed"),
            ("tracker_policy", "link"),
        ] {
            let expected = format!(r#"name="{name}" value="{value}" checked"#);
            assert!(
                html.contains(&expected),
                "`{name}` does not default to `{value}`; the form opens on a different choice \
                 from the one that was decided on"
            );
        }

        // Every option the route accepts is offered, and vice versa: the route parses these
        // strictly, so a word that differs on either side is a 400 from an ordinary-looking form.
        for value in ["none", "room", "per_slot"] {
            assert!(
                html.contains(&format!(r#"value="{value}""#)),
                "no `{value}` option"
            );
            assert!(puna_core::model::room::SlotAuth::parse(value).is_some());
        }
        for value in ["full", "feed", "disabled"] {
            assert!(puna_core::model::room::JournalPolicy::parse(value).is_some());
        }
        for value in ["link", "members", "disabled"] {
            assert!(puna_core::model::room::TrackerPolicy::parse(value).is_some());
        }
        for value in ["open", "claimed"] {
            assert!(puna_core::model::room::PatchPolicy::parse(value).is_some());
        }
        for value in ["full", "filtered"] {
            assert!(puna_core::model::room::PrimaryPort::parse(value).is_some());
        }

        // **The primary port is preselected from the seed's size**, which is the one default here
        // computed rather than fixed, so it is asserted against the same function `room::create`
        // calls rather than against a literal. A form recommending one port while creation stored
        // the other would be invisible until somebody compared the page with the room.
        for slots in [1, 199, 200, 2000] {
            let expected = puna_core::model::room::PrimaryPort::for_slots(slots);
            let rendered = ShowTemplate {
                base: crate::tpl::TplContext::new(&crate::auth::Session::default()),
                generation: generation::Generation {
                    slots,
                    ..a_generation()
                },
                slots: Vec::new(),
                deduplicated: false,
                primary_port_default: puna_core::model::room::PrimaryPort::for_slots(slots),
                default_room_name: "n".into(),
                has_lobby: true,
            }
            .render()
            .expect("renders");
            assert!(
                rendered.contains(&format!(
                    r#"name="primary_port" value="{}" checked"#,
                    expected.as_sql()
                )),
                "a {slots}-slot seed does not preselect {}",
                expected.as_sql()
            );
        }
        // The threshold itself, stated here so moving it is a deliberate edit in two places rather
        // than a silent change to what every large sync tells its players to connect to.
        assert_eq!(
            puna_core::model::room::PrimaryPort::for_slots(199),
            puna_core::model::room::PrimaryPort::Full
        );
        assert_eq!(
            puna_core::model::room::PrimaryPort::for_slots(200),
            puna_core::model::room::PrimaryPort::Filtered
        );

        // **A hint per option, not per group.** They are server-rendered and revealed one at a
        // time, so a page with no scripting shows all of them, verbose and correct, where
        // building the text in script would leave an unscripted reader with unlabelled radios.
        let hints = html.matches("data-for=").count();
        assert_eq!(
            hints, 13,
            "expected one hint for each of the 13 explained options, found {hints}"
        );

        // Unchecked, and it says what it is for: this is pahoa's in-game `!admin login` and not
        // anything Puna's own console needs.
        assert!(html.contains(r#"type="checkbox" name="server_password""#));
        assert!(!html.contains(r#"name="server_password" value="1" checked"#));

        // Rendered but inert. A disabled input submits nothing, so the route cannot receive it by
        // accident before the pipeline behind it exists.
        assert!(
            html.contains(r#"id="lobby_url""#) && html.contains("disabled"),
            "the lobby field is either missing or live before anything can honor it"
        );

        assert!(
            html.contains(r#"type="reset""#),
            "no way back to the defaults"
        );

        // **The creation form does not ask about the spoiler, deliberately.** It is the one thing
        // on a room whose disclosure cannot be taken back, so a new room starts at the tightest
        // setting anybody can still reach and widening it is a deliberate visit to the options
        // page, not a radio somebody passes on the way to making a room.
        assert!(
            !html.contains("spoiler_policy"),
            "the creation form offers a spoiler setting, which is a decision to make on purpose \
             rather than in passing"
        );
    }

    fn a_generation() -> generation::Generation {
        generation::Generation {
            id: GenerationId::new(),
            sha256: vec![0; 32],
            size_bytes: 1,
            seed_name: "seed".into(),
            slots: 2,
            locations: 20,
            games: vec!["Balatro".into()],
            race_mode: false,
            has_spoiler: false,
            created_at: chrono::Utc::now(),
        }
    }
}
