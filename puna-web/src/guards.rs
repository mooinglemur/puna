//! Room-scoped request guards: [`RoomAccess`] for staff, [`SlotAccess`] for per-slot material.
//!
//! ## Both read the room id from the route's FIRST dynamic segment
//!
//! Rocket guards take no parameters, so a guard cannot be told which room it is guarding -- it has
//! to find out, from the routed URI. **`Request::param(n)` is SEGMENT-indexed**, so for
//! `/room/<id>/...` the id is segment **1** (segment 0 is the literal `room`), and for
//! `/room/<id>/slot/<n>/...` the slot number is segment **3**.
//!
//! Reading `param(0)` as "the first dynamic parameter" is the obvious misreading and it compiles.
//! It also made every guarded route here answer 404 for everybody from M5 until 2026-08-20 -- see
//! [`ROOM_ID_SEGMENT`].
//!
//! So every room-scoped route must spell the room id first: `/room/<id>/...`. That is a convention,
//! and conventions rot -- but breaking it fails loudly and immediately, with a 404 on the first
//! request rather than authorization against the wrong room. The failure mode is "this page never
//! works", not "this page works for the wrong people".
//!
//! ## Admins short-circuit; nothing else does
//!
//! A global admin resolves to [`RoomRole::Organizer`] without a roster row, and deliberately does
//! not get one -- `member::list` stays an honest answer to "who is staff here". Everyone else is
//! resolved from the roster and from nothing else: there is no creator special case, because
//! `rooms.created_by` is informational and the uploader is simply the first organizer row.

use std::marker::PhantomData;

use puna_core::db::Pool;
use puna_core::ids::RoomId;
use puna_core::model::member::{self, RoomRole};
use puna_core::model::room::{self, Room};
use puna_core::model::slot::{self, Slot};
use rocket::Request;
use rocket::State;
use rocket::http::Status;
use rocket::outcome::Outcome as RocketOutcome;
use rocket::request::{FromRequest, Outcome};

use crate::auth::{LoggedInSession, Session};
use crate::error::{Error, forbidden, not_found, unauthorized};

/// Whether this request is a person navigating, as opposed to a machine fetching.
///
/// **This is D8**, and the hazard it exists for is specific: pasting a room link into Discord makes
/// Discord fetch the page to build an unfurl. If `GET /room/<id>` starts an idle room, an unfurl
/// spins up a pod — so does a search crawler, a link checker, and every preview pane the URL passes
/// through on its way to the players.
///
/// Two headers, and both are needed. `Sec-Fetch-Mode: navigate` is sent by every current browser on
/// a top-level navigation and by nothing doing a background fetch; it is a *hint* rather than a
/// guarantee, since anything may send it. `Accept: text/html` filters the rest. Neither is a
/// security control and neither needs to be: **the explicit Start button is always there**, so the
/// worst a false negative costs is one click, and a false positive costs a pod that somebody was
/// about to ask for anyway.
pub struct Navigation(pub bool);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Navigation {
    type Error = std::convert::Infallible;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let headers = request.headers();
        let navigating = headers.get_one("Sec-Fetch-Mode") == Some("navigate");
        let wants_html = headers
            .get_one("Accept")
            .is_some_and(|accept| accept.contains("text/html"));

        // An older browser sends no `Sec-Fetch-Mode` at all. Requiring both would make the implicit
        // start silently stop working there, which is a worse failure than an occasional missed
        // start -- so the absence of the header falls back to the Accept header alone.
        let absent = headers.get_one("Sec-Fetch-Mode").is_none();
        RocketOutcome::Success(Navigation(wants_html && (navigating || absent)))
    }
}

/// Whether the caller asked for JSON rather than a page.
///
/// One route can then answer a form post with a redirect and a scripted control with a result, so
/// the tier check and the whole command path exist once. **Asked of `Accept` rather than the path**,
/// because the two callers are the same operation seen by different clients — a second URL would be
/// a second thing to keep in step, and the one that drifts is the one nobody reviews.
///
/// Deliberately not `Sec-Fetch-Mode`, unlike [`Navigation`]: that guard is about whether a *person*
/// navigated, which is a decision about side effects. This is about how to render an answer, and
/// `Accept` is the header that means exactly that.
pub struct WantsJson(pub bool);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for WantsJson {
    type Error = std::convert::Infallible;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        RocketOutcome::Success(WantsJson(
            request
                .headers()
                .get_one("Accept")
                .is_some_and(|accept| accept.contains("application/json")),
        ))
    }
}

