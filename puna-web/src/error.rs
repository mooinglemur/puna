//! One error type for handlers, carrying an HTTP status.
//!
//! Handlers return `Result<T>`; anything that fails converts through `anyhow` and comes out as a
//! 500 unless it was built with an explicit status. The status matters more than usual here
//! because `401` is what triggers the login redirect (see the catcher in `main.rs`), so returning
//! a plain 500 for "not logged in" would leave a user staring at an error instead of Discord.

use rocket::http::Status;
use rocket::request::Request;
use rocket::response::{self, Responder, Response};

#[derive(Debug)]
pub struct Error {
    pub status: Status,
    pub source: anyhow::Error,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(status: Status, source: anyhow::Error) -> Self {
        Self { status, source }
    }
}

pub fn unauthorized(message: &'static str) -> Error {
    Error::new(Status::Unauthorized, anyhow::anyhow!(message))
}

pub fn forbidden(message: &'static str) -> Error {
    Error::new(Status::Forbidden, anyhow::anyhow!(message))
}

impl<E: Into<anyhow::Error>> From<E> for Error {
    fn from(e: E) -> Self {
        Self {
            status: Status::InternalServerError,
            source: e.into(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.source)
    }
}

impl<'r> Responder<'r, 'static> for Error {
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'static> {
        // Log the full chain, return only the status. Error bodies reach users, and an anyhow
        // chain from a database failure can name tables, columns and connection strings.
        if self.status.code >= 500 {
            tracing::error!(error = ?self.source, status = self.status.code, "request failed");
        } else {
            tracing::debug!(error = %self.source, status = self.status.code, "request rejected");
        }

        Response::build().status(self.status).ok()
    }
}

/// A 404 that carries a reason for the log without putting it in the response.
pub fn not_found(message: &'static str) -> Error {
    Error::new(Status::NotFound, anyhow::anyhow!(message))
}
