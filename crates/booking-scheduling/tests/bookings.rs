//! The double-booking guarantee, through the API and below it: one winner among parallel
//! holds, buffers inside the guarded range, an expired hold that stops blocking, and the
//! same rules for a writer that bypasses the server.
#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::many_single_char_names,
    reason = "tests"
)]

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use booking_db::testing::Role;
use common::{Kit, ahead};
use serde_json::json;

const N: usize = 32;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn parallel_holds_for_one_slot_have_exactly_one_winner() {
    let kit = Arc::new(Kit::new().await);
    let t = Arc::new(kit.tenant("race@example.test", "UTC").await);
    let (_, start) = ahead(7, "10:00");
    let barrier = Arc::new(tokio::sync::Barrier::new(N));
    let mut tasks = Vec::new();
    for i in 0..N {
        let (kit, t, start, barrier) = (kit.clone(), t.clone(), start.clone(), barrier.clone());
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            kit.hold(&t, t.staff[0], &start, &format!("race-{i}")).await
        }));
    }
    let mut won = 0;
    let mut taken = 0;
    for task in tasks {
        let answer = task.await.unwrap();
        match answer.status {
            StatusCode::CREATED => won += 1,
            StatusCode::CONFLICT if answer.code() == Some("slot_taken") => taken += 1,
            other => panic!("unexpected {other}: {}", answer.body),
        }
    }
    assert_eq!((won, taken), (1, N - 1));
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM bookings WHERE status IN ('held', 'confirmed')")
            .fetch_one(&kit.db.pool)
            .await
            .unwrap();
    assert_eq!(live, 1);
    drop(t);
    Arc::into_inner(kit).unwrap().db.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn parallel_overlapping_starts_have_exactly_one_winner() {
    // 09:45, 10:00 and 10:15 (30 min + 15 after) each overlap the others' blocked ranges.
    let kit = Arc::new(Kit::new().await);
    let t = Arc::new(kit.tenant("overlap@example.test", "UTC").await);
    let starts = ["09:45", "10:00", "10:15"].map(|h| ahead(7, h).1);
    let barrier = Arc::new(tokio::sync::Barrier::new(N));
    let mut tasks = Vec::new();
    for i in 0..N {
        let (kit, t, start, barrier) = (
            kit.clone(),
            t.clone(),
            starts[i % 3].clone(),
            barrier.clone(),
        );
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            kit.hold(&t, t.staff[0], &start, &format!("o-{i}"))
                .await
                .status
        }));
    }
    let mut won = 0;
    for task in tasks {
        if task.await.unwrap() == StatusCode::CREATED {
            won += 1;
        }
    }
    assert_eq!(won, 1);
    drop(t);
    Arc::into_inner(kit).unwrap().db.cleanup().await;
}