/// A minimum role, lifted to the type level so it can index a guard.
pub trait MinRole: Send + Sync + 'static {
    const ROLE: RoomRole;
}

/// Hints, chat, countdowns, status: everything that does not change the room or its roster.
pub struct Helper;
impl MinRole for Helper {
    const ROLE: RoomRole = RoomRole::Helper;
}

/// Membership, settings, releases, kicks, deletion.
pub struct Organizer;
impl MinRole for Organizer {
    const ROLE: RoomRole = RoomRole::Organizer;
}

/// Proof that the caller holds at least `M` in the room named by the route.
///
/// Carries the room, so a handler needs one guard rather than a guard plus a lookup -- and so the
/// room it authorized against is provably the room it renders.
pub struct RoomAccess<M: MinRole> {
    pub room: Room,
    pub role: RoomRole,
    pub session: LoggedInSession,
    _min: PhantomData<M>,
}

impl<M: MinRole> RoomAccess<M> {
    pub fn user_id(&self) -> i64 {
        self.session.user_id()
    }

    /// What actually authorized this request.
    ///
    /// Worth carrying rather than discarding: "an organizer did this" and "a global admin did
    /// this while holding no role here" are different facts about the same action, and only the
    /// first survives the roster changing.
    pub fn role(&self) -> RoomRole {
        self.role
    }
}

/// Where the room id sits in `/room/<id>/...`.
///
/// **`Request::param` is SEGMENT-indexed, not parameter-indexed.** It returns "the 0-indexed nth
/// non-empty segment from the routed request", so segment 0 is the literal `room` and the id is
/// segment 1. Reading it as "the first dynamic parameter" is the obvious misreading, it compiles,
/// and it makes every guarded route answer **404 for everybody**.
///
/// It did, from M5 until 2026-08-20: eleven `RoomAccess` routes — stop, clone, members, invites,
/// settings, the whole console — plus the two `SlotAccess` ones. Nothing caught it because
/// `/room/<id>` itself takes its id as a handler argument rather than through this guard, so the
/// room page worked perfectly and everything behind it did not. It surfaced when somebody went
/// looking for a console link.
const ROOM_ID_SEGMENT: usize = 1;

/// Where the slot number sits in `/room/<id>/slot/<n>/...`: `room`, `<id>`, `slot`, `<n>`.
const SLOT_NUMBER_SEGMENT: usize = 3;

fn room_id_from_route(request: &Request<'_>) -> Option<RoomId> {
    request
        .param::<crate::params::RoomParam>(ROOM_ID_SEGMENT)
        .and_then(Result::ok)
        .map(|p| p.0)
}

fn slot_number_from_route(request: &Request<'_>) -> Option<i32> {
    request
        .param::<i32>(SLOT_NUMBER_SEGMENT)
        .and_then(Result::ok)
}

async fn room_from_request(request: &Request<'_>) -> Result<(Room, Pool), Error> {
    let id: RoomId = room_id_from_route(request)
        .ok_or_else(|| not_found("no room id in this route's first parameter"))?;

    let pool = request
        .guard::<&State<Pool>>()
        .await
        .succeeded()
        .ok_or_else(|| {
            Error::new(
                Status::InternalServerError,
                anyhow::anyhow!("no database pool in Rocket state"),
            )
        })?;

    let mut conn = pool
        .get()
        .await
        .map_err(|e| Error::new(Status::ServiceUnavailable, e.into()))?;

    let room = room::get(&mut conn, id)
        .await
        .map_err(|e| Error::new(Status::InternalServerError, e.into()))?
        .ok_or_else(|| not_found("no such room"))?;

    Ok((room, (*pool).clone()))
}

/// What role does this caller hold here, counting the admin short-circuit?
async fn effective_role(
    pool: &Pool,
    room: RoomId,
    session: &LoggedInSession,
) -> Result<Option<RoomRole>, Error> {
    if session.is_admin() {
        return Ok(Some(RoomRole::Organizer));
    }
    let mut conn = pool
        .get()
        .await
        .map_err(|e| Error::new(Status::ServiceUnavailable, e.into()))?;
    member::role_of(&mut conn, room, session.user_id())
        .await
        .map_err(|e| Error::new(Status::InternalServerError, e.into()))
}

#[rocket::async_trait]
impl<'r, M: MinRole> FromRequest<'r> for RoomAccess<M> {
    type Error = Error;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        // Authentication before authorization, so an anonymous caller gets a 401 and therefore a
        // login redirect rather than a 403 for something they have not identified themselves for.
        let session = match request.guard::<LoggedInSession>().await {
            RocketOutcome::Success(session) => session,
            RocketOutcome::Error(e) => return RocketOutcome::Error(e),
            RocketOutcome::Forward(f) => return RocketOutcome::Forward(f),
        };

