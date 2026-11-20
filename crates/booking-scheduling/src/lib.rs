//! The product's backend, on the accounts kit: each workspace is one business (tenant).
//! Every route is under `/v1/workspaces/{workspace}`, authorized with
//! [`booking_accounts::Accounts::access`], and every query is keyed by that workspace —
//! a caller never names another tenant's row, whatever ids it sends. The API is in
//! `docs/SCHEDULING.md`.

pub mod domain;
pub mod error;
mod routes;
pub mod store;

use booking_accounts::Accounts;
use sqlx::PgPool;

pub use crate::routes::{router, zone};

/// What the routes need: the kit (authorization) and the pool.
#[derive(Debug, Clone)]
pub struct Scheduling {
    accounts: Accounts,
    db: PgPool,
}

impl Scheduling {
    #[must_use]
    pub fn new(accounts: Accounts, db: PgPool) -> Self {
        Self { accounts, db }
    }
}
