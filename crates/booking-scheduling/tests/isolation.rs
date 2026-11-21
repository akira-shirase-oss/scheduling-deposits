//! Tenant isolation: tenant A, with every credential it can hold, can never read or write
//! tenant B's records — not through B's workspace path, and not by naming B's ids under its
//! own. Every attempt is answered as if the record did not exist, and B is unchanged.
#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::many_single_char_names,
    reason = "tests"
)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Auth, Kit, ahead};
use serde_json::{Value, json};
use uuid::Uuid;

/// Everything tenant B has, as B reads it.
async fn snapshot(kit: &Kit, session: &str, w: Uuid, booking: Uuid) -> Vec<Value> {
    let paths = [
        format!("/v1/workspaces/{w}/business"),
        format!("/v1/workspaces/{w}/services"),
        format!("/v1/workspaces/{w}/staff"),
        format!("/v1/workspaces/{w}/exceptions"),
        format!("/v1/workspaces/{w}/bookings/{booking}"),
    ];
    let mut out = Vec::new();
    for p in &paths {
        let answer = kit.get(session, p).await;
        assert_eq!(answer.status, StatusCode::OK, "{p}: {}", answer.body);
        out.push(answer.body);
    }
    out
}

#[tokio::test]
async fn one_tenant_never_reads_or_writes_anothers_records() {
    let kit = Kit::new().await;
    let a = kit.tenant("a@example.test", "Europe/London").await;
    let b = kit.tenant("b@example.test", "America/New_York").await;
    let (date, start) = ahead(7, "15:00");
    let b_booking = kit.hold(&b, b.staff[0], &start, "b-1").await;
    assert_eq!(b_booking.status, StatusCode::CREATED, "{}", b_booking.body);
    let b_booking: Uuid = b_booking.body["id"].as_str().unwrap().parse().unwrap();
    let b_exception = Uuid::now_v7();
    let e = kit
        .put(&b.session, &format!("/v1/workspaces/{}/exceptions/{b_exception}", b.workspace), json!({
            "first_date": date, "last_date": date, "kind": "closed", "start": "08:00", "end": "09:00"
        }))
        .await;
    assert_eq!(e.status, StatusCode::CREATED, "{}", e.body);
    let (_, a_token) = kit.token(&a.session, a.workspace).await;

    let before = snapshot(&kit, &b.session, b.workspace, b_booking).await;

    let bw = format!("/v1/workspaces/{}", b.workspace);
    let aw = format!("/v1/workspaces/{}", a.workspace);
    let service_body = json!({ "name": "Stolen", "duration_minutes": 30, "price_minor": 1, "deposit": { "kind": "none" } });
    let hold_b = json!({ "service_id": b.service, "staff_id": b.staff[0], "starts_at": ahead(7, "16:00").1,
                         "customer_name": "Eve", "idempotency_key": "eve" });
    // 1. B's workspace path, every route, with A's session and A's token.
    let through_b: Vec<(Method, String, Option<Value>)> = vec![
        (Method::GET, format!("{bw}/business"), None),
        (
            Method::PUT,
            format!("{bw}/business"),
            Some(json!({ "name": "x", "time_zone": "UTC", "currency": "EUR" })),
        ),
        (Method::GET, format!("{bw}/services"), None),
        (Method::GET, format!("{bw}/services/{}", b.service), None),
        (
            Method::PUT,
            format!("{bw}/services/{}", b.service),
            Some(service_body.clone()),
        ),
        (Method::DELETE, format!("{bw}/services/{}", b.service), None),
        (Method::GET, format!("{bw}/staff"), None),
        (Method::GET, format!("{bw}/staff/{}", b.staff[0]), None),
        (
            Method::PUT,
            format!("{bw}/staff/{}", b.staff[0]),
            Some(json!({ "name": "x" })),
        ),
        (Method::DELETE, format!("{bw}/staff/{}", b.staff[1]), None),
        (
            Method::PUT,
            format!("{bw}/staff/{}/services", b.staff[0]),
            Some(json!({ "service_ids": [] })),
        ),
        (
            Method::PUT,
            format!("{bw}/staff/{}/hours", b.staff[0]),
            Some(json!({ "hours": [] })),
        ),
        (Method::GET, format!("{bw}/exceptions"), None),
        (
            Method::PUT,
            format!("{bw}/exceptions/{b_exception}"),
            Some(json!({ "first_date": date, "last_date": date, "kind": "closed" })),
        ),
        (
            Method::DELETE,
            format!("{bw}/exceptions/{b_exception}"),
            None,
        ),
        (
            Method::GET,
            format!("{bw}/availability?service={}&from={date}", b.service),
            None,
        ),
        (Method::POST, format!("{bw}/holds"), Some(hold_b.clone())),
        (
            Method::GET,
            format!("{bw}/bookings?from=2000-01-01T00:00:00Z&to=2200-01-01T00:00:00Z"),
            None,
        ),
        (Method::GET, format!("{bw}/bookings/{b_booking}"), None),
        (
            Method::POST,
            format!("{bw}/bookings/{b_booking}/confirm"),
            None,
        ),
        (
            Method::POST,
            format!("{bw}/bookings/{b_booking}/cancel"),
            None,
        ),
    ];
    for (method, path, body) in &through_b {
        for auth in [Auth::Session(&a.session), Auth::Token(&a_token)] {
            let answer = kit
                .call("203.0.113.9:1000", method.clone(), path, auth, body.clone())
                .await;
            assert_eq!(
                answer.status,
                StatusCode::NOT_FOUND,
                "{method} {path}: {}",
                answer.body
            );
        }
    }
    // 2. A's own path, naming B's ids: absent for reads, refused or A-local for writes.
    let reads = [
        format!("{aw}/services/{}", b.service),
        format!("{aw}/staff/{}", b.staff[0]),
        format!("{aw}/bookings/{b_booking}"),
        format!("{aw}/availability?service={}&from={date}", b.service),
        format!(
            "{aw}/availability?service={}&staff={}&from={date}",
            a.service, b.staff[0]
        ),
    ];
    for path in &reads {
        let answer = kit.get(&a.session, path).await;
        let empty = answer.status == StatusCode::OK && answer.body["starts"] == json!([]);
        assert!(
            answer.status == StatusCode::NOT_FOUND || empty,
            "{path}: {}",
            answer.body
        );
    }
    for path in [
        format!("{aw}/bookings/{b_booking}/confirm"),
        format!("{aw}/bookings/{b_booking}/cancel"),
    ] {
        assert_eq!(
            kit.post(&a.session, &path, None).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    for path in [
        format!("{aw}/services/{}", b.service),
        format!("{aw}/staff/{}", b.staff[1]),
        format!("{aw}/exceptions/{b_exception}"),
    ] {
        let answer = kit
            .call(
                "203.0.113.9:1000",
                Method::DELETE,
                &path,
                Auth::Session(&a.session),
                None,
            )
            .await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{path}");
    }
    let assign = kit
        .put(
            &a.session,
            &format!("{aw}/staff/{}/services", a.staff[0]),
            json!({ "service_ids": [b.service] }),
        )
        .await;
    assert_eq!(assign.code(), Some("unknown_service"), "{}", assign.body);
    let hours = kit
        .put(
            &a.session,
            &format!("{aw}/staff/{}/hours", b.staff[0]),
            json!({ "hours": [] }),
        )
        .await;
    assert_eq!(hours.status, StatusCode::NOT_FOUND, "{}", hours.body);
    let assign_b = kit
        .put(
            &a.session,
            &format!("{aw}/staff/{}/services", b.staff[0]),
            json!({ "service_ids": [] }),
        )
        .await;
    assert_eq!(assign_b.status, StatusCode::NOT_FOUND, "{}", assign_b.body);
    let exc = kit
        .put(
            &a.session,
            &format!("{aw}/exceptions/{}", Uuid::now_v7()),
            json!({
            "staff_id": b.staff[0], "first_date": date, "last_date": date, "kind": "closed" }),
        )
        .await;
    assert_eq!(exc.code(), Some("unknown_staff"), "{}", exc.body);
    let hold = kit
        .post(&a.session, &format!("{aw}/holds"), Some(hold_b))
        .await;
    assert_eq!(hold.status, StatusCode::NOT_FOUND, "{}", hold.body);
    let mixed = kit
        .post(
            &a.session,
            &format!("{aw}/holds"),
            Some(json!({
        "service_id": a.service, "staff_id": b.staff[0], "starts_at": ahead(7, "16:00").1,
        "customer_name": "Eve", "idempotency_key": "eve-2" })),
        )
        .await;
    assert_eq!(
        mixed.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        mixed.body
    );
    // Same ids, A's namespace: a PUT with B's service id makes A's own row, not B's.
    let shadow = kit
        .put(
            &a.session,
            &format!("{aw}/services/{}", b.service),
            service_body,
        )
        .await;
    assert_eq!(shadow.status, StatusCode::CREATED, "{}", shadow.body);
    // A's lists never include B's rows.
    let a_lists = kit
        .get(
            &a.session,
            &format!("{aw}/bookings?from=2000-01-01T00:00:00Z&to=2200-01-01T00:00:00Z"),
        )
        .await;
    assert_eq!(a_lists.body["bookings"], json!([]));
    let a_exc = kit.get(&a.session, &format!("{aw}/exceptions")).await;
    assert_eq!(a_exc.body["exceptions"], json!([]));

    assert_eq!(
        snapshot(&kit, &b.session, b.workspace, b_booking).await,
        before,
        "tenant B changed"
    );
    kit.db.cleanup().await;
}

#[tokio::test]
async fn roles_and_tokens_are_held_to_the_policy() {
    let kit = Kit::new().await;
    let a = kit.tenant("owner@example.test", "UTC").await;
    let aw = format!("/v1/workspaces/{}", a.workspace);
    let (_, token) = kit.token(&a.session, a.workspace).await;
    // A token reads, never writes.
    let read = kit
        .call(
            "203.0.113.9:1000",
            Method::GET,
            &format!("{aw}/services"),
            Auth::Token(&token),
            None,
        )
        .await;
    assert_eq!(read.status, StatusCode::OK);
    let write = kit
        .call(
            "203.0.113.9:1000",
            Method::PUT,
            &format!("{aw}/staff/{}", Uuid::now_v7()),
            Auth::Token(&token),
            Some(json!({ "name": "x" })),
        )
        .await;
    assert_eq!(write.code(), Some("session_required"), "{}", write.body);
    let hold = kit
        .call(
            "203.0.113.9:1000",
            Method::POST,
            &format!("{aw}/holds"),
            Auth::Token(&token),
            Some(json!({
            "service_id": a.service, "staff_id": a.staff[0], "starts_at": ahead(7, "10:00").1,
            "customer_name": "x", "idempotency_key": "t" })),
        )
        .await;
    assert_eq!(hold.code(), Some("session_required"), "{}", hold.body);
    // A member books but does not configure.
    kit.verified_user("member@example.test").await;
    let added = kit
        .post(
            &a.session,
            &format!("{aw}/members"),
            Some(json!({ "email": "member@example.test", "role": "member" })),
        )
        .await;
    assert_eq!(added.status, StatusCode::CREATED, "{}", added.body);
    let member = kit.session("203.0.113.9:1000", "member@example.test").await;
    let configure = kit
        .put(
            &member,
            &format!("{aw}/staff/{}", Uuid::now_v7()),
            json!({ "name": "x" }),
        )
        .await;
    assert_eq!(
        configure.status,
        StatusCode::FORBIDDEN,
        "{}",
        configure.body
    );
    let book = kit
        .post(
            &member,
            &format!("{aw}/holds"),
            Some(json!({
            "service_id": a.service, "staff_id": a.staff[0], "starts_at": ahead(7, "10:00").1,
            "customer_name": "x", "idempotency_key": "m" })),
        )
        .await;
    assert_eq!(book.status, StatusCode::CREATED, "{}", book.body);
    // Nobody.
    let anon = kit
        .call(
            "203.0.113.9:1000",
            Method::GET,
            &format!("{aw}/services"),
            Auth::None,
            None,
        )
        .await;
    assert_eq!(anon.status, StatusCode::UNAUTHORIZED);
    kit.db.cleanup().await;
}
