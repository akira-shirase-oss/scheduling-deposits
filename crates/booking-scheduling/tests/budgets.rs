//! What one caller may make the server do (D-018): the availability work budget and its
//! cursor, and the per-principal read rate, hold rate and open-hold cap. Each test is the
//! proof of one `LIMIT(...)` line (`scripts/prove-limits.sh`).
#![allow(clippy::unwrap_used, reason = "tests")]

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use booking_http::budget::Rate;
use common::{Auth, Kit, ahead, generous};
use serde_json::json;

const PEER: &str = "203.0.113.1:1000";

async fn five_minute_grid(kit: &Kit, t: &common::Tenant) {
    let w = format!("/v1/workspaces/{}", t.workspace);
    let b = kit
        .put(
            &t.session,
            &format!("{w}/business"),
            json!({
        "name": "Studio", "time_zone": "UTC", "currency": "EUR", "slot_step_minutes": 5,
        "min_notice_minutes": 0, "max_advance_days": 365, "hold_minutes": 10 }),
        )
        .await;
    assert_eq!(b.status, StatusCode::OK, "{}", b.body);
}

#[tokio::test]
async fn a_read_over_the_work_budget_is_refused_before_the_engine_runs() {
    // 2 staff × 288 points a day: one day is 576 cells, two days 1,152.
    let limits = booking_scheduling::Limits {
        max_cells: 1_000,
        ..generous()
    };
    let kit = Kit::bounded(|_| {}, limits).await;
    let t = kit.tenant("cells@example.test", "UTC").await;
    five_minute_grid(&kit, &t).await;
    let w = format!("/v1/workspaces/{}", t.workspace);
    let (date, _) = ahead(2, "00:00");
    let read = |days: u32, staff: Option<uuid::Uuid>| {
        let staff = staff.map(|s| format!("&staff={s}")).unwrap_or_default();
        format!(
            "{w}/availability?service={}&from={date}&days={days}{staff}",
            t.service
        )
    };
    assert_eq!(
        kit.get(&t.session, &read(1, None)).await.status,
        StatusCode::OK
    );
    let over = kit.get(&t.session, &read(2, None)).await;
    assert_eq!(
        over.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        over.body
    );
    assert_eq!(over.code(), Some("too_much_work"));
    // Narrowed to one staff member, the same dates fit.
    let one = kit.get(&t.session, &read(1, Some(t.staff[0]))).await;
    assert_eq!(one.status, StatusCode::OK, "{}", one.body);
    kit.db.cleanup().await;
}

#[tokio::test]
async fn exceptions_count_in_the_work_budget() {
    // 2 staff × 96 points: one day is 192 cells, well inside the budget. 200 business-wide
    // closures of one minute each are work the cell count does not see.
    let limits = booking_scheduling::Limits {
        max_cells: 1_000,
        ..generous()
    };
    let kit = Kit::bounded(|_| {}, limits).await;
    let t = kit.tenant("exceptions-work@example.test", "UTC").await;
    let w = format!("/v1/workspaces/{}", t.workspace);
    let (date, _) = ahead(3, "00:00");
    let read = format!("{w}/availability?service={}&from={date}&days=1", t.service);
    assert_eq!(kit.get(&t.session, &read).await.status, StatusCode::OK);
    let (first, _) = ahead(1, "00:00");
    let (last, _) = ahead(30, "00:00");
    for k in 0..200u16 {
        let (from, until) = (k * 7, k * 7 + 1);
        let e = kit
            .put(
                &t.session,
                &format!("{w}/exceptions/{}", uuid::Uuid::now_v7()),
                json!({ "first_date": first, "last_date": last, "kind": "closed",
                        "start": format!("{:02}:{:02}", from / 60, from % 60),
                        "end": format!("{:02}:{:02}", until / 60, until % 60) }),
            )
            .await;
        assert_eq!(e.status, StatusCode::CREATED, "{}", e.body);
    }
    let over = kit.get(&t.session, &read).await;
    assert_eq!(
        over.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        over.body
    );
    assert_eq!(over.code(), Some("too_much_work"));
    kit.db.cleanup().await;
}

