//! The harness: a test database, the kit and the product's routes behind the real stack,
//! and helpers that onboard a business through the API.
#![allow(dead_code, reason = "each test binary uses part of the harness")]
#![allow(
    unreachable_pub,
    reason = "a test module shared by several test binaries"
)]
#![allow(
    clippy::unwrap_used,
    clippy::many_single_char_names,
    reason = "test harness"
)]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use booking_accounts::mail::SmtpRelay;
use booking_accounts::testing::SmtpSink;
use booking_accounts::{Accounts, Settings, store, tokens};
use booking_db::testing::TestDb;
use booking_http::stack::{self, ErrorWindow, StackLimits};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

pub const PASSWORD: &str = "correct horse battery staple";

pub struct Kit {
    pub db: TestDb,
    pub app: Router,
    pub sink: SmtpSink,
    pub accounts: Accounts,
}

/// What a request authenticates with.
#[derive(Clone, Copy)]
pub enum Auth<'a> {
    None,
    Session(&'a str),
    Token(&'a str),
}

pub struct Answer {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
}

impl Answer {
    pub fn code(&self) -> Option<&str> {
        self.body["error"]["code"].as_str()
    }
}

impl Kit {
    pub async fn new() -> Self {
        Self::with(|_| {}).await
    }

    pub async fn with(tune: impl FnOnce(&mut Settings)) -> Self {
        let db = TestDb::new().await;
        let sink = SmtpSink::start("127.0.0.1:0", None).await.unwrap();
        let mut settings = Settings {
            public_url: "https://app.example.test".to_owned(),
            cookie_secure: false,
            ..Settings::default()
        };
        tune(&mut settings);
        let accounts = Accounts::new(
            db.pool.clone(),
            settings,
            Arc::new(SmtpRelay::new(sink.addr).unwrap()),
        )
        .await
        .unwrap();
        let product = booking_scheduling::router(booking_scheduling::Scheduling::new(
            accounts.clone(),
            db.pool.clone(),
        ));
        let app = Router::new().nest(
            "/v1",
            booking_accounts::router(accounts.clone()).merge(product),
        );
        let app = stack::apply(
            app,
            StackLimits::default(),
            Arc::new(ErrorWindow::default()),
        );
        Self {
            db,
            app,
            sink,
            accounts,
        }
    }

    pub async fn call(
        &self,
        peer: &str,
        method: Method,
        path: &str,
        auth: Auth<'_>,
        body: Option<Value>,
    ) -> Answer {
        let mut request = Request::builder().method(method).uri(path);
        match auth {
            Auth::None => {}
            Auth::Session(id) => {
                request = request.header(header::COOKIE, format!("booking_session={id}"));
            }
            Auth::Token(token) => {
                request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
        }
        let body = match body {
            Some(v) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        let mut request = request.body(body).unwrap();
        let peer: SocketAddr = peer.parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Answer {
            status,
            headers,
            body,
        }
    }

    pub async fn invite(&self) -> String {
        let code = tokens::new_invite().unwrap();
        store::create_invites(
            &self.db.pool,
            &[tokens::digest(&code)],
            std::time::Duration::from_secs(3600),
        )
        .await
        .unwrap();
        code
    }

    /// Signs up through the API and verifies through the link the sink received.
    pub async fn verified_user(&self, email: &str) -> Uuid {
        let invite = self.invite().await;
        let up = self
            .call(
                "203.0.113.1:1000",
                Method::POST,
                "/v1/signup",
                Auth::None,
                Some(json!({ "invite": invite, "email": email, "password": PASSWORD })),
            )
            .await;
        assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
        let token = self.sink.latest_to(email).await.unwrap().token().unwrap();
        let verified = self
            .call(
                "203.0.113.1:1000",
                Method::POST,
                "/v1/verify",
                Auth::None,
                Some(json!({ "token": token })),
            )
            .await;
        assert_eq!(verified.status, StatusCode::NO_CONTENT, "{}", verified.body);
        up.body["user_id"].as_str().unwrap().parse().unwrap()
    }

    /// Signs in from `peer`; the session id.
    pub async fn session(&self, peer: &str, email: &str) -> String {
        let answer = self
            .call(
                peer,
                Method::POST,
                "/v1/session",
                Auth::None,
                Some(json!({ "email": email, "password": PASSWORD })),
            )
            .await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
        session_from(&answer).unwrap()
    }

    pub async fn workspace(&self, session: &str, name: &str) -> Uuid {
        let answer = self
            .call(
                "203.0.113.1:1000",
                Method::POST,
                "/v1/workspaces",
                Auth::Session(session),
                Some(json!({ "name": name })),
            )
            .await;
        assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
        answer.body["id"].as_str().unwrap().parse().unwrap()
    }

    pub async fn token(&self, session: &str, workspace: Uuid) -> (Uuid, String) {
        let answer = self
            .call(
                "203.0.113.1:1000",
                Method::POST,
                &format!("/v1/workspaces/{workspace}/tokens"),
                Auth::Session(session),
                Some(json!({ "name": "ci" })),
            )
            .await;
        assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
        (
            answer.body["id"].as_str().unwrap().parse().unwrap(),
            answer.body["token"].as_str().unwrap().to_owned(),
        )
    }
}

pub fn session_from(answer: &Answer) -> Option<String> {
    let cookie = answer.headers.get(header::SET_COOKIE)?.to_str().ok()?;
    let value = cookie.strip_prefix("booking_session=")?;
    Some(value.split(';').next()?.to_owned())
}

/// A signed-in owner with a set-up business: one service (30 min, 15 after), two staff who
/// perform it, open 08:00–18:00 every day, in `zone`.
pub struct Tenant {
    pub session: String,
    pub workspace: Uuid,
    pub service: Uuid,
    pub staff: [Uuid; 2],
}

impl Kit {
    pub async fn put(&self, session: &str, path: &str, body: Value) -> Answer {
        self.call(
            "203.0.113.1:1000",
            Method::PUT,
            path,
            Auth::Session(session),
            Some(body),
        )
        .await
    }

    pub async fn get(&self, session: &str, path: &str) -> Answer {
        self.call(
            "203.0.113.1:1000",
            Method::GET,
            path,
            Auth::Session(session),
            None,
        )
        .await
    }

    pub async fn post(&self, session: &str, path: &str, body: Option<Value>) -> Answer {
        self.call(
            "203.0.113.1:1000",
            Method::POST,
            path,
            Auth::Session(session),
            body,
        )
        .await
    }

    pub async fn tenant(&self, email: &str, zone: &str) -> Tenant {
        self.verified_user(email).await;
        let session = self.session("203.0.113.1:1000", email).await;
        let workspace = self.workspace(&session, email).await;
        let w = format!("/v1/workspaces/{workspace}");
        let b = self
            .put(&session, &format!("{w}/business"), json!({
                "name": "Studio", "time_zone": zone, "currency": "EUR",
                "slot_step_minutes": 15, "min_notice_minutes": 0, "max_advance_days": 365, "hold_minutes": 10
            }))
            .await;
        assert_eq!(b.status, StatusCode::CREATED, "{}", b.body);
        let service = Uuid::now_v7();
        let s = self
            .put(
                &session,
                &format!("{w}/services/{service}"),
                json!({
                    "name": "Cut", "duration_minutes": 30, "buffer_before_minutes": 0,
                    "buffer_after_minutes": 15, "price_minor": 4000,
                    "deposit": { "kind": "percent", "percent": 25 }
                }),
            )
            .await;
        assert_eq!(s.status, StatusCode::CREATED, "{}", s.body);
        let staff = [Uuid::now_v7(), Uuid::now_v7()];
        let hours: Vec<Value> = (1..=7)
            .map(|d| json!({ "weekday": d, "start": "08:00", "end": "18:00" }))
            .collect();
        for (i, id) in staff.iter().enumerate() {
            let p = self
                .put(
                    &session,
                    &format!("{w}/staff/{id}"),
                    json!({ "name": format!("Staff {i}") }),
                )
                .await;
            assert_eq!(p.status, StatusCode::CREATED, "{}", p.body);
            let a = self
                .put(
                    &session,
                    &format!("{w}/staff/{id}/services"),
                    json!({ "service_ids": [service] }),
                )
                .await;
            assert_eq!(a.status, StatusCode::OK, "{}", a.body);
            let h = self
                .put(
                    &session,
                    &format!("{w}/staff/{id}/hours"),
                    json!({ "hours": hours }),
                )
                .await;
            assert_eq!(h.status, StatusCode::OK, "{}", h.body);
        }
        Tenant {
            session,
            workspace,
            service,
            staff,
        }
    }

    pub async fn hold(&self, t: &Tenant, staff: Uuid, starts_at: &str, key: &str) -> Answer {
        self.post(
            &t.session,
            &format!("/v1/workspaces/{}/holds", t.workspace),
            Some(json!({
                "service_id": t.service, "staff_id": staff, "starts_at": starts_at,
                "customer_name": "Ada", "idempotency_key": key
            })),
        )
        .await
    }
}

/// A date a week ahead (UTC) and an instant on it, `HH:MM` UTC.
pub fn ahead(days: i64, hhmm: &str) -> (String, String) {
    let date = jiff::tz::TimeZone::UTC
        .to_datetime(jiff::Timestamp::now())
        .date()
        .checked_add(jiff::Span::new().days(days))
        .unwrap();
    (date.to_string(), format!("{date}T{hhmm}:00Z"))
}
