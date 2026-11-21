//! The hold path's locks, each proven deterministically (D-018): a concurrent transaction
//! holds the contended lock, the test waits until the request is seen waiting on exactly
//! the lock under test (`pg_stat_activity`), then lets go. No timing, no retries: each is
//! the target of one mutant in `scripts/prove-limits.sh`.
#![allow(clippy::unwrap_used, reason = "tests")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use booking_db::testing::Role;
use common::{Kit, Tenant, ahead, generous};
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// Holds queued behind one staff member's in-flight hold.
const HOLDS: usize = 6;

/// Backends of the server's role waiting on a lock while running a query like `pattern`.
async fn waiting(pool: &PgPool, pattern: &str, event: Option<&str>) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
          WHERE datname = current_database() AND pid <> pg_backend_pid()
            AND wait_event_type = 'Lock' AND ($2::text IS NULL OR wait_event = $2)
            AND query ILIKE $1",
    )
    .bind(pattern)
    .bind(event)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Until a backend waits on `pattern`; panics if `task` answers first.
async fn until_waiting<T>(
    pool: &PgPool,
    pattern: &str,
    event: Option<&str>,
    task: &tokio::task::JoinHandle<T>,
) {
    for _ in 0..6_000 {
        if waiting(pool, pattern, event).await >= 1 {
            return;
        }
        assert!(
            !task.is_finished(),
            "the request answered without waiting on {pattern}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the request never waited on {pattern}");
}

/// A hold inserted the way a concurrent server transaction would.
async fn insert_hold(
    x: &mut PgConnection,
    t: &Tenant,
    staff: Uuid,
    at: &str,
    key: &str,
    held_by: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO bookings (business_id, id, staff_id, service_id, status, starts_at, ends_at,
                               buffer_before_minutes, buffer_after_minutes, blocked, hold_expires_at,
                               customer_name, idempotency_key, created_at, updated_at, held_by)
         VALUES ($1, $2, $3, $4, 'held', $5::timestamptz, $5::timestamptz, 0, 0, 'empty',
                 now() + interval '10 minutes', 'X', $6, now(), now(), $7)",
    )
    .bind(t.workspace)
    .bind(Uuid::now_v7())
    .bind(staff)
    .bind(t.service)
    .bind(at)
    .bind(key)
    .bind(held_by)
    .execute(&mut *x)
    .await
    .map(|_| ())
}

async fn live(pool: &PgPool, t: &Tenant, staff: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM bookings WHERE business_id = $1 AND staff_id = $2
            AND status = 'held' AND hold_expires_at > now()",
    )
    .bind(t.workspace)
    .bind(staff)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn cleanup(kit: Arc<Kit>) {
    let Ok(kit) = Arc::try_unwrap(kit) else {
        panic!("a request outlived its test");
    };
    kit.db.cleanup().await;
}

async fn setup(limits: booking_scheduling::Limits, email: &str) -> (Arc<Kit>, Arc<Tenant>, PgPool) {
    let kit = Arc::new(Kit::bounded(|_| {}, limits).await);
    let t = Arc::new(kit.tenant(email, "UTC").await);
    let owner = kit.db.connect_as(Role::Owner).await;
    (kit, t, owner)
}

#[tokio::test]
async fn a_staff_write_and_a_hold_on_the_same_staff_never_deadlock() {
    let (kit, t, owner) = setup(generous(), "lock-order@example.test").await;
    let staff = t.staff[0];
    // An in-flight hold: it has its staff member's lock and has yet to insert.
    let mut x = owner.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM staff WHERE business_id = $1 AND id = $2 FOR UPDATE")
        .bind(t.workspace)
        .bind(staff)
        .execute(&mut *x)
        .await
        .unwrap();
    // A staff write locks the business, then waits for that staff row.
    let rename = {
        let (kit, t) = (kit.clone(), t.clone());
        tokio::spawn(async move {
            kit.put(
                &t.session,
                &format!("/v1/workspaces/{}/staff/{staff}", t.workspace),
                json!({ "name": "Renamed" }),
            )
            .await
        })
    };
    until_waiting(&kit.db.pool, "INSERT INTO staff%", None, &rename).await;
    // The hold's insert takes KEY SHARE on the business through its foreign key: it must
    // not wait for the staff write, or the two wait for each other.
    insert_hold(&mut x, &t, staff, &ahead(7, "10:00").1, "x", None)
        .await
        .expect("the hold's insert must not deadlock with the staff write");
    x.commit().await.unwrap();
    let renamed = rename.await.unwrap();
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);
    assert_eq!(live(&kit.db.pool, &t, staff).await, 1);
    cleanup(kit).await;
}