#[tokio::test]
async fn busy_rows_count_in_the_work_budget() {
    // One staff member, one day: 96 cells and about 215 units of engine work (grid, walk,
    // dates and hours); 52 busy rows take it past 240.
    let limits = booking_scheduling::Limits {
        max_cells: 240,
        ..generous()
    };
    let kit = Kit::bounded(|_| {}, limits).await;
    let t = kit.tenant("busy-work@example.test", "UTC").await;
    let w = format!("/v1/workspaces/{}", t.workspace);
    let (date, _) = ahead(3, "00:00");
    let read = format!(
        "{w}/availability?service={}&from={date}&days=1&staff={}",
        t.service, t.staff[0]
    );
    let fits = kit.get(&t.session, &read).await;
    assert_eq!(fits.status, StatusCode::OK, "{}", fits.body);
    // 13 holds a day (45 minutes apart, 08:00-17:30) on the read's day and the days
    // around it: every one is a busy row the read loads.
    let mut made = 0;
    for day in 2..=5 {
        for slot in 0..13 {
            let minute = 8 * 60 + slot * 45;
            let at = ahead(day, &format!("{:02}:{:02}", minute / 60, minute % 60)).1;
            let h = kit.hold(&t, t.staff[0], &at, &format!("busy-{made}")).await;
            assert_eq!(h.status, StatusCode::CREATED, "{}", h.body);
            made += 1;
        }
    }
    let over = kit.get(&t.session, &read).await;
    assert_eq!(
        over.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        over.body
    );
    assert_eq!(over.code(), Some("too_much_work"));
    kit.db.cleanup().await;
}

#[tokio::test]
async fn one_workspace_cannot_hold_every_compute_permit() {
    let compute = booking_scheduling::compute::Compute::new(4, 1);
    let a = uuid::Uuid::now_v7();
    let b = std::iter::repeat_with(uuid::Uuid::now_v7)
        .find(|b| !compute.same_share(a, *b))
        .unwrap();
    let wait = Duration::from_secs(30);
    let held = compute.acquire(a, wait).await.unwrap();
    // A's share is spent: its next read waits out its deadline and is refused (503) ...
    assert!(
        compute
            .acquire(a, Duration::from_millis(20))
            .await
            .is_none()
    );
    // ... while another workspace's reads still get permits.
    let _b1 = compute.acquire(b, wait).await.unwrap();
    drop(held);
    let _a = compute.acquire(a, wait).await.unwrap();
}

#[tokio::test]
async fn paging_with_after_visits_every_slot_once_in_order() {
    let kit = Kit::new().await;
    let t = kit.tenant("pages@example.test", "UTC").await;
    five_minute_grid(&kit, &t).await;
    let w = format!("/v1/workspaces/{}", t.workspace);
    let (date, _) = ahead(2, "00:00");
    let base = format!("{w}/availability?service={}&from={date}&days=31", t.service);
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        let path = match &after {
            None => base.clone(),
            Some(a) => format!("{base}&after={}", a.replace('+', "%2B")),
        };
        let a = kit.get(&t.session, &path).await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.body);
        let ids = a.body["staff_ids"].as_array().unwrap();
        let starts = a.body["starts"].as_array().unwrap();
        let mut pairs = 0;
        for at in starts {
            for i in at["staff"].as_array().unwrap() {
                let staff = ids[usize::try_from(i.as_u64().unwrap()).unwrap()]
                    .as_str()
                    .unwrap();
                seen.push((at["at"].as_str().unwrap().to_owned(), staff.to_owned()));
                pairs += 1;
            }
        }
        assert!(pairs <= 5_000);
        pages += 1;
        if a.body["truncated"] == false {
            assert!(a.body["next_after"].is_null());
            break;
        }
        let next = a.body["next_after"].as_str().unwrap().to_owned();
        assert_eq!(
            starts.last().unwrap()["at"],
            next.as_str(),
            "a page ends on a whole instant"
        );
        after = Some(next);
        assert!(pages < 20);
    }
    assert!(pages > 1, "the answer spans pages");
    let unique: BTreeSet<_> = seen.iter().cloned().collect();
    assert_eq!(unique.len(), seen.len(), "no slot twice");
    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(sorted, seen, "in start order across pages");
    // Every day's last bookable slot (17:30 for a 30-minute service, 08:00-18:00) is reached.
    let last_day = seen
        .iter()
        .filter(|(s, _)| s.ends_with("T17:30:00Z"))
        .count();
    assert_eq!(last_day, 31 * 2);
    let bad = kit
        .get(&t.session, &format!("{base}&after=yesterday"))
        .await;
    assert_eq!(bad.code(), Some("invalid_after"));
    kit.db.cleanup().await;
}

