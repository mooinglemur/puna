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
//!
//! ## The refusal has to reach a page, not only a status code
//!
//! The guard answers `403` with an empty body, which for as long as it has existed was the *only*
//! way somebody learned they were not allowed to upload a generation or open a room: the pages
//! offering those controls are ungated, so the link was always there and always led to a blank
//! refusal. So [`standing`] is a function rather than something the guard does inline, and
//! [`refusal_notice`] is the shape a page asks it in. A page renders the sentence where the
//! control would be; the route renders the same sentence into the `403` it still answers to
//! anybody who posts anyway. One decision, one wording, two places it can be met.

use std::marker::PhantomData;

use diesel_async::AsyncPgConnection;
use puna_core::db::Pool;
use puna_core::model::RoomSource;
use puna_core::model::settings::{self, Decision, Grant, Refusal};
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

/// Why a caller may not create, if they may not.
///
/// Two refusals that arrive from different places and read as one thing to the person refused:
/// their account's own standing, and the source's gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denial {
    /// The account is restricted, with whatever note the administrator left.
    Restricted(Option<String>),
    /// The gate refused, for one of its two reasons.
    Gate(Refusal),
}

impl Denial {
    /// The sentence a refused caller is shown, wherever they meet the refusal.
    ///
    /// One message for the page that withholds the control and the route that would answer `403`,
    /// so what somebody is told when a button is missing and what they are told if they post
    /// anyway cannot be two different explanations.
    pub fn message(&self) -> String {
        match self {
            Self::Restricted(Some(why)) => {
                format!("This account cannot open rooms or upload generations. {why}")
            }
            Self::Restricted(None) => {
                "This account cannot open rooms or upload generations.".to_string()
            }
            Self::Gate(refusal) => refusal.message().to_string(),
        }
    }
}

/// What to say where a creation control would be, or `None` when there is nothing to say.
///
/// The page-side shape of [`standing`]: a page asks this, renders the sentence in place of the
/// button, and never has to know which of the three refusals it is looking at.
pub async fn refusal_notice(
    conn: &mut AsyncPgConnection,
    source: RoomSource,
    user_id: i64,
    is_admin: bool,
) -> Result<Option<String>, Error> {
    Ok(standing(conn, source, user_id, is_admin)
        .await?
        .err()
        .map(|denial| denial.message()))
}

/// The creation decision for one caller and one source.
///
/// **One function, two callers**, the same rule `room::may_see_spoiler` follows: the guard below
/// turns a refusal into a `403`, and a page that offers a creation control calls this to decide
/// whether to offer it and what to say instead. A page reaching its own conclusion would be one
/// edit away from offering a control the route refuses, or hiding one it would serve.
///
/// A read that fails is not permission: both errors answer `503`, matching what the gate policy
/// itself does with a missing or unreadable row.
pub async fn standing(
    conn: &mut AsyncPgConnection,
    source: RoomSource,
    user_id: i64,
    is_admin: bool,
) -> Result<Result<Grant, Denial>, Error> {
    // **A restricted account is refused here and nowhere else**, because this guard is already
    // the only door onto both things `restricted` withholds: opening a room and uploading a
    // generation. Checked BEFORE the gate so the answer does not depend on whether creation
    // happens to be open, and **before the admin bypass**, which is the point that matters: an
    // administrator who has been restricted is restricted, or the sanction means nothing the
    // moment it is applied to somebody who can turn it off.
    match puna_core::model::user::status_of(conn, user_id).await {
        Ok(Some((status, note))) if !status.may_create() => {
            return Ok(Err(Denial::Restricted(note)));
        }
        Ok(_) => {}
        // Unreadable standing is not permission to create. The gate below fails closed for the
        // same reason and this must not be the softer of the two.
        Err(e) => {
            return Err(Error::new(Status::ServiceUnavailable, e.into()));
        }
    }

    match settings::evaluate(conn, source, user_id, is_admin).await {
        Ok(Decision::Allowed(grant)) => Ok(Ok(grant)),
        Ok(Decision::Refused(refusal)) => Ok(Err(Denial::Gate(refusal))),
        // A gate that cannot be read is not a gate that permits. `settings::evaluate` already
        // fails closed on a missing or unrecognized row; this is the connection-level case.
        Err(e) => Err(Error::new(Status::ServiceUnavailable, e.into())),
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

        let standing =
            match standing(&mut conn, S::SOURCE, session.user_id(), session.is_admin()).await {
                Ok(standing) => standing,
                Err(e) => return Outcome::Error((e.status, e)),
            };

        match standing {
            Ok(grant) => {
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
            Err(denial) => {
                // Logged at info rather than debug: a refusal is the signal an administrator
                // wants when someone reports that they cannot upload.
                tracing::info!(
                    user_id = session.user_id(),
                    source = S::SOURCE.as_sql(),
                    ?denial,
                    "room creation refused"
                );
                Outcome::Error((
                    Status::Forbidden,
                    Error::new(Status::Forbidden, anyhow::anyhow!(denial.message())),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every refusal says what was refused and what to do next**, because these sentences are
    /// now rendered on a page rather than only carried into a log line.
    ///
    /// Two properties, and the second is the one that would rot. Each message names *both* things
    /// the gate governs, since one gate withholds opening a room and uploading a generation and
    /// the same sentence is read on both pages. And none of them is a bare statement of fact: a
    /// refusal a reader cannot act on is the bare `403` again with a nicer typeface, so each one
    /// points at the administrator who can change it.
    #[test]
    fn every_refusal_names_both_actions_and_says_what_to_do_next() {
        let all = [
            Denial::Restricted(None),
            Denial::Restricted(Some("Uploaded somebody else's seed.".into())),
            Denial::Gate(Refusal::Disabled),
            Denial::Gate(Refusal::NotAllowlisted),
        ];

        for denial in &all {
            let message = denial.message();
            assert!(
                message.contains("rooms") && message.contains("generations"),
                "{denial:?} names only part of what it withholds: {message}"
            );
            assert!(
                message.contains("administrator") || message.contains("account"),
                "{denial:?} states a fact and leaves the reader nowhere to go: {message}"
            );
            assert!(
                message.ends_with('.'),
                "{denial:?} is a fragment, and it is rendered as a sentence: {message}"
            );
        }

        // An administrator's note is the whole reason a restricted account's refusal is not a
        // constant: it is the one place a person is told what THEY did, rather than what the
        // deployment is doing.
        assert!(
            Denial::Restricted(Some("Uploaded somebody else's seed.".into()))
                .message()
                .contains("somebody else's seed"),
            "the note an administrator left is dropped, so the sanction cannot be explained"
        );
    }
}
