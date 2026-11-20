//! The product's backend, on the accounts kit: each workspace is one business (tenant).
//! Every route is under `/v1/workspaces/{workspace}`, authorized with
//! [`booking_accounts::Accounts::access`], and every query is keyed by that workspace —
//! a caller never names another tenant's row, whatever ids it sends. The API is in
//! `docs/SCHEDULING.md`.

pub mod domain;
pub mod error;
mod routes;
pub mod store;

use std::sync::Arc;
use std::time::Duration;

use booking_accounts::Accounts;
use booking_http::budget::{PerKey, Rate};
use sqlx::PgPool;
use tokio::sync::Semaphore;

pub use crate::routes::{router, zone};

/// The bounds on what one caller may make the server do (D-018).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// Grid points (staff × days × points a day) one availability read may walk.
    pub max_cells: u64,
    /// Availability computations at once, off the async workers.
    pub compute_permits: usize,
    /// How long a read waits for a compute permit before it is refused (503).
    pub compute_wait: Duration,
    /// Availability reads per principal (token or member).
    pub read_rate: Rate,
    /// `POST …/holds` per principal.
    pub hold_rate: Rate,
    /// Open (unexpired) holds per principal per business.
    pub open_holds: u32,
    /// Principals the rate budgets track at once (bounded memory).
    pub tracked_principals: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_cells: 300_000,
            compute_permits: std::thread::available_parallelism().map_or(2, std::num::NonZero::get),
            compute_wait: Duration::from_secs(1),
            read_rate: Rate::new(60, Duration::from_millis(50)),
            hold_rate: Rate::new(20, Duration::from_millis(500)),
            open_holds: 50,
            tracked_principals: 10_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Scheduling {
    accounts: Accounts,
    db: PgPool,
    limits: Limits,
    compute: Arc<Semaphore>,
    reads: PerKey<uuid::Uuid>,
    holds: PerKey<uuid::Uuid>,
}

impl Scheduling {
    /// With the default [`Limits`].
    #[must_use]
    pub fn new(accounts: Accounts, db: PgPool) -> Self {
        Self::with_limits(accounts, db, Limits::default())
    }

    #[must_use]
    pub fn with_limits(accounts: Accounts, db: PgPool, limits: Limits) -> Self {
        Self {
            accounts,
            db,
            limits,
            compute: Arc::new(Semaphore::new(limits.compute_permits.max(1))),
            reads: PerKey::new(limits.read_rate, limits.tracked_principals),
            holds: PerKey::new(limits.hold_rate, limits.tracked_principals),
        }
    }
}