#[tokio::test]
async fn availability_reads_are_rate_limited_per_principal() {
    let limits = booking_scheduling::Limits {
        read_rate: Rate::new(3, Duration::from_secs(3600)),
        ..generous()
    };
    let kit = Kit::bounded(|_| {}, limits).await;
    let t = kit.tenant("reads@example.test", "UTC").await;
    let (_, token) = kit.token(&t.session, t.workspace).await;
    let (date, _) = ahead(2, "00:00");
    let path = format!(
        "/v1/workspaces/{}/availability?service={}&from={date}",
        t.workspace, t.service
    );
    for _ in 0..3 {
        assert_eq!(
            kit.call(PEER, Method::GET, &path, Auth::Token(&token), None)
                .await
                .status,
            StatusCode::OK
        );
    }
    let spent = kit
        .call(PEER, Method::GET, &path, Auth::Token(&token), None)
        .await;
    assert_eq!(
        spent.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        spent.body
    );
    assert_eq!(spent.code(), Some("rate_limited"));
    // Another principal of the same workspace is not affected.
    assert_eq!(kit.get(&t.session, &path).await.status, StatusCode::OK);
    kit.db.cleanup().await;
}

#[tokio::test]
async fn holds_are_rate_limited_per_principal() {
    let limits = booking_scheduling::Limits {
        hold_rate: Rate::new(3, Duration::from_secs(3600)),
        ..generous()
    };
    let kit = Kit::bounded(|_| {}, limits).await;
    let t = kit.tenant("hold-rate@example.test", "UTC").await;
    for (i, at) in ["09:00", "10:00", "11:00"].iter().enumerate() {
        let h = kit
            .hold(&t, t.staff[0], &ahead(7, at).1, &format!("r{i}"))
            .await;
        assert_eq!(h.status, StatusCode::CREATED, "{}", h.body);
    }
    let spent = kit.hold(&t, t.staff[0], &ahead(7, "12:00").1, "r3").await;
    assert_eq!(
        spent.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        spent.body
    );
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM bookings")
        .fetch_one(&kit.db.pool)
        .await
        .unwrap();
    assert_eq!(live, 3);
    kit.db.cleanup().await;
}

#[tokio::test]
async fn open_holds_are_capped_per_principal_even_concurrently() {
    let limits = booking_scheduling::Limits {
        open_holds: 3,
        ..generous()
    };
    let kit = std::sync::Arc::new(Kit::bounded(|_| {}, limits).await);
    let t = std::sync::Arc::new(kit.tenant("open-holds@example.test", "UTC").await);
    let mut tasks = Vec::new();
    for i in 0..12 {
        let (kit, t) = (kit.clone(), t.clone());
        tasks.push(tokio::spawn(async move {
            let staff = t.staff[i % 2];
            let at = format!("{:02}:00", 8 + i / 2);
            kit.hold(&t, staff, &ahead(7, &at).1, &format!("c{i}"))
                .await
        }));
    }
    let mut created = 0;
    for task in tasks {
        let h = task.await.unwrap();
        match h.status {
            StatusCode::CREATED => created += 1,
            StatusCode::FORBIDDEN => assert_eq!(h.code(), Some("open_holds"), "{}", h.body),
            other => panic!("unexpected {other}: {}", h.body),
        }
    }
    assert_eq!(created, 3);
    // A retry of a key already held is answered, not refused by the cap.
    let key: String = sqlx::query_scalar("SELECT idempotency_key FROM bookings LIMIT 1")
        .fetch_one(&kit.db.pool)
        .await
        .unwrap();
    let (staff, starts_at): (uuid::Uuid, String) = sqlx::query_as(
        "SELECT staff_id, to_char(starts_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') FROM bookings WHERE idempotency_key = $1",
    )
    .bind(&key)
    .fetch_one(&kit.db.pool)
    .await
    .unwrap();
    assert_eq!(
        kit.hold(&t, staff, &starts_at, &key).await.status,
        StatusCode::OK
    );
    // A cancelled hold no longer counts.
    sqlx::query("UPDATE bookings SET status = 'cancelled' WHERE idempotency_key = $1")
        .bind(&key)
        .execute(&kit.db.pool)
        .await
        .unwrap();
    let again = kit
        .hold(&t, t.staff[0], &ahead(8, "09:00").1, "after-cancel")
        .await;
    assert_eq!(again.status, StatusCode::CREATED, "{}", again.body);
    drop(t);
    std::sync::Arc::into_inner(kit).unwrap().db.cleanup().await;
}
