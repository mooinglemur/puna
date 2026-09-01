//! The `CanCreateRoom` request guard.
//!
//! One implementation, generic over the source, so that adding a creation route means naming a
//! source in its signature rather than remembering to call a check. A route that forgets the guard
//! does not compile into something insecure: it compiles into something that cannot see who the
//! caller is, because the guard is also how a handler gets its [`LoggedInSession`].
//!
//! The *policy* is not here. It lives in `puna_core::model::settings`, because the lobby push
//! (M14) authenticates with `X-Api-Key` rather than a session cookie and takes its acting user
//! from the request manifest, so it cannot pass through a cookie-shaped guard and calls
//! `settings::evaluate` directly. This type is the web tier's adapter onto that decision, not a
//! second copy of it.

use std::marker::PhantomData;

use puna_core::db::Pool;
use puna_core::model::RoomSource;
use puna_core::model::settings::{self, Decision, Grant};
use rocket::State;
use rocket::http::Status;
use rocket::request::{FromRequest, Outcome};
use rocket::{Request, outcome::Outcome as RocketOutcome};

use crate::auth::LoggedInSession;
use crate::error::Error;

/// A creation source, lifted to the type level so it can index a request guard.
///
/// Rocket guards take no parameters, so `CanCreateRoom(source)` becomes
/// `CanCreateRoom<Direct>`: distinct types over one implementation, which is what keeps the
/// check in a single place while still letting a route say which gate applies to it.
pub trait GateSource: Send + Sync + 'static {
    const SOURCE: RoomSource;
}

/// A zip uploaded through Puna's own form.
pub struct Direct;
impl GateSource for Direct {
    const SOURCE: RoomSource = RoomSource::Direct;
}

// There is deliberately NO `Lobby` marker here. The lobby push authenticates with `X-Api-Key`
// and names its acting user in the request manifest, so it cannot produce a `LoggedInSession` and
// will call `settings::evaluate(.., RoomSource::Lobby, ..)` directly at M14. A marker type that
// no route could use would only look like the check was covered.

/// Proof that this caller may create a room from `S`.
///
/// Carries the session, so a handler needs only this one guard, and the [`Grant`] that admitted
/// them, which is what a `room_events` row should record. "Created while the gate was open" and
/// "created by an admin over a closed gate" are different facts about the same room, and only one
/// of them survives the gate being closed again.
pub struct CanCreateRoom<S: GateSource> {
    session: LoggedInSession,
    grant: Grant,
    _source: PhantomData<S>,
}

impl<S: GateSource> CanCreateRoom<S> {
    pub fn session(&self) -> &LoggedInSession {
        &self.session
    }

    pub fn user_id(&self) -> i64 {
        self.session.user_id()
    }

    pub fn grant(&self) -> Grant {
        self.grant
    }
}

#[rocket::async_trait]
impl<'r, S: GateSource> FromRequest<'r> for CanCreateRoom<S> {
    type Error = Error;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        // Authentication first, so an anonymous caller gets a 401 and therefore a login redirect
        // rather than a 403 telling them they are not allowed to do something they have not yet
        // identified themselves for.
        let session = match request.guard::<LoggedInSession>().await {
            RocketOutcome::Success(session) => session,
            RocketOutcome::Error(e) => return RocketOutcome::Error(e),
            RocketOutcome::Forward(f) => return RocketOutcome::Forward(f),
        };

        let Some(pool) = request.guard::<&State<Pool>>().await.succeeded() else {
            return Outcome::Error((
                Status::InternalServerError,
                Error::new(
                    Status::InternalServerError,
                    anyhow::anyhow!("no database pool in Rocket state"),
                ),
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

        // **A restricted account is refused here and nowhere else**, because this guard is already
        // the only door onto both things `restricted` withholds -- opening a room and uploading a
        // generation. Checked BEFORE the gate so the answer does not depend on whether creation
        // happens to be open, and **before the admin bypass**, which is the point that matters: an
        // administrator who has been restricted is restricted, or the sanction means nothing the
        // moment it is applied to somebody who can turn it off.
        match puna_core::model::user::status_of(&mut conn, session.user_id()).await {
            Ok(Some((status, note))) if !status.may_create() => {
                let message = match note {
                    Some(why) => {
                        format!("This account cannot create rooms or upload generations. {why}")
                    }
                    None => "This account cannot create rooms or upload generations.".to_string(),
                };
                return Outcome::Error((
                    Status::Forbidden,
                    Error::new(Status::Forbidden, anyhow::anyhow!(message)),
                ));
            }
            Ok(_) => {}
            // Unreadable standing is not permission to create. The gate below fails closed for the
            // same reason and this must not be the softer of the two.
            Err(e) => {
                return Outcome::Error((
                    Status::ServiceUnavailable,
                    Error::new(Status::ServiceUnavailable, e.into()),
                ));
            }
        }

        let decision =
            match settings::evaluate(&mut conn, S::SOURCE, session.user_id(), session.is_admin())
                .await
            {
                Ok(decision) => decision,
                // A gate that cannot be read is not a gate that permits. `settings::evaluate` already
                // fails closed on a missing or unrecognized row; this is the connection-level case.
                Err(e) => {
                    return Outcome::Error((
                        Status::ServiceUnavailable,
                        Error::new(Status::ServiceUnavailable, e.into()),
                    ));
                }
            };

        match decision {
            Decision::Allowed(grant) => {
                tracing::debug!(
                    user_id = session.user_id(),
                    source = S::SOURCE.as_sql(),
                    ?grant,
                    "room creation permitted"
                );
                Outcome::Success(CanCreateRoom {
                    session,
                    grant,
                    _source: PhantomData,
                })
            }
            Decision::Refused(refusal) => {
                // Logged at info rather than debug: a refusal is the signal an administrator
                // wants when someone reports that they cannot upload.
                tracing::info!(
                    user_id = session.user_id(),
                    source = S::SOURCE.as_sql(),
                    ?refusal,
                    "room creation refused by the gate"
                );
                Outcome::Error((
                    Status::Forbidden,
                    Error::new(Status::Forbidden, anyhow::anyhow!(refusal.message())),
                ))
            }
        }
    }
}
