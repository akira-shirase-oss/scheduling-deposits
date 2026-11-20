//! The product's errors and their answers. Messages are fixed text; nothing a client sent
//! is echoed.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use booking_accounts::error::AccountsError;
use booking_db::DbError;
use booking_http::stack::error;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SchedulingError {
    /// Refused by the kit (unauthenticated, forbidden, not found, …).
    #[error(transparent)]
    Access(#[from] AccountsError),
    /// A malformed or out-of-bounds value (422).
    #[error("{1}")]
    Invalid(&'static str, &'static str),
    /// Nothing here, or nothing this caller may see (404).
    #[error("not found")]
    NotFound,
    /// The business is not set up yet: `PUT …/business` first (409).
    #[error("business required")]
    BusinessRequired,
    /// The slot's blocked range overlaps a live booking or hold (409, SQLSTATE 23P01).
    #[error("slot taken")]
    SlotTaken,
    /// The start is not a bookable slot by the hours, notice or advance (422).
    #[error("slot unavailable")]
    SlotUnavailable,
    /// The hold expired before it was confirmed (409).
    #[error("hold expired")]
    HoldExpired,
    /// The idempotency key was used with another request (409).
    #[error("idempotency conflict")]
    IdempotencyConflict,
    /// A state conflict (409), with its code.
    #[error("conflict {0}")]
    Conflict(&'static str),
    /// A size bound (403), naming which.
    #[error("limit {0}")]
    Limit(&'static str),
    /// The database is unavailable (503).
    #[error("database unavailable")]
    Unavailable,
    /// Anything else (500), logged once here.
    #[error("internal")]
    Internal(#[source] anyhow::Error),
}

fn code(e: &sqlx::Error) -> Option<String> {
    if let sqlx::Error::Database(db) = e {
        db.code().map(std::borrow::Cow::into_owned)
    } else {
        None
    }
}

impl From<sqlx::Error> for SchedulingError {
    fn from(e: sqlx::Error) -> Self {
        match code(&e).as_deref() {
            Some("23P01") => Self::SlotTaken,
            Some("BK001") => Self::HoldExpired,
            Some("BK002") => Self::Conflict("inactive"),
            Some("BK003") => Self::Invalid(
                "not_assigned",
                "the staff member does not perform the service",
            ),
            Some("BK005") => Self::Conflict("invalid_transition"),
            _ => DbError::from(e).into(),
        }
    }
}

impl From<DbError> for SchedulingError {
    fn from(e: DbError) -> Self {
        if e.is_unavailable() {
            Self::Unavailable
        } else {
            Self::Internal(anyhow::anyhow!(booking_db::chain(&e)))
        }
    }
}

impl IntoResponse for SchedulingError {
    fn into_response(self) -> Response {
        use StatusCode as S;
        match self {
            Self::Access(e) => e.into_response(),
            Self::Invalid(code, message) => error(S::UNPROCESSABLE_ENTITY, code, message),
            Self::NotFound => error(S::NOT_FOUND, "not_found", "not found"),
            Self::BusinessRequired => error(
                S::CONFLICT,
                "business_required",
                "set up the business first",
            ),
            Self::SlotTaken => error(
                S::CONFLICT,
                "slot_taken",
                "the slot overlaps another booking or hold",
            ),
            Self::SlotUnavailable => error(
                S::UNPROCESSABLE_ENTITY,
                "slot_unavailable",
                "the start is not a bookable slot",
            ),
            Self::HoldExpired => error(S::CONFLICT, "hold_expired", "the hold has expired"),
            Self::IdempotencyConflict => error(
                S::CONFLICT,
                "idempotency_conflict",
                "the idempotency key was used with another request",
            ),
            Self::Conflict(code) => error(
                S::CONFLICT,
                code,
                "the request conflicts with the current state",
            ),
            Self::Limit(which) => error(S::FORBIDDEN, which, "a size limit is reached"),
            Self::Unavailable => error(
                S::SERVICE_UNAVAILABLE,
                "unavailable",
                "the database is unavailable",
            ),
            Self::Internal(e) => {
                tracing::error!(error = %format!("{e:#}"), "scheduling request failed");
                error(S::INTERNAL_SERVER_ERROR, "internal", "internal error")
            }
        }
    }
}
