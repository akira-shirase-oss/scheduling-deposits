//! A busy credential is not a write per request: a fresh session or token is read, and its
//! row is rewritten at most once a minute. Every write to one row serializes on its lock and
//! a WAL flush, which caps a single token or session at the disk's commit rate.
#![expect(clippy::unwrap_used, reason = "test")]

mod common;

use axum::http::{Method, StatusCode};
use booking_db::testing::Role;
use common::{Auth, Kit};
use sqlx::PgPool;

const PEER: &str = "203.0.113.9:4000";

/// The row version: it changes whenever the row is written, even to the same values.
async fn version(db: &PgPool, table: &str) -> String {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT xmin::text FROM {table}"
    )))
    .fetch_one(db)
    .await
    .unwrap()
}

#[tokio::test]
async fn fresh_credentials_are_read_not_rewritten_and_stale_ones_are_touched() {
    let kit = Kit::new().await;
    kit.verified_user("alice@example.test").await;
    let alice = kit.session(PEER, "alice@example.test").await;
    let ws = kit.workspace(&alice, "A").await;
    let (_, token) = kit.token(&alice, ws).await;
    let admin = kit.db.connect_as(Role::Admin).await;
    let path = format!("/v1/workspaces/{ws}");

    for (table, column, auth) in [
        ("api_tokens", "last_used_at", Auth::Token(&token)),
        ("sessions", "last_seen_at", Auth::Session(&alice)),
    ] {
        // Touched once (a token's first use), then fresh: reads leave the row alone.
        let first = kit.call(PEER, Method::GET, &path, auth, None).await;
        assert_eq!(first.status, StatusCode::OK, "{}", first.body);
        let before = version(&admin, table).await;
        for _ in 0..5 {
            let read = kit.call(PEER, Method::GET, &path, auth, None).await;
            assert_eq!(read.status, StatusCode::OK, "{}", read.body);
        }
        assert_eq!(
            version(&admin, table).await,
            before,
            "{table}: a fresh read rewrote the row"
        );

        // Stale by more than a minute: the next use touches it, once.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET {column} = now() - interval '2 minutes'"
        )))
        .execute(&admin)
        .await
        .unwrap();
        let stale = version(&admin, table).await;
        let read = kit.call(PEER, Method::GET, &path, auth, None).await;
        assert_eq!(read.status, StatusCode::OK, "{}", read.body);
        let touched = version(&admin, table).await;
        assert_ne!(
            touched, stale,
            "{table}: a stale credential was not touched"
        );
        let fresh: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT {column} > now() - interval '10 seconds' FROM {table}"
        )))
        .fetch_one(&admin)
        .await
        .unwrap();
        assert!(fresh, "{table}: the touch did not move {column}");
        let again = kit.call(PEER, Method::GET, &path, auth, None).await;
        assert_eq!(again.status, StatusCode::OK, "{}", again.body);
        assert_eq!(
            version(&admin, table).await,
            touched,
            "{table}: touched twice"
        );
    }
    admin.close().await;
    kit.db.cleanup().await;
}
