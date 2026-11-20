//! Onboarding through the API: the business, services with deposits, staff with services
//! and weekly hours, exceptions, and availability across a DST change — and the refusals.
#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::many_single_char_names,
    reason = "tests"
)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Auth, Kit};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn a_business_is_set_up_and_offers_slots_across_a_dst_change() {
    let kit = Kit::new().await;
    kit.verified_user("o@example.test").await;
    let s = kit.session("203.0.113.1:1000", "o@example.test").await;
    let w = kit.workspace(&s, "Salon").await;
    let w = format!("/v1/workspaces/{w}");
    // Nothing before the business.
    assert_eq!(
        kit.get(&s, &format!("{w}/business")).await.status,
        StatusCode::NOT_FOUND
    );
    let early = kit
        .put(
            &s,
            &format!("{w}/staff/{}", Uuid::now_v7()),
            json!({ "name": "x" }),
        )
        .await;
    assert_eq!(early.code(), Some("business_required"), "{}", early.body);
    // Refusals name the rule.
    for (body, code) in [
        (
            json!({ "name": "S", "time_zone": "Mars/Olympus", "currency": "EUR" }),
            "invalid_time_zone",
        ),
        (
            json!({ "name": "S", "time_zone": "../../etc/passwd", "currency": "EUR" }),
            "invalid_time_zone",
        ),
        (
            json!({ "name": "S", "time_zone": "Europe/London", "currency": "eur" }),
            "invalid_currency",
        ),
        (
            json!({ "name": "S", "time_zone": "Europe/London", "currency": "EUR", "slot_step_minutes": 7 }),
            "invalid_step",
        ),
        (
            json!({ "name": "", "time_zone": "Europe/London", "currency": "EUR" }),
            "invalid_name",
        ),
    ] {
        assert_eq!(
            kit.put(&s, &format!("{w}/business"), body).await.code(),
            Some(code)
        );
    }
    let unknown = kit
        .put(
            &s,
            &format!("{w}/business"),
            json!({ "name": "S", "time_zone": "UTC", "currency": "EUR", "extra": 1 }),
        )
        .await;
    assert_eq!(
        unknown.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        unknown.body
    );
    let b = kit
        .put(
            &s,
            &format!("{w}/business"),
            json!({
        "name": "Salon", "time_zone": "Europe/London", "currency": "GBP", "slot_step_minutes": 30,
        "min_notice_minutes": 0, "max_advance_days": 365, "hold_minutes": 5 }),
        )
        .await;
    assert_eq!(b.status, StatusCode::CREATED, "{}", b.body);
    let again = kit
        .put(
            &s,
            &format!("{w}/business"),
            json!({
        "name": "Salon", "time_zone": "Europe/London", "currency": "GBP", "slot_step_minutes": 30,
        "min_notice_minutes": 0, "max_advance_days": 365, "hold_minutes": 5 }),
        )
        .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);

    let service = Uuid::now_v7();
    for (deposit, code) in [
        (
            json!({ "kind": "fixed", "amount_minor": 5000 }),
            "invalid_deposit",
        ),
        (
            json!({ "kind": "percent", "percent": 0 }),
            "invalid_deposit",
        ),
    ] {
        let r = kit
            .put(
                &s,
                &format!("{w}/services/{service}"),
                json!({
            "name": "Colour", "duration_minutes": 60, "price_minor": 4000, "deposit": deposit }),
            )
            .await;
        assert_eq!(r.code(), Some(code), "{}", r.body);
    }
    let bad = kit.put(&s, &format!("{w}/services/{service}"), json!({
        "name": "Colour", "duration_minutes": 61, "price_minor": 4000, "deposit": { "kind": "none" } })).await;
    assert_eq!(bad.code(), Some("invalid_duration"));
    let svc = kit.put(&s, &format!("{w}/services/{service}"), json!({
        "name": "Colour", "duration_minutes": 60, "buffer_before_minutes": 0, "buffer_after_minutes": 0,
        "price_minor": 4001, "deposit": { "kind": "percent", "percent": 25 } })).await;
    assert_eq!(svc.status, StatusCode::CREATED, "{}", svc.body);
    assert_eq!(svc.body["deposit_minor"], 1000); // 1000.25 rounds half up to 1000

    let staff = Uuid::now_v7();
    assert_eq!(
        kit.put(&s, &format!("{w}/staff/{staff}"), json!({ "name": "Kim" }))
            .await
            .status,
        StatusCode::CREATED
    );
    let assigned = kit
        .put(
            &s,
            &format!("{w}/staff/{staff}/services"),
            json!({ "service_ids": [service, service] }),
        )
        .await;
    assert_eq!(assigned.body["service_ids"], json!([service]));
    let overlap = kit.put(&s, &format!("{w}/staff/{staff}/hours"), json!({ "hours": [
        { "weekday": 7, "start": "00:00", "end": "04:00" }, { "weekday": 7, "start": "03:00", "end": "05:00" }] })).await;
    assert_eq!(overlap.code(), Some("overlapping_hours"));
    // Sunday 00:00–04:00 local: on 2027-03-28 (spring forward at 01:00) only 3 real hours.
    let hours = kit
        .put(
            &s,
            &format!("{w}/staff/{staff}/hours"),
            json!({ "hours": [
        { "weekday": 7, "start": "00:00", "end": "04:00" }] }),
        )
        .await;
    assert_eq!(hours.status, StatusCode::OK, "{}", hours.body);
    assert_eq!(
        hours.body["hours"],
        json!([{ "weekday": 7, "start": "00:00", "end": "04:00" }])
    );
    let slots = |date: &'static str| {
        let path = format!("{w}/availability?service={service}&from={date}");
        let kit = &kit;
        let s = s.clone();
        async move {
            let a = kit.get(&s, &path).await;
            assert_eq!(a.status, StatusCode::OK, "{}", a.body);
            a.body["starts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x["at"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        slots("2027-03-28").await,
        [
            "2027-03-28T00:00:00Z",
            "2027-03-28T00:30:00Z",
            "2027-03-28T01:00:00Z",
            "2027-03-28T01:30:00Z",
            "2027-03-28T02:00:00Z"
        ]
    );
    // Fall back on 2027-10-31: 01:00–02:00 happens twice, so five real hours.
    assert_eq!(slots("2027-10-31").await.len(), 9);
    // A closed exception removes the date; deleting it brings it back.
    let exc = Uuid::now_v7();
    let e = kit
        .put(
            &s,
            &format!("{w}/exceptions/{exc}"),
            json!({ "first_date": "2027-10-31", "last_date": "2027-10-31", "kind": "closed" }),
        )
        .await;
    assert_eq!(e.status, StatusCode::CREATED, "{}", e.body);
    assert_eq!(slots("2027-10-31").await, Vec::<String>::new());
    let listed = kit
        .get(&s, &format!("{w}/exceptions?from=2027-10-01&to=2027-11-01"))
        .await;
    assert_eq!(listed.body["exceptions"].as_array().unwrap().len(), 1);
    let del = kit
        .call(
            "203.0.113.1:1000",
            Method::DELETE,
            &format!("{w}/exceptions/{exc}"),
            Auth::Session(&s),
            None,
        )
        .await;
    assert_eq!(del.status, StatusCode::NO_CONTENT);
    assert_eq!(slots("2027-10-31").await.len(), 9);
    let wide = kit
        .get(
            &s,
            &format!("{w}/availability?service={service}&from=2027-10-31&days=32"),
        )
        .await;
    assert_eq!(wide.code(), Some("invalid_days"));
    // An unused staff member can be deleted.
    let del = kit
        .call(
            "203.0.113.1:1000",
            Method::DELETE,
            &format!("{w}/staff/{staff}"),
            Auth::Session(&s),
            None,
        )
        .await;
    assert_eq!(del.status, StatusCode::NO_CONTENT);
    kit.db.cleanup().await;
}
