//! One staff member's holds, one at a time, in process (D-018): a hold waits for its
//! staff member's turn here, before it takes a pool connection, so contention for one
//! staff member queues in memory and never fills the pool with transactions waiting on a
//! row lock. The staff row lock in the transaction still decides across processes.

use std::hash::BuildHasher;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, OwnedMutexGuard};
use uuid::Uuid;

/// Striped by (business, staff): bounded memory, nothing to evict. A collision only makes
/// two staff members share a turn; the hash is keyed per process, so it cannot be chosen.
const STRIPES: usize = 1024;

#[derive(Debug, Clone)]
pub struct StaffQueue {
    stripes: Arc<[Arc<Mutex<()>>]>,
    hasher: std::collections::hash_map::RandomState,
    waiting: Arc<AtomicUsize>,
}

/// A staff member's turn: held while the hold's transaction runs.
#[derive(Debug)]
pub struct Turn {
    _guard: Option<OwnedMutexGuard<()>>,
}

impl Default for StaffQueue {
    fn default() -> Self {
        Self {
            stripes: (0..STRIPES).map(|_| Arc::new(Mutex::new(()))).collect(),
            hasher: std::collections::hash_map::RandomState::new(),
            waiting: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl StaffQueue {
    /// The staff member's turn, or `None` once `wait` has passed.
    pub async fn enter(&self, business: Uuid, staff: Uuid, wait: Duration) -> Option<Turn> {
        let stripe =
            usize::try_from(self.hasher.hash_one((business, staff))).unwrap_or(0) % STRIPES;
        let lock = self.stripes.get(stripe)?.clone();
        self.waiting.fetch_add(1, Ordering::Relaxed);
        let guard = tokio::time::timeout(wait, lock.lock_owned()).await;
        self.waiting.fetch_sub(1, Ordering::Relaxed);
        let guard = guard.ok()?;
        Some(Turn {
            _guard: Some(guard),
        }) // LIMIT(staff-queue)
    }

    /// Holds waiting for a turn now.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Relaxed)
    }
}
