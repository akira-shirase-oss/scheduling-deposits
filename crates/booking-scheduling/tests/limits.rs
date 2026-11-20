//! Each per-business bound holds at its edge: the next row is refused with the limit's
//! code, an edit of an existing row still succeeds, and reads are capped.
#![allow(clippy::unwrap_used, reason = "tests")]

mod common;

use axum::http::StatusCode;
use common::{Kit, Tenant, ahead};
use serde_json::json;
use uuid::Uuid;

async fn fill(kit: &Kit, t: &Tenant, sql: &str) {
    sqlx::query(sqlx::AssertSqlSafe(
        sql.replace("$W", &t.workspace.to_string()),
    ))
    .execute(&kit.db.pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn services_are_capped_per_business() {
    let kit = Kit::new().await;
    let t = kit.tenant("services@example.test", "UTC").await;
    fill(&kit, &t, "INSERT INTO services (business_id, id, name, duration_minutes, buffer_before_minutes,
        buffer_after_minutes, price_minor, deposit_kind, deposit_value, active)
        SELECT '$W', gen_random_uuid(), 'S' || g, 30, 0, 0, 0, 'none', 0, true FROM generate_series(1, 199) g").await;
    let w = format!("/v1/workspaces/{}", t.workspace);
    let body = json!({ "name": "X", "duration_minutes": 30, "price_minor": 0, "deposit": { "kind": "none" } });
    let over = kit
        .put(
            &t.session,
            &format!("{w}/services/{}", Uuid::now_v7()),
            body.clone(),
        )
        .await;
    assert_eq!(over.code(), Some("services_limit"), "{}", over.body);
    let edit = kit
        .put(&t.session, &format!("{w}/services/{}", t.service), body)
        .await;
    assert_eq!(edit.status, StatusCode::OK, "{}", edit.body);
    kit.db.cleanup().await;
}

#[tokio::test]
async fn staff_are_capped_per_business() {
    let kit = Kit::new().await;
    let t = kit.tenant("staff@example.test", "UTC").await;
    fill(
        &kit,
        &t,
        "INSERT INTO staff (business_id, id, name, active)
        SELECT '$W', gen_random_uuid(), 'P' || g, true FROM generate_series(1, 98) g",
    )
    .await;
    let w = format!("/v1/workspaces/{}", t.workspace);
    let over = kit
        .put(
            &t.session,
            &format!("{w}/staff/{}", Uuid::now_v7()),
            json!({ "name": "X" }),
        )
        .await;
    assert_eq!(over.code(), Some("staff_limit"), "{}", over.body);
    let edit = kit
        .put(
            &t.session,
            &format!("{w}/staff/{}", t.staff[0]),
            json!({ "name": "X" }),
        )
        .await;
    assert_eq!(edit.status, StatusCode::OK, "{}", edit.body);
    kit.db.cleanup().await;
}

#[tokio::test]
async fn exceptions_are_capped_per_business() {
    let kit = Kit::new().await;
    let t = kit.tenant("exceptions@example.test", "UTC").await;
    fill(
        &kit,
        &t,
        "INSERT INTO hours_exceptions (business_id, id, first_date, last_date, kind)
        SELECT '$W', gen_random_uuid(), DATE '2030-01-01' + g, DATE '2030-01-01' + g, 'closed'
          FROM generate_series(1, 1000) g",
    )
    .await;
    let w = format!("/v1/workspaces/{}", t.workspace);
    let body = json!({ "first_date": "2031-01-01", "last_date": "2031-01-01", "kind": "closed" });
    let over = kit
        .put(
            &t.session,
            &format!("{w}/exceptions/{}", Uuid::now_v7()),
            body,
        )
        .await;
    assert_eq!(over.code(), Some("exceptions_limit"), "{}", over.body);
    kit.db.cleanup().await;
}

#[tokio::test]
async fn an_availability_read_returns_at_most_its_cap_and_says_so() {
    let kit = Kit::new().await;
    let t = kit.tenant("slots@example.test", "UTC").await;
    let w = format!("/v1/workspaces/{}", t.workspace);
    let b = kit
        .put(
            &t.session,
            &format!("{w}/business"),
            json!({
        "name": "Studio", "time_zone": "UTC", "currency": "EUR", "slot_step_minutes": 5,
        "min_notice_minutes": 0, "max_advance_days": 365 }),
        )
        .await;
    assert_eq!(b.status, StatusCode::OK, "{}", b.body);
    // 2 staff × 31 days × 08:00–18:00 every 5 minutes (30 + 15 after): far over 5,000.
    let (date, _) = ahead(2, "00:00");
    let a = kit
        .get(
            &t.session,
            &format!("{w}/availability?service={}&from={date}&days=31", t.service),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["slots"].as_array().unwrap().len(), 5_000);
    assert_eq!(a.body["truncated"], true);
    kit.db.cleanup().await;
}

#[tokio::test]
async fn a_booking_list_returns_at_most_its_cap() {
    let kit = Kit::new().await;
    let t = kit.tenant("list@example.test", "UTC").await;
    let staff = t.staff[0];
    let service = t.service;
    fill(&kit, &t, &format!("INSERT INTO bookings (business_id, id, staff_id, service_id, status, starts_at, ends_at,
        buffer_before_minutes, buffer_after_minutes, blocked, hold_expires_at, customer_name, idempotency_key,
        created_at, updated_at)
        SELECT '$W', gen_random_uuid(), '{staff}', '{service}', 'held', t, t, 0, 0, tstzrange(t, t + interval '1 hour'),
               now() + interval '10 minutes', 'C', 'k' || g, now(), now()
          FROM generate_series(1, 501) g, LATERAL (SELECT now() + g * interval '1 hour' AS t) x")).await;
    let w = format!("/v1/workspaces/{}", t.workspace);
    let list = kit
        .get(
            &t.session,
            &format!("{w}/bookings?from=2000-01-01T00:00:00Z&to=2200-01-01T00:00:00Z"),
        )
        .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    assert_eq!(list.body["bookings"].as_array().unwrap().len(), 500);
    kit.db.cleanup().await;
}
