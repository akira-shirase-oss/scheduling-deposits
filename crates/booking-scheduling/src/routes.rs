//! The routes, all under `/v1/workspaces/{workspace}`; `docs/SCHEDULING.md` is the API.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use booking_accounts::domain::Action;
use jiff::tz::TimeZone;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::Scheduling;
use crate::domain::{
    self, BusinessInput, ExceptionInput, HoldInput, ServiceInput, StaffInput, WeeklyRange,
};
use crate::error::SchedulingError;
use crate::store;

type Result<T> = std::result::Result<T, SchedulingError>;

pub fn router(state: Scheduling) -> Router {
    Router::new()
        .route(
            "/workspaces/{workspace}/business",
            get(read_business).put(put_business),
        )
        .route("/workspaces/{workspace}/services", get(list_services))
        .route(
            "/workspaces/{workspace}/services/{id}",
            get(read_service).put(put_service).delete(delete_service),
        )
        .route("/workspaces/{workspace}/staff", get(list_staff))
        .route(
            "/workspaces/{workspace}/staff/{id}",
            get(read_staff).put(put_staff).delete(delete_staff),
        )
        .route(
            "/workspaces/{workspace}/staff/{id}/services",
            put(put_staff_services),
        )
        .route("/workspaces/{workspace}/staff/{id}/hours", put(put_hours))
        .route("/workspaces/{workspace}/exceptions", get(list_exceptions))
        .route(
            "/workspaces/{workspace}/exceptions/{id}",
            put(put_exception).delete(delete_exception),
        )
        .route("/workspaces/{workspace}/availability", get(availability))
        .route("/workspaces/{workspace}/holds", post(place_hold))
        .route("/workspaces/{workspace}/bookings", get(list_bookings))
        .route("/workspaces/{workspace}/bookings/{id}", get(read_booking))
        .route(
            "/workspaces/{workspace}/bookings/{id}/confirm",
            post(confirm),
        )
        .route("/workspaces/{workspace}/bookings/{id}/cancel", post(cancel))
        .with_state(state)
}

/// Authorizes, and answers with the business id every query is keyed by.
async fn authorize(
    s: &Scheduling,
    headers: &HeaderMap,
    workspace: Uuid,
    action: Action,
) -> Result<Uuid> {
    Ok(s.accounts
        .access(headers, workspace, action)
        .await?
        .workspace)
}

fn invalid((code, message): domain::Invalid) -> SchedulingError {
    SchedulingError::Invalid(code, message)
}

fn created_or_ok(created: bool, body: serde_json::Value) -> Response {
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    (status, Json(body)).into_response()
}

/// Resolves a zone against the zone database (a name, never a path: shape-checked first).
pub fn zone(name: &str) -> Result<TimeZone> {
    if !domain::zone_name_shaped(name) {
        return Err(invalid((
            "invalid_time_zone",
            "the time zone is not an IANA name",
        )));
    }
    TimeZone::get(name).map_err(|_| {
        invalid((
            "invalid_time_zone",
            "the time zone is not in the zone database",
        ))
    })
}

async fn read_business(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path(w): Path<Uuid>,
) -> Result<Json<store::BusinessRow>> {
    let b = authorize(&s, &headers, w, Action::Read).await?;
    Ok(Json(
        store::business(&s.db, b)
            .await?
            .ok_or(SchedulingError::NotFound)?,
    ))
}

async fn put_business(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path(w): Path<Uuid>,
    Json(body): Json<BusinessInput>,
) -> Result<Response> {
    let b = authorize(&s, &headers, w, Action::Configure).await?;
    let settings = body.validate().map_err(invalid)?;
    zone(&settings.time_zone)?;
    let (row, created) = store::put_business(&s.db, b, &settings).await?;
    Ok(created_or_ok(created, json!(row)))
}

async fn list_services(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path(w): Path<Uuid>,
) -> Result<Json<serde_json::Value>> {
    let b = authorize(&s, &headers, w, Action::Read).await?;
    Ok(Json(
        json!({ "services": store::services(&s.db, b).await? }),
    ))
}