#[tokio::test]
async fn holds_for_one_staff_member_queue_in_memory_not_on_connections() {
    let (kit, t, owner) = setup(generous(), "staff-queue@example.test").await;
    let staff = t.staff[0];
    let mut x = owner.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM staff WHERE business_id = $1 AND id = $2 FOR UPDATE")
        .bind(t.workspace)
        .bind(staff)
        .execute(&mut *x)
        .await
        .unwrap();
    let tasks: Vec<_> = (0..HOLDS)
        .map(|i| {
            let (kit, t) = (kit.clone(), t.clone());
            tokio::spawn(async move {
                let at = ahead(7, &format!("{:02}:00", 8 + i)).1;
                kit.hold(&t, staff, &at, &format!("q{i}")).await
            })
        })
        .collect();
    // One hold waits on the row lock with a connection; the others wait for their turn in
    // memory. Without the queue every one of them would hold a connection on the lock.
    let mut queued = false;
    for _ in 0..6_000 {
        let on_lock = waiting(&kit.db.pool, "%FROM staff%FOR UPDATE%", None).await;
        assert!(
            on_lock <= 1,
            "{on_lock} holds wait on the staff lock with a connection each"
        );
        if on_lock == 1 && kit.scheduling.holds_waiting() == HOLDS - 1 {
            queued = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(queued, "the holds never queued");
    x.rollback().await.unwrap();
    for task in tasks {
        let h = task.await.unwrap();
        assert_eq!(h.status, StatusCode::CREATED, "{}", h.body);
    }
    assert_eq!(
        live(&kit.db.pool, &t, staff).await,
        i64::try_from(HOLDS).unwrap()
    );
    cleanup(kit).await;
}

#[tokio::test]
async fn a_hold_waits_on_the_staff_lock_and_the_constraint_refuses_the_overlap() {
    let (kit, t, owner) = setup(generous(), "staff-lock@example.test").await;
    let staff = t.staff[0];
    // A concurrent hold, 10:00-10:45 blocked, inserted but not committed: invisible to the
    // pre-check, and its foreign key holds KEY SHARE on the staff row.
    let mut x = owner.begin().await.unwrap();
    insert_hold(&mut x, &t, staff, &ahead(7, "10:00").1, "x", None)
        .await
        .unwrap();
    let hold = {
        let (kit, t) = (kit.clone(), t.clone());
        tokio::spawn(async move { kit.hold(&t, staff, &ahead(7, "10:15").1, "api").await })
    };
    // It waits on the staff lock, not inside the exclusion check.
    until_waiting(&kit.db.pool, "%FROM staff%FOR UPDATE%", None, &hold).await;
    assert_eq!(
        waiting(&kit.db.pool, "INSERT INTO bookings%", None).await,
        0
    );
    x.commit().await.unwrap();
    // Then the constraint meets the committed overlap: 409, never a second live hold.
    let lost = hold.await.unwrap();
    assert_eq!(lost.status, StatusCode::CONFLICT, "{}", lost.body);
    assert_eq!(lost.code(), Some("slot_taken"));
    assert_eq!(live(&kit.db.pool, &t, staff).await, 1);
    cleanup(kit).await;
}

#[tokio::test]
async fn the_open_hold_cap_counts_a_concurrent_hold_under_the_principal_lock() {
    let limits = booking_scheduling::Limits {
        open_holds: 1,
        ..generous()
    };
    let (kit, t, owner) = setup(limits, "principal-lock@example.test").await;
    let principal: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE email = $1")
        .bind("principal-lock@example.test")
        .fetch_one(&owner)
        .await
        .unwrap();
    // The same principal's other hold, in flight: it has the principal lock, and its row
    // is not committed, so a count that does not wait for the lock misses it.
    let mut x = owner.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text || '/' || $2::text, 0))")
        .bind(t.workspace)
        .bind(principal)
        .execute(&mut *x)
        .await
        .unwrap();
    insert_hold(
        &mut x,
        &t,
        t.staff[1],
        &ahead(7, "12:00").1,
        "x",
        Some(principal),
    )
    .await
    .unwrap();
    let hold = {
        let (kit, t) = (kit.clone(), t.clone());
        tokio::spawn(async move { kit.hold(&t, t.staff[0], &ahead(7, "10:00").1, "api").await })
    };
    until_waiting(
        &kit.db.pool,
        "%pg_advisory_xact_lock%",
        Some("advisory"),
        &hold,
    )
    .await;
    x.commit().await.unwrap();
    let refused = hold.await.unwrap();
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.code(), Some("open_holds"));
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM bookings WHERE business_id = $1 AND held_by = $2
            AND status = 'held' AND hold_expires_at > now()",
    )
    .bind(t.workspace)
    .bind(principal)
    .fetch_one(&kit.db.pool)
    .await
    .unwrap();
    assert_eq!(open, 1);
    cleanup(kit).await;
}