#[tokio::test]
async fn buffers_are_inside_the_guarded_range() {
    let kit = Kit::new().await;
    let t = kit.tenant("buffers@example.test", "UTC").await;
    let first = kit.hold(&t, t.staff[0], &ahead(7, "10:00").1, "a").await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    assert_eq!(first.body["ends_at"], ahead(7, "10:30").1);
    assert_eq!(first.body["blocked_until"], ahead(7, "10:45").1);
    // 10:30 is free of the service but inside its 15-minute buffer after.
    let in_buffer = kit.hold(&t, t.staff[0], &ahead(7, "10:30").1, "b").await;
    assert_eq!(in_buffer.status, StatusCode::CONFLICT, "{}", in_buffer.body);
    assert_eq!(in_buffer.code(), Some("slot_taken"));
    // 09:30 + 30 + 15 = 10:15 overlaps 10:00: the new booking's own buffer counts too.
    let before = kit.hold(&t, t.staff[0], &ahead(7, "09:30").1, "c").await;
    assert_eq!(before.code(), Some("slot_taken"), "{}", before.body);
    assert_eq!(
        kit.hold(&t, t.staff[0], &ahead(7, "10:45").1, "d")
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        kit.hold(&t, t.staff[0], &ahead(7, "09:15").1, "e")
            .await
            .status,
        StatusCode::CREATED
    );
    // Another staff member is another range.
    assert_eq!(
        kit.hold(&t, t.staff[1], &ahead(7, "10:30").1, "f")
            .await
            .status,
        StatusCode::CREATED
    );
    // Availability agrees: 10:30 is gone for staff 0.
    let (date, _) = ahead(7, "00:00");
    let a = kit
        .get(
            &t.session,
            &format!(
                "/v1/workspaces/{}/availability?service={}&staff={}&from={date}",
                t.workspace, t.service, t.staff[0]
            ),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let starts: Vec<&str> = a.body["slots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["start"].as_str().unwrap())
        .collect();
    for taken in [
        "09:15", "09:30", "09:45", "10:00", "10:15", "10:30", "10:45", "11:00", "11:15",
    ] {
        assert!(
            !starts.contains(&ahead(7, taken).1.as_str()),
            "{taken} offered: {starts:?}"
        );
    }
    assert!(starts.contains(&ahead(7, "11:30").1.as_str()));
    kit.db.cleanup().await;
}

async fn expire(kit: &Kit, id: &str) {
    // Time travel for the test only: the administrator lifts the immutability trigger for
    // one transaction and restores it as the migration set it.
    kit.db
        .exec_as(
            Role::Admin,
            &format!(
                "BEGIN;
                 ALTER TABLE bookings DISABLE TRIGGER bookings_transition;
                 UPDATE bookings SET hold_expires_at = now() - interval '1 second' WHERE id = '{id}';
                 ALTER TABLE bookings ENABLE ALWAYS TRIGGER bookings_transition;
                 COMMIT;"
            ),
        )
        .await;
}

#[tokio::test]
async fn an_expired_hold_stops_blocking_and_cannot_be_confirmed() {
    let kit = Kit::new().await;
    let t = kit.tenant("expiry@example.test", "UTC").await;
    let start = ahead(7, "10:00").1;
    let first = kit.hold(&t, t.staff[0], &start, "first").await;
    assert_eq!(first.status, StatusCode::CREATED);
    let id = first.body["id"].as_str().unwrap().to_owned();
    assert_eq!(
        kit.hold(&t, t.staff[0], &start, "second").await.code(),
        Some("slot_taken")
    );
    expire(&kit, &id).await;
    let w = format!("/v1/workspaces/{}", t.workspace);
    assert_eq!(
        kit.get(&t.session, &format!("{w}/bookings/{id}"))
            .await
            .body["status"],
        "expired"
    );
    let again = kit.hold(&t, t.staff[0], &start, "second").await;
    assert_eq!(again.status, StatusCode::CREATED, "{}", again.body);
    let late = kit
        .post(&t.session, &format!("{w}/bookings/{id}/confirm"), None)
        .await;
    assert_eq!(late.status, StatusCode::CONFLICT, "{}", late.body);
    let second = again.body["id"].as_str().unwrap();
    let ok = kit
        .post(&t.session, &format!("{w}/bookings/{second}/confirm"), None)
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    assert_eq!(ok.body["status"], "confirmed");
    kit.db.cleanup().await;
}

#[tokio::test]
async fn cancelling_frees_the_slot_and_holds_are_idempotent() {
    let kit = Kit::new().await;
    let t = kit.tenant("idem@example.test", "UTC").await;
    let w = format!("/v1/workspaces/{}", t.workspace);
    let start = ahead(7, "12:00").1;
    let first = kit.hold(&t, t.staff[0], &start, "k1").await;
    assert_eq!(first.status, StatusCode::CREATED);
    let replay = kit.hold(&t, t.staff[0], &start, "k1").await;
    assert_eq!(replay.status, StatusCode::OK, "{}", replay.body);
    assert_eq!(replay.body["id"], first.body["id"]);
    let reuse = kit.hold(&t, t.staff[1], &start, "k1").await;
    assert_eq!(reuse.code(), Some("idempotency_conflict"), "{}", reuse.body);
    let id = first.body["id"].as_str().unwrap();
    let c = kit
        .post(&t.session, &format!("{w}/bookings/{id}/cancel"), None)
        .await;
    assert_eq!(c.body["status"], "cancelled", "{}", c.body);
    let twice = kit
        .post(&t.session, &format!("{w}/bookings/{id}/confirm"), None)
        .await;
    assert_eq!(twice.code(), Some("invalid_transition"), "{}", twice.body);
    assert_eq!(
        kit.hold(&t, t.staff[0], &start, "k2").await.status,
        StatusCode::CREATED
    );
    // Not on the grid / outside hours / in the past.
    assert_eq!(
        kit.hold(&t, t.staff[0], &ahead(7, "12:07").1, "k3")
            .await
            .code(),
        Some("slot_unavailable")
    );
    assert_eq!(
        kit.hold(&t, t.staff[0], &ahead(7, "17:45").1, "k4")
            .await
            .code(),
        Some("slot_unavailable")
    );
    assert_eq!(
        kit.hold(&t, t.staff[0], &ahead(-1, "12:00").1, "k5")
            .await
            .code(),
        Some("slot_unavailable")
    );
    kit.db.cleanup().await;
}

/// Below the server: a writer with the runtime role's own SQL meets the same rules.
#[tokio::test]
async fn the_database_enforces_ranges_and_expiry_without_the_server() {
    let kit = Kit::new().await;
    let t = kit.tenant("sql@example.test", "UTC").await;
    let insert = |key: &str, minutes: i32| {
        format!(
            "INSERT INTO bookings (business_id, id, staff_id, service_id, status, starts_at, ends_at,
                 buffer_before_minutes, buffer_after_minutes, blocked, hold_expires_at,
                 customer_name, idempotency_key, created_at, updated_at)
             VALUES ('{w}', gen_random_uuid(), '{s}', '{v}', 'held', '{at}', '{at}', 0, 0,
                 tstzrange('{at}', '{at}'::timestamptz + interval '1 minute'),
                 now() + make_interval(mins => {minutes}), 'X', '{key}', now(), now()) RETURNING id",
            w = t.workspace, s = t.staff[0], v = t.service, at = ahead(7, "10:00").1,
        )
    };
    let pool = &kit.db.pool;
    // A forged one-minute range is replaced by the service's buffered one.
    let id: uuid::Uuid = sqlx::query_scalar(sqlx::AssertSqlSafe(insert("s1", 10)))
        .fetch_one(pool)
        .await
        .unwrap();
    let until: String =
        sqlx::query_scalar("SELECT upper(blocked)::text FROM bookings WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(until.contains("10:45:00"), "{until}");
    let clash = sqlx::query(sqlx::AssertSqlSafe(insert("s2", 10)))
        .execute(pool)
        .await
        .unwrap_err();
    assert_eq!(
        clash.as_database_error().unwrap().code().as_deref(),
        Some("23P01")
    );
    // A hold already past its expiry is refused, not stored blocking.
    let stale = sqlx::query(sqlx::AssertSqlSafe(insert("s3", -1)))
        .execute(pool)
        .await
        .unwrap_err();
    assert_eq!(
        stale.as_database_error().unwrap().code().as_deref(),
        Some("BK001")
    );
    expire(&kit, &id.to_string()).await;
    sqlx::query(sqlx::AssertSqlSafe(insert("s2", 10)))
        .execute(pool)
        .await
        .unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM bookings WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(status, "expired");
    // A booking's times are immutable and it cannot be deleted by the runtime role.
    let moved = sqlx::query("UPDATE bookings SET starts_at = starts_at + interval '1 hour'")
        .execute(pool)
        .await
        .unwrap_err();
    assert_eq!(
        moved.as_database_error().unwrap().code().as_deref(),
        Some("BK004")
    );
    let deleted = sqlx::query("DELETE FROM bookings")
        .execute(pool)
        .await
        .unwrap_err();
    assert_eq!(
        deleted.as_database_error().unwrap().code().as_deref(),
        Some("42501")
    );
    let _ = json!(null);
    kit.db.cleanup().await;
}