        let (room, pool) = match room_from_request(request).await {
            Ok(pair) => pair,
            Err(e) => return Outcome::Error((e.status, e)),
        };

        let role = match effective_role(&pool, room.id, &session).await {
            Ok(role) => role,
            Err(e) => return Outcome::Error((e.status, e)),
        };

        // A non-member is told the room exists, because the room page itself is public -- there is
        // nothing to hide by pretending otherwise, and a 404 here would send an organizer who
        // mistyped their own account hunting for a deleted room.
        match role {
            Some(role) if role >= M::ROLE => Outcome::Success(RoomAccess {
                room,
                role,
                session,
                _min: PhantomData,
            }),
            _ => {
                tracing::info!(
                    user_id = session.user_id(),
                    room = %room.id,
                    held = ?role,
                    required = ?M::ROLE,
                    "room action refused"
                );
                Outcome::Error((
                    Status::Forbidden,
                    forbidden("you do not have that role in this room"),
                ))
            }
        }
    }
}

/// Proof that the caller may see one slot's patch and password.
///
/// Route shape: the room id first, the slot number second. The decision itself is
/// [`slot::may_access`] in `puna-core`, called from here and from nowhere else in the web tier.
pub struct SlotAccess {
    pub room: Room,
    pub slot: Slot,
    pub session: Session,
}