async fn read_service(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path((w, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<store::ServiceRow>> {
    let b = authorize(&s, &headers, w, Action::Read).await?;
    Ok(Json(
        store::service(&s.db, b, id)
            .await?
            .ok_or(SchedulingError::NotFound)?,
    ))
}

async fn put_service(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path((w, id)): Path<(Uuid, Uuid)>,
    Json(body): Json<ServiceInput>,
) -> Result<Response> {
    let b = authorize(&s, &headers, w, Action::Configure).await?;
    let service = body.validate().map_err(invalid)?;
    let (row, created) = store::put_service(&s.db, b, id, &service).await?;
    Ok(created_or_ok(created, json!(row)))
}

async fn delete_service(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path((w, id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    let b = authorize(&s, &headers, w, Action::Configure).await?;
    if store::delete_service(&s.db, b, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(SchedulingError::NotFound)
    }
}

async fn list_staff(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path(w): Path<Uuid>,
) -> Result<Json<serde_json::Value>> {
    let b = authorize(&s, &headers, w, Action::Read).await?;
    Ok(Json(json!({ "staff": store::staff_list(&s.db, b).await? })))
}

async fn read_staff(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path((w, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<store::StaffRow>> {
    let b = authorize(&s, &headers, w, Action::Read).await?;
    Ok(Json(
        store::staff(&s.db, b, id)
            .await?
            .ok_or(SchedulingError::NotFound)?,
    ))
}

async fn put_staff(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path((w, id)): Path<(Uuid, Uuid)>,
    Json(body): Json<StaffInput>,
) -> Result<Response> {
    let b = authorize(&s, &headers, w, Action::Configure).await?;
    let (name, active) = body.validate().map_err(invalid)?;
    let created = store::put_staff(&s.db, b, id, &name, active).await?;
    let row = store::staff(&s.db, b, id)
        .await?
        .ok_or(SchedulingError::NotFound)?;
    Ok(created_or_ok(created, json!(row)))
}

async fn delete_staff(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path((w, id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    let b = authorize(&s, &headers, w, Action::Configure).await?;
    if store::delete_staff(&s.db, b, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(SchedulingError::NotFound)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StaffServices {
    service_ids: Vec<Uuid>,
}

async fn put_staff_services(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path((w, id)): Path<(Uuid, Uuid)>,
    Json(mut body): Json<StaffServices>,
) -> Result<Json<store::StaffRow>> {
    let b = authorize(&s, &headers, w, Action::Configure).await?;
    body.service_ids.sort_unstable();
    body.service_ids.dedup();
    if body.service_ids.len() > usize::try_from(domain::MAX_SERVICES).unwrap_or(0) {
        return Err(invalid((
            "too_many_services",
            "at most 200 services per staff member",
        )));
    }
    store::set_staff_services(&s.db, b, id, &body.service_ids).await?;
    Ok(Json(
        store::staff(&s.db, b, id)
            .await?
            .ok_or(SchedulingError::NotFound)?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Hours {
    hours: Vec<WeeklyRange>,
}

async fn put_hours(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path((w, id)): Path<(Uuid, Uuid)>,
    Json(body): Json<Hours>,
) -> Result<Json<store::StaffRow>> {
    let b = authorize(&s, &headers, w, Action::Configure).await?;
    let hours = domain::weekly_hours(&body.hours).map_err(invalid)?;
    store::set_hours(&s.db, b, id, &hours).await?;
    Ok(Json(
        store::staff(&s.db, b, id)
            .await?
            .ok_or(SchedulingError::NotFound)?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DateRange {
    from: Option<String>,
    to: Option<String>,
}

fn optional_date(raw: Option<&str>) -> Result<Option<jiff::civil::Date>> {
    raw.map(|d| domain::parse_date(d).ok_or(invalid(("invalid_date", "dates are YYYY-MM-DD"))))
        .transpose()
}

async fn list_exceptions(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path(w): Path<Uuid>,
    Query(q): Query<DateRange>,
) -> Result<Json<serde_json::Value>> {
    let b = authorize(&s, &headers, w, Action::Read).await?;
    let (from, to) = (
        optional_date(q.from.as_deref())?,
        optional_date(q.to.as_deref())?,
    );
    Ok(Json(
        json!({ "exceptions": store::exceptions(&s.db, b, from, to).await? }),
    ))
}

async fn put_exception(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path((w, id)): Path<(Uuid, Uuid)>,
    Json(body): Json<ExceptionInput>,
) -> Result<Response> {
    let b = authorize(&s, &headers, w, Action::Configure).await?;
    let rule = body.validate().map_err(invalid)?;
    let (row, created) = store::put_exception(&s.db, b, id, &rule).await?;
    Ok(created_or_ok(created, json!(row)))
}

async fn delete_exception(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path((w, id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    let b = authorize(&s, &headers, w, Action::Configure).await?;
    if store::delete_exception(&s.db, b, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(SchedulingError::NotFound)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AvailabilityQuery {
    service: Uuid,
    staff: Option<Uuid>,
    from: String,
    #[serde(default = "one")]
    days: u32,
}

const fn one() -> u32 {
    1
}

fn engine_error(e: &availability::Error) -> SchedulingError {
    match e {
        availability::Error::OutOfRange => invalid(("invalid_date", "the dates are out of range")),
        _ => {
            SchedulingError::Internal(anyhow::anyhow!("availability refused validated input: {e}"))
        }
    }
}

fn query_of(inputs: &store::Inputs, from: jiff::civil::Date, days: u32) -> availability::Query {
    let b = &inputs.business.settings;
    availability::Query {
        from,
        days,
        now: inputs.now,
        min_notice_minutes: b.min_notice_minutes,
        max_advance_days: b.max_advance_days,
        step_minutes: b.slot_step_minutes,
    }
}

fn service_of(inputs: &store::Inputs) -> availability::Service {
    availability::Service {
        duration_minutes: inputs.service.duration_minutes,
        buffer_before_minutes: inputs.service.buffer_before_minutes,
        buffer_after_minutes: inputs.service.buffer_after_minutes,
    }
}

async fn availability(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path(w): Path<Uuid>,
    Query(q): Query<AvailabilityQuery>,
) -> Result<Json<serde_json::Value>> {
    let b = authorize(&s, &headers, w, Action::Read).await?;
    let from =
        domain::parse_date(&q.from).ok_or(invalid(("invalid_date", "from is YYYY-MM-DD")))?;
    if !(1..=domain::MAX_READ_DAYS).contains(&q.days) {
        return Err(invalid(("invalid_days", "days is 1 to 31")));
    }
    let inputs = store::inputs(&s.db, b, q.service, q.staff, from, q.days, true).await?;
    let tz = zone(&inputs.business.settings.time_zone)?;
    let mut slots = availability::slots(
        &tz,
        &inputs.staff,
        &inputs.exceptions,
        service_of(&inputs),
        &query_of(&inputs, from, q.days),
    )
    .map_err(|e| engine_error(&e))?;
    let truncated = slots.len() > domain::MAX_SLOTS;
    slots.truncate(domain::MAX_SLOTS);
    let slots: Vec<_> = slots
        .iter()
        .map(|slot| json!({ "start": slot.start.to_string(), "end": slot.end.to_string(), "staff_id": slot.staff }))
        .collect();
    Ok(Json(json!({
        "service_id": q.service,
        "time_zone": inputs.business.settings.time_zone,
        "from": from.to_string(),
        "days": q.days,
        "as_of": inputs.now.to_string(),
        "slots": slots,
        "truncated": truncated,
    })))
}

async fn place_hold(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path(w): Path<Uuid>,
    Json(body): Json<HoldInput>,
) -> Result<Response> {
    let b = authorize(&s, &headers, w, Action::Book).await?;
    let hold = body.validate().map_err(invalid)?;
    let tz = zone(
        &store::business(&s.db, b)
            .await?
            .ok_or(SchedulingError::BusinessRequired)?
            .settings
            .time_zone,
    )?;
    let day = tz.to_datetime(hold.starts_at).date();
    // The hours, notice and advance are checked here; overlap is the constraint's alone, so
    // concurrent requests are decided by the database.
    let inputs = store::inputs(
        &s.db,
        b,
        hold.service_id,
        Some(hold.staff_id),
        day,
        1,
        false,
    )
    .await?;
    let slots = availability::slots(
        &tz,
        &inputs.staff,
        &inputs.exceptions,
        service_of(&inputs),
        &query_of(&inputs, day, 1),
    )
    .map_err(|e| engine_error(&e))?;
    if !slots
        .iter()
        .any(|slot| slot.start == hold.starts_at && slot.staff == hold.staff_id)
    {
        return Err(SchedulingError::SlotUnavailable);
    }
    let (row, created) =
        store::hold(&s.db, b, &hold, inputs.business.settings.hold_minutes).await?;
    Ok(created_or_ok(created, json!(row)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstantRange {
    from: String,
    to: String,
}

async fn list_bookings(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path(w): Path<Uuid>,
    Query(q): Query<InstantRange>,
) -> Result<Json<serde_json::Value>> {
    let b = authorize(&s, &headers, w, Action::Read).await?;
    let bad = || {
        invalid((
            "invalid_range",
            "from and to are RFC 3339 instants, from before to",
        ))
    };
    let from: jiff::Timestamp = q.from.parse().map_err(|_| bad())?;
    let to: jiff::Timestamp = q.to.parse().map_err(|_| bad())?;
    if from >= to {
        return Err(bad());
    }
    Ok(Json(
        json!({ "bookings": store::bookings(&s.db, b, from, to).await? }),
    ))
}

async fn read_booking(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path((w, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<store::BookingRow>> {
    let b = authorize(&s, &headers, w, Action::Read).await?;
    Ok(Json(
        store::booking(&s.db, b, id)
            .await?
            .ok_or(SchedulingError::NotFound)?,
    ))
}

async fn confirm(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path((w, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<store::BookingRow>> {
    let b = authorize(&s, &headers, w, Action::Book).await?;
    Ok(Json(store::transition(&s.db, b, id, "confirmed").await?))
}

async fn cancel(
    State(s): State<Scheduling>,
    headers: HeaderMap,
    Path((w, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<store::BookingRow>> {
    let b = authorize(&s, &headers, w, Action::Book).await?;
    Ok(Json(store::transition(&s.db, b, id, "cancelled").await?))
}
