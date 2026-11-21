//! Compute permits for availability reads (D-018): a bounded number at once in all, and a
//! share of them per workspace, so one tenant's reads cannot hold every permit while
//! another tenant's reads wait out their deadline.

use std::hash::BuildHasher;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

/// Workspace shares are striped, not one per workspace: bounded memory, nothing to evict.
/// The stripe hash is keyed per process, so a caller cannot choose a colliding workspace.
const STRIPES: usize = 256;

#[derive(Debug, Clone)]
pub struct Compute {
    all: Arc<Semaphore>,
    stripes: Arc<[Arc<Semaphore>]>,
    hasher: std::collections::hash_map::RandomState,
}

/// Held while one read computes; both permits return on drop.
#[derive(Debug)]
pub struct Permit {
    _share: OwnedSemaphorePermit,
    _all: OwnedSemaphorePermit,
}

impl Compute {
    /// `permits` at once in all, at most `share` of them for one workspace.
    #[must_use]
    pub fn new(permits: usize, share: usize) -> Self {
        let permits = permits.max(1);
        let share = share.clamp(1, permits); // LIMIT(compute-share)
        Self {
            all: Arc::new(Semaphore::new(permits)),
            stripes: (0..STRIPES)
                .map(|_| Arc::new(Semaphore::new(share)))
                .collect(),
            hasher: std::collections::hash_map::RandomState::new(),
        }
    }

    /// Whether two workspaces draw on the same share (a stripe collision).
    #[doc(hidden)]
    #[must_use]
    pub fn same_share(&self, a: Uuid, b: Uuid) -> bool {
        self.stripe(a) == self.stripe(b)
    }

    fn stripe(&self, workspace: Uuid) -> usize {
        usize::try_from(self.hasher.hash_one(workspace)).unwrap_or(0) % STRIPES
    }

    /// A permit for a read of `workspace`, or `None` once `wait` has passed.
    pub async fn acquire(&self, workspace: Uuid, wait: Duration) -> Option<Permit> {
        let deadline = tokio::time::Instant::now() + wait;
        let share = self.stripes.get(self.stripe(workspace))?.clone();
        let share = tokio::time::timeout_at(deadline, share.acquire_owned())
            .await
            .ok()?
            .ok()?;
        let all = tokio::time::timeout_at(deadline, self.all.clone().acquire_owned())
            .await
            .ok()?
            .ok()?;
        Some(Permit {
            _share: share,
            _all: all,
        })
    }
}