impl SlotAccess {
    /// Is the caller the person who claimed this slot, as opposed to staff looking at it?
    pub fn is_owner(&self) -> bool {
        matches!((self.session.user_id, self.slot.owner_id), (Some(u), Some(o)) if u == o)
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for SlotAccess {
    type Error = Error;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let session = Session::from_request_sync(request);

        let (room, pool) = match room_from_request(request).await {
            Ok(pair) => pair,
            Err(e) => return Outcome::Error((e.status, e)),
        };

        let Some(slot_number) = slot_number_from_route(request) else {
            return Outcome::Error((
                Status::NotFound,
                not_found("no slot number in this route's second parameter"),
            ));
        };

        let mut conn = match pool.get().await {
            Ok(conn) => conn,
            Err(e) => {
                return Outcome::Error((
                    Status::ServiceUnavailable,
                    Error::new(Status::ServiceUnavailable, e.into()),
                ));
            }
        };

        let slot = match slot::get(&mut conn, room.id, slot_number).await {
            Ok(Some(slot)) => slot,
            Ok(None) => return Outcome::Error((Status::NotFound, not_found("no such slot"))),
            Err(e) => {
                return Outcome::Error((
                    Status::InternalServerError,
                    Error::new(Status::InternalServerError, e.into()),
                ));
            }
        };

        let role = if session.is_admin {
            Some(RoomRole::Organizer)
        } else if let Some(user_id) = session.user_id {
            match member::role_of(&mut conn, room.id, user_id).await {
                Ok(role) => role,
                Err(e) => {
                    return Outcome::Error((
                        Status::InternalServerError,
                        Error::new(Status::InternalServerError, e.into()),
                    ));
                }
            }
        } else {
            None
        };

        if slot::may_access(&slot, session.user_id, role, session.is_admin) {
            return Outcome::Success(SlotAccess {
                room,
                slot,
                session,
            });
        }

        // 401 for an anonymous caller so the catcher sends them to Discord -- a player following
        // their own claim link from a phone is the common case, and a 403 would strand them.
        // 403 once logged in, because logging in again will not help.
        if session.is_logged_in {
            Outcome::Error((
                Status::Forbidden,
                forbidden("that slot belongs to somebody else"),
            ))
        } else {
            Outcome::Error((Status::Unauthorized, unauthorized("not logged in")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocket::http::Header;
    use rocket::local::blocking::Client;
    use rocket::{get, routes};

    #[rocket::get("/probe")]
    fn probe(navigation: Navigation) -> String {
        navigation.0.to_string()
    }

    fn client() -> Client {
        Client::untracked(rocket::build().mount("/", rocket::routes![probe])).expect("a client")
    }

    /// D8, as a request. The hazard is a link preview starting a room, so the two cases that matter
    /// are "a browser navigating" and "something fetching the URL to build an unfurl".
    #[test]
    fn a_navigation_is_told_apart_from_a_link_preview() {
        let client = client();

        let browser = client
            .get("/probe")
            .header(Header::new("Sec-Fetch-Mode", "navigate"))
            .header(Header::new(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            ))
            .dispatch();
        assert_eq!(browser.into_string().as_deref(), Some("true"));

        // What a bot fetching a URL for an unfurl looks like: it asks for HTML, but it is not
        // navigating. This is the request that must NOT start a room.
        let unfurl = client
            .get("/probe")
            .header(Header::new("Sec-Fetch-Mode", "cors"))
            .header(Header::new("Accept", "text/html"))
            .dispatch();
        assert_eq!(unfurl.into_string().as_deref(), Some("false"));

        // The page's own poller: same origin, same session, and it must never start anything.
        let poll = client
            .get("/probe")
            .header(Header::new("Sec-Fetch-Mode", "cors"))
            .header(Header::new("Accept", "application/json"))
            .dispatch();
        assert_eq!(poll.into_string().as_deref(), Some("false"));

        // A bare curl: no Accept for HTML, so no implicit start either.
        let curl = client.get("/probe").dispatch();
        assert_eq!(curl.into_string().as_deref(), Some("false"));
    }

    /// An older browser sends no `Sec-Fetch-Mode` at all. Requiring it would make the implicit start
    /// silently stop working there, which is worse than the occasional missed one -- the explicit
    /// button is always present either way.
    #[test]
    fn a_browser_without_the_header_still_counts_as_navigating() {
        let client = client();
        let response = client
            .get("/probe")
            .header(Header::new("Accept", "text/html,*/*;q=0.8"))
            .dispatch();
        assert_eq!(response.into_string().as_deref(), Some("true"));
    }

    /// Echoes what the guards would extract, without needing a database or a session.
    struct Extracted(Option<String>, Option<i32>);

    #[rocket::async_trait]
    impl<'r> FromRequest<'r> for Extracted {
        type Error = std::convert::Infallible;
        async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
            RocketOutcome::Success(Extracted(
                room_id_from_route(request).map(|id| id.to_string()),
                slot_number_from_route(request),
            ))
        }
    }

    #[get("/room/<_id>/console")]
    fn console(_id: &str, extracted: Extracted) -> String {
        format!("{:?}|{:?}", extracted.0, extracted.1)
    }

    #[get("/room/<_id>/command/<_cid>")]
    fn command(_id: &str, _cid: &str, extracted: Extracted) -> String {
        format!("{:?}|{:?}", extracted.0, extracted.1)
    }

    #[get("/room/<_id>/slot/<_n>/patch")]
    fn patch(_id: &str, _n: i32, extracted: Extracted) -> String {
        format!("{:?}|{:?}", extracted.0, extracted.1)
    }

    fn route_client() -> Client {
        Client::tracked(rocket::build().mount("/", routes![console, command, patch]))
            .expect("a test client")
    }

    /// **The bug, pinned.** `param` is segment-indexed, so `param(0)` is the literal `"room"` and
    /// can never parse as a uuid — which made every guarded route 404 for everybody, for months.
    ///
    /// Exercised through Rocket's real router rather than by asserting the constant, because the
    /// constant was not the thing that was wrong: the *reading* of the API was.
    #[test]
    fn the_room_id_is_extracted_from_every_guarded_route_shape() {
        let client = route_client();
        let room = "e41ca59b-9084-4a44-855e-b14c61eff9b0";

        for path in [
            format!("/room/{room}/console"),
            format!("/room/{room}/command/11111111-1111-1111-1111-111111111111"),
            format!("/room/{room}/slot/3/patch"),
        ] {
            let body = client.get(&path).dispatch().into_string().expect("a body");
            assert!(
                body.contains(room),
                "{path} did not yield the room id: {body}"
            );
        }
    }

    /// And the slot number comes from the slot route, not from wherever the room id happened to be.
    #[test]
    fn the_slot_number_is_extracted_from_the_slot_route() {
        let client = route_client();
        let room = "e41ca59b-9084-4a44-855e-b14c61eff9b0";

        let body = client
            .get(format!("/room/{room}/slot/3/patch"))
            .dispatch()
            .into_string()
            .expect("a body");
        assert!(
            body.ends_with("Some(3)"),
            "the slot number is wrong: {body}"
        );

        // A route with no slot in it yields none, rather than reading some other segment as one.
        let body = client
            .get(format!("/room/{room}/console"))
            .dispatch()
            .into_string()
            .expect("a body");
        assert!(body.ends_with("None"), "a slot was invented: {body}");
    }
}
