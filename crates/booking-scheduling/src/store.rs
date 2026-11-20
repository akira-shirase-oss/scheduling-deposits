//! Storage. Every query names the business (the workspace id) in its key; writes that are
//! bounded lock the business row first and count in the same transaction.

use std::collections::HashMap;

use availability::{Busy, Exception, ExceptionKind, Minutes, WeeklySpan};
use jiff::Timestamp;
use jiff::civil::Date;
use serde::Serialize;
use sqlx::{AssertSqlSafe, PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::domain::{
    self, Business, Deposit, ExceptionRule, Hold, MAX_BOOKINGS_LISTED, MAX_EXCEPTIONS,
    MAX_SERVICES, MAX_STAFF, Service, WeeklyRange,
};
use crate::error::SchedulingError;

type Result<T> = std::result::Result<T, SchedulingError>;

fn is_fk_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23503"))
}

fn timestamp(second: i64) -> Result<Timestamp> {
    Timestamp::from_second(second)
        .map_err(|e| SchedulingError::Internal(anyhow::anyhow!("stored instant out of range: {e}")))
}

fn date(raw: &str) -> Result<Date> {
    raw.parse()
        .map_err(|e| SchedulingError::Internal(anyhow::anyhow!("stored date unreadable: {e}")))
}

fn small(v: i16) -> u16 {
    u16::try_from(v).unwrap_or(0)
}

/// Locks the business row: the writes it bounds serialise here.
async fn lock_business(tx: &mut PgConnection, business: Uuid) -> Result<()> {
    sqlx::query_scalar::<_, i32>("SELECT 1 FROM businesses WHERE workspace_id = $1 FOR UPDATE")
        .bind(business)
        .fetch_optional(&mut *tx)
        .await?
        .map(|_| ())
        .ok_or(SchedulingError::BusinessRequired)
}

async fn count(tx: &mut PgConnection, table: &str, business: Uuid) -> Result<i64> {
    // `table` is one of this module's literals, never input.
    let sql = format!("SELECT count(*) FROM {table} WHERE business_id = $1");
    Ok(sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(business)
        .fetch_one(&mut *tx)
        .await?)
}

// ---------------------------------------------------------------- business

#[derive(Debug, Clone, Serialize)]
pub struct BusinessRow {
    pub workspace_id: Uuid,
    #[serde(flatten)]
    pub settings: Business,
}

const BUSINESS_COLUMNS: &str = "workspace_id, name, time_zone, currency, slot_step_minutes, \
     min_notice_minutes, max_advance_days, hold_minutes";

fn business_row(r: &sqlx::postgres::PgRow) -> BusinessRow {
    BusinessRow {
        workspace_id: r.get("workspace_id"),
        settings: Business {
            name: r.get("name"),
            time_zone: r.get("time_zone"),
            currency: r.get("currency"),
            slot_step_minutes: small(r.get("slot_step_minutes")),
            min_notice_minutes: u32::try_from(r.get::<i32, _>("min_notice_minutes")).unwrap_or(0),
            max_advance_days: u32::try_from(r.get::<i32, _>("max_advance_days")).unwrap_or(0),
            hold_minutes: small(r.get("hold_minutes")),
        },
    }
}

pub async fn business(db: &PgPool, business: Uuid) -> Result<Option<BusinessRow>> {
    let sql = format!("SELECT {BUSINESS_COLUMNS} FROM businesses WHERE workspace_id = $1");
    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(business)
        .fetch_optional(db)
        .await?;
    Ok(row.as_ref().map(business_row))
}

/// Creates or replaces the business behind `workspace` (onboarding, then edits).
pub async fn put_business(
    db: &PgPool,
    workspace: Uuid,
    b: &Business,
) -> Result<(BusinessRow, bool)> {
    let sql = format!(
        "INSERT INTO businesses ({BUSINESS_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (workspace_id) DO UPDATE SET name = EXCLUDED.name,
             time_zone = EXCLUDED.time_zone, currency = EXCLUDED.currency,
             slot_step_minutes = EXCLUDED.slot_step_minutes,
             min_notice_minutes = EXCLUDED.min_notice_minutes,
             max_advance_days = EXCLUDED.max_advance_days,
             hold_minutes = EXCLUDED.hold_minutes, updated_at = now()
         RETURNING {BUSINESS_COLUMNS}, (xmax = 0) AS created"
    );
    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(workspace)
        .bind(&b.name)
        .bind(&b.time_zone)
        .bind(&b.currency)
        .bind(i16::try_from(b.slot_step_minutes).unwrap_or(i16::MAX))
        .bind(i32::try_from(b.min_notice_minutes).unwrap_or(i32::MAX))
        .bind(i32::try_from(b.max_advance_days).unwrap_or(i32::MAX))
        .bind(i16::try_from(b.hold_minutes).unwrap_or(i16::MAX))
        .fetch_one(db)
        .await?;
    Ok((business_row(&row), row.get("created")))
}

/// Every zone a business uses, for the startup check.
pub async fn zones(db: &PgPool) -> Result<Vec<String>> {
    Ok(
        sqlx::query_scalar("SELECT DISTINCT time_zone FROM businesses ORDER BY 1")
            .fetch_all(db)
            .await?,
    )
}

// ---------------------------------------------------------------- services

#[derive(Debug, Clone, Serialize)]
pub struct ServiceRow {
    pub id: Uuid,
    pub name: String,
    pub duration_minutes: u16,
    pub buffer_before_minutes: u16,
    pub buffer_after_minutes: u16,
    pub price_minor: i64,
    pub deposit: Deposit,
    /// The deposit on the price, in minor units.
    pub deposit_minor: i64,
    pub active: bool,
}

const SERVICE_COLUMNS: &str = "id, name, duration_minutes, buffer_before_minutes, \
     buffer_after_minutes, price_minor, deposit_kind, deposit_value, active";

fn service_row(r: &sqlx::postgres::PgRow) -> Result<ServiceRow> {
    let price_minor: i64 = r.get("price_minor");
    let deposit = Deposit::from_stored(r.get("deposit_kind"), r.get("deposit_value"))
        .ok_or_else(|| SchedulingError::Internal(anyhow::anyhow!("stored deposit kind unknown")))?;
    Ok(ServiceRow {
        id: r.get("id"),
        name: r.get("name"),
        duration_minutes: small(r.get("duration_minutes")),
        buffer_before_minutes: small(r.get("buffer_before_minutes")),
        buffer_after_minutes: small(r.get("buffer_after_minutes")),
        price_minor,
        deposit,
        deposit_minor: deposit.amount(price_minor),
        active: r.get("active"),
    })
}

pub async fn services(db: &PgPool, business: Uuid) -> Result<Vec<ServiceRow>> {
    let sql =
        format!("SELECT {SERVICE_COLUMNS} FROM services WHERE business_id = $1 ORDER BY name, id");
    sqlx::query(AssertSqlSafe(sql))
        .bind(business)
        .fetch_all(db)
        .await?
        .iter()
        .map(service_row)
        .collect()
}

pub async fn service(db: &PgPool, business: Uuid, id: Uuid) -> Result<Option<ServiceRow>> {
    let sql = format!("SELECT {SERVICE_COLUMNS} FROM services WHERE business_id = $1 AND id = $2");
    sqlx::query(AssertSqlSafe(sql))
        .bind(business)
        .bind(id)
        .fetch_optional(db)
        .await?
        .as_ref()
        .map(service_row)
        .transpose()
}

pub async fn put_service(
    db: &PgPool,
    business: Uuid,
    id: Uuid,
    s: &Service,
) -> Result<(ServiceRow, bool)> {
    let mut tx = begin(db).await?;
    lock_business(&mut tx, business).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM services WHERE business_id = $1 AND id = $2)",
    )
    .bind(business)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if !exists && count(&mut tx, "services", business).await? >= MAX_SERVICES {
        return Err(SchedulingError::Limit("services_limit"));
    }
    let sql = format!(
        "INSERT INTO services (business_id, {SERVICE_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (business_id, id) DO UPDATE SET name = EXCLUDED.name,
             duration_minutes = EXCLUDED.duration_minutes,
             buffer_before_minutes = EXCLUDED.buffer_before_minutes,
             buffer_after_minutes = EXCLUDED.buffer_after_minutes,
             price_minor = EXCLUDED.price_minor, deposit_kind = EXCLUDED.deposit_kind,
             deposit_value = EXCLUDED.deposit_value, active = EXCLUDED.active, updated_at = now()
         RETURNING {SERVICE_COLUMNS}"
    );
    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(business)
        .bind(id)
        .bind(&s.name)
        .bind(i16::try_from(s.duration_minutes).unwrap_or(i16::MAX))
        .bind(i16::try_from(s.buffer_before_minutes).unwrap_or(i16::MAX))
        .bind(i16::try_from(s.buffer_after_minutes).unwrap_or(i16::MAX))
        .bind(s.price_minor)
        .bind(s.deposit.kind())
        .bind(s.deposit.value())
        .bind(s.active)
        .fetch_one(&mut *tx)
        .await?;
    let out = service_row(&row)?;
    tx.commit().await?;
    Ok((out, !exists))
}

/// Deletes a service and its staff assignments; refused while a booking references it.
pub async fn delete_service(db: &PgPool, business: Uuid, id: Uuid) -> Result<bool> {
    match sqlx::query("DELETE FROM services WHERE business_id = $1 AND id = $2")
        .bind(business)
        .bind(id)
        .execute(db)
        .await
    {
        Ok(done) => Ok(done.rows_affected() == 1),
        Err(e) if is_fk_violation(&e) => Err(SchedulingError::Conflict("in_use")),
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------- staff

#[derive(Debug, Clone, Serialize)]
pub struct StaffRow {
    pub id: Uuid,
    pub name: String,
    pub active: bool,
    pub service_ids: Vec<Uuid>,
    pub hours: Vec<WeeklyRange>,
}

pub async fn staff_list(db: &PgPool, business: Uuid) -> Result<Vec<StaffRow>> {
    let rows = sqlx::query(
        "SELECT s.id, s.name, s.active,
                coalesce((SELECT array_agg(a.service_id ORDER BY a.service_id) FROM staff_services a
                           WHERE a.business_id = s.business_id AND a.staff_id = s.id), '{}') AS service_ids,
                coalesce((SELECT array_agg(h.weekday ORDER BY h.weekday, h.start_minute) FROM weekly_hours h
                           WHERE h.business_id = s.business_id AND h.staff_id = s.id), '{}') AS days,
                coalesce((SELECT array_agg(h.start_minute ORDER BY h.weekday, h.start_minute) FROM weekly_hours h
                           WHERE h.business_id = s.business_id AND h.staff_id = s.id), '{}') AS starts,
                coalesce((SELECT array_agg(h.end_minute ORDER BY h.weekday, h.start_minute) FROM weekly_hours h
                           WHERE h.business_id = s.business_id AND h.staff_id = s.id), '{}') AS ends
           FROM staff s WHERE s.business_id = $1 ORDER BY s.name, s.id",
    )
    .bind(business)
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let days: Vec<i16> = r.get("days");
            let starts: Vec<i16> = r.get("starts");
            let ends: Vec<i16> = r.get("ends");
            StaffRow {
                id: r.get("id"),
                name: r.get("name"),
                active: r.get("active"),
                service_ids: r.get("service_ids"),
                hours: days
                    .iter()
                    .zip(starts.iter().zip(&ends))
                    .map(|(d, (s, e))| WeeklyRange {
                        weekday: i8::try_from(*d).unwrap_or(0),
                        start: domain::format_hhmm(small(*s)),
                        end: domain::format_hhmm(small(*e)),
                    })
                    .collect(),
            }
        })
        .collect())
}

pub async fn staff(db: &PgPool, business: Uuid, id: Uuid) -> Result<Option<StaffRow>> {
    Ok(staff_list(db, business)
        .await?
        .into_iter()
        .find(|s| s.id == id))
}

pub async fn put_staff(
    db: &PgPool,
    business: Uuid,
    id: Uuid,
    name: &str,
    active: bool,
) -> Result<bool> {
    let mut tx = begin(db).await?;
    lock_business(&mut tx, business).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM staff WHERE business_id = $1 AND id = $2)",
    )
    .bind(business)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if !exists && count(&mut tx, "staff", business).await? >= MAX_STAFF {
        return Err(SchedulingError::Limit("staff_limit"));
    }
    sqlx::query(
        "INSERT INTO staff (business_id, id, name, active) VALUES ($1, $2, $3, $4)
         ON CONFLICT (business_id, id) DO UPDATE SET name = EXCLUDED.name, active = EXCLUDED.active, updated_at = now()",
    )
    .bind(business).bind(id).bind(name).bind(active)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(!exists)
}

pub async fn delete_staff(db: &PgPool, business: Uuid, id: Uuid) -> Result<bool> {
    match sqlx::query("DELETE FROM staff WHERE business_id = $1 AND id = $2")
        .bind(business)
        .bind(id)
        .execute(db)
        .await
    {
        Ok(done) => Ok(done.rows_affected() == 1),
        Err(e) if is_fk_violation(&e) => Err(SchedulingError::Conflict("in_use")),
        Err(e) => Err(e.into()),
    }
}

async fn lock_staff(tx: &mut PgConnection, business: Uuid, staff: Uuid) -> Result<()> {
    sqlx::query_scalar::<_, i32>(
        "SELECT 1 FROM staff WHERE business_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(business)
    .bind(staff)
    .fetch_optional(&mut *tx)
    .await?
    .map(|_| ())
    .ok_or(SchedulingError::NotFound)
}

/// Replaces which services a staff member performs. Every id must be this business's.
pub async fn set_staff_services(
    db: &PgPool,
    business: Uuid,
    staff: Uuid,
    services: &[Uuid],
) -> Result<()> {
    let mut tx = begin(db).await?;
    lock_staff(&mut tx, business, staff).await?;
    let known: i64 =
        sqlx::query_scalar("SELECT count(*) FROM services WHERE business_id = $1 AND id = ANY($2)")
            .bind(business)
            .bind(services)
            .fetch_one(&mut *tx)
            .await?;
    if usize::try_from(known).ok() != Some(services.len()) {
        return Err(SchedulingError::Invalid(
            "unknown_service",
            "a service id is not one of this business's",
        ));
    }
    sqlx::query("DELETE FROM staff_services WHERE business_id = $1 AND staff_id = $2")
        .bind(business)
        .bind(staff)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO staff_services (business_id, staff_id, service_id) SELECT $1, $2, unnest($3::uuid[])")
        .bind(business).bind(staff).bind(services).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Replaces a staff member's weekly hours.
pub async fn set_hours(
    db: &PgPool,
    business: Uuid,
    staff: Uuid,
    hours: &[(i8, Minutes)],
) -> Result<()> {
    let mut tx = begin(db).await?;
    lock_staff(&mut tx, business, staff).await?;
    sqlx::query("DELETE FROM weekly_hours WHERE business_id = $1 AND staff_id = $2")
        .bind(business)
        .bind(staff)
        .execute(&mut *tx)
        .await?;
    let days: Vec<i16> = hours.iter().map(|(d, _)| i16::from(*d)).collect();
    let starts: Vec<i16> = hours
        .iter()
        .map(|(_, m)| i16::try_from(m.start()).unwrap_or(0))
        .collect();
    let ends: Vec<i16> = hours
        .iter()
        .map(|(_, m)| i16::try_from(m.end()).unwrap_or(0))
        .collect();
    sqlx::query(
        "INSERT INTO weekly_hours (business_id, staff_id, weekday, start_minute, end_minute)
         SELECT $1, $2, d, s, e FROM unnest($3::int2[], $4::int2[], $5::int2[]) AS t(d, s, e)",
    )
    .bind(business)
    .bind(staff)
    .bind(&days)
    .bind(&starts)
    .bind(&ends)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------- exceptions

#[derive(Debug, Clone, Serialize)]
pub struct ExceptionRow {
    pub id: Uuid,
    pub staff_id: Option<Uuid>,
    pub first_date: String,
    pub last_date: String,
    pub kind: &'static str,
    pub start: Option<String>,
    pub end: Option<String>,
    pub note: Option<String>,
}

fn exception_rule(r: &sqlx::postgres::PgRow) -> Result<(Uuid, ExceptionRule)> {
    let span = match (
        r.get::<Option<i16>, _>("start_minute"),
        r.get::<Option<i16>, _>("end_minute"),
    ) {
        (Some(s), Some(e)) => Minutes::new(small(s), small(e)),
        _ => None,
    };
    let kind = match (r.get::<&str, _>("kind"), span) {
        ("open", Some(m)) => ExceptionKind::Open(m),
        (_, m) => ExceptionKind::Closed(m),
    };
    Ok((
        r.get("id"),
        ExceptionRule {
            staff_id: r.get("staff_id"),
            first: date(r.get("first_date"))?,
            last: date(r.get("last_date"))?,
            kind,
            note: r.get("note"),
        },
    ))
}

impl ExceptionRow {
    fn from_rule(id: Uuid, rule: &ExceptionRule) -> Self {
        let (kind, span) = match rule.kind {
            ExceptionKind::Open(m) => ("open", Some(m)),
            ExceptionKind::Closed(m) => ("closed", m),
        };
        Self {
            id,
            staff_id: rule.staff_id,
            first_date: rule.first.to_string(),
            last_date: rule.last.to_string(),
            kind,
            start: span.map(|m| domain::format_hhmm(m.start())),
            end: span.map(|m| domain::format_hhmm(m.end())),
            note: rule.note.clone(),
        }
    }
}

const EXCEPTION_COLUMNS: &str = "id, staff_id, first_date::text AS first_date, last_date::text AS last_date, kind, start_minute, end_minute, note";

async fn exception_rules(
    db: impl sqlx::PgExecutor<'_>,
    business: Uuid,
    from: Option<Date>,
    until: Option<Date>,
) -> Result<Vec<(Uuid, ExceptionRule)>> {
    let sql = format!(
        "SELECT {EXCEPTION_COLUMNS} FROM hours_exceptions
          WHERE business_id = $1 AND ($2::date IS NULL OR last_date >= $2::date)
            AND ($3::date IS NULL OR first_date <= $3::date)
          ORDER BY first_date, id LIMIT {MAX_EXCEPTIONS}"
    );
    sqlx::query(AssertSqlSafe(sql))
        .bind(business)
        .bind(from.map(|d| d.to_string()))
        .bind(until.map(|d| d.to_string()))
        .fetch_all(db)
        .await?
        .iter()
        .map(exception_rule)
        .collect()
}

pub async fn exceptions(
    db: &PgPool,
    business: Uuid,
    from: Option<Date>,
    until: Option<Date>,
) -> Result<Vec<ExceptionRow>> {
    Ok(exception_rules(db, business, from, until)
        .await?
        .iter()
        .map(|(id, rule)| ExceptionRow::from_rule(*id, rule))
        .collect())
}

pub async fn put_exception(
    db: &PgPool,
    business: Uuid,
    id: Uuid,
    rule: &ExceptionRule,
) -> Result<(ExceptionRow, bool)> {
    let mut tx = begin(db).await?;
    lock_business(&mut tx, business).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM hours_exceptions WHERE business_id = $1 AND id = $2)",
    )
    .bind(business)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if !exists && count(&mut tx, "hours_exceptions", business).await? >= MAX_EXCEPTIONS {
        return Err(SchedulingError::Limit("exceptions_limit"));
    }
    let row = ExceptionRow::from_rule(id, rule);
    let span = match rule.kind {
        ExceptionKind::Open(m) | ExceptionKind::Closed(Some(m)) => Some(m),
        ExceptionKind::Closed(None) => None,
    };
    let done = sqlx::query(
        "INSERT INTO hours_exceptions (business_id, id, staff_id, first_date, last_date, kind, start_minute, end_minute, note)
         VALUES ($1, $2, $3, $4::date, $5::date, $6, $7, $8, $9)
         ON CONFLICT (business_id, id) DO UPDATE SET staff_id = EXCLUDED.staff_id,
             first_date = EXCLUDED.first_date, last_date = EXCLUDED.last_date, kind = EXCLUDED.kind,
             start_minute = EXCLUDED.start_minute, end_minute = EXCLUDED.end_minute, note = EXCLUDED.note",
    )
    .bind(business)
    .bind(id)
    .bind(rule.staff_id)
    .bind(&row.first_date)
    .bind(&row.last_date)
    .bind(row.kind)
    .bind(span.map(|m| i16::try_from(m.start()).unwrap_or(0)))
    .bind(span.map(|m| i16::try_from(m.end()).unwrap_or(0)))
    .bind(&rule.note)
    .execute(&mut *tx)
    .await;
    match done {
        Ok(_) => {}
        // The composite key (business, staff): another business's staff id is unknown here.
        Err(e) if is_fk_violation(&e) => {
            return Err(SchedulingError::Invalid(
                "unknown_staff",
                "the staff id is not one of this business's",
            ));
        }
        Err(e) => return Err(e.into()),
    }
    tx.commit().await?;
    Ok((row, !exists))
}

pub async fn delete_exception(db: &PgPool, business: Uuid, id: Uuid) -> Result<bool> {
    let done = sqlx::query("DELETE FROM hours_exceptions WHERE business_id = $1 AND id = $2")
        .bind(business)
        .bind(id)
        .execute(db)
        .await?;
    Ok(done.rows_affected() == 1)
}

// ---------------------------------------------------------------- availability

/// Everything the engine needs for one read, from one snapshot, with the database's clock.
#[derive(Debug)]
pub struct Inputs {
    pub business: BusinessRow,
    pub service: ServiceRow,
    pub staff: Vec<availability::Staff>,
    pub exceptions: Vec<Exception>,
    pub now: Timestamp,
}

/// Begins a transaction, timing the wait for a pooled connection (with its `BEGIN`).
async fn begin(
    db: &PgPool,
) -> std::result::Result<sqlx::Transaction<'static, sqlx::Postgres>, sqlx::Error> {
    booking_http::timing::timed(booking_http::timing::Stage::PoolWait, db.begin()).await
}

#[expect(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "one snapshot transaction, read top to bottom"
)]
/// Loads the inputs for `service` on the dates `from..from + days` (local), for `only` one
/// staff member or every active one who performs it. `with_busy: false` leaves out the
/// bookings (the hold path checks the hours here and leaves overlap to the constraint).
///
/// Refuses, before it loads hours, exceptions or bookings, a read whose grid (staff × days
/// × grid points a day) is over `max_cells`.
pub async fn inputs(
    db: &PgPool,
    business: Uuid,
    service_id: Uuid,
    only: Option<Uuid>,
    from: Date,
    days: u32,
    with_busy: bool,
    max_cells: u64,
) -> Result<Inputs> {
    let mut tx = begin(db).await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let sql = format!(
        "SELECT {BUSINESS_COLUMNS}, extract(epoch FROM now())::bigint AS now FROM businesses WHERE workspace_id = $1"
    );
    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(business)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(SchedulingError::BusinessRequired)?;
    let business_row = business_row(&row);
    let now = timestamp(row.get("now"))?;
    let sql = format!("SELECT {SERVICE_COLUMNS} FROM services WHERE business_id = $1 AND id = $2");
    let service = sqlx::query(AssertSqlSafe(sql))
        .bind(business)
        .bind(service_id)
        .fetch_optional(&mut *tx)
        .await?
        .as_ref()
        .map(service_row)
        .transpose()?
        .filter(|s| s.active)
        .ok_or(SchedulingError::NotFound)?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT s.id FROM staff s JOIN staff_services a
             ON a.business_id = s.business_id AND a.staff_id = s.id AND a.service_id = $2
          WHERE s.business_id = $1 AND s.active AND ($3::uuid IS NULL OR s.id = $3)
          ORDER BY s.id",
    )
    .bind(business)
    .bind(service_id)
    .bind(only)
    .fetch_all(&mut *tx)
    .await?;
    let cells = availability::cells(ids.len(), days, business_row.settings.slot_step_minutes);
    if cells > max_cells {
        return Err(SchedulingError::Invalid(
            "too_much_work",
            "narrow the read: fewer days or one staff member",
        ));
    }
    let hours = sqlx::query(
        "SELECT staff_id, weekday, start_minute, end_minute FROM weekly_hours
          WHERE business_id = $1 AND staff_id = ANY($2)",
    )
    .bind(business)
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?;
    let last = from
        .checked_add(jiff::Span::new().days(i64::from(days)))
        .map_err(|_| SchedulingError::Invalid("invalid_date", "the range is out of bounds"))?;
    let day_before = from
        .yesterday()
        .map_err(|_| SchedulingError::Invalid("invalid_date", "the range is out of bounds"))?;
    let rules = exception_rules(&mut *tx, business, Some(day_before), Some(last)).await?;
    let mut busy: HashMap<Uuid, Vec<Busy>> = HashMap::new();
    if with_busy {
        // Grouped per staff member in SQL, in order. Wider than any offset: a booking
        // outside this cannot touch a slot of these dates.
        let rows = sqlx::query(
            "SELECT staff_id,
                    array_agg(extract(epoch FROM lower(blocked))::bigint ORDER BY lower(blocked)) AS f,
                    array_agg(extract(epoch FROM upper(blocked))::bigint ORDER BY lower(blocked)) AS u
               FROM bookings
              WHERE business_id = $1 AND staff_id = ANY($2)
                AND status IN ('held', 'confirmed')
                AND (status = 'confirmed' OR hold_expires_at > now())
                AND blocked && tstzrange(($3::date - 2)::timestamp AT TIME ZONE 'UTC', ($4::date + 3)::timestamp AT TIME ZONE 'UTC')
              GROUP BY staff_id",
        )
        .bind(business).bind(&ids).bind(from.to_string()).bind(last.to_string())
        .fetch_all(&mut *tx)
        .await?;
        for r in rows {
            let (f, u): (Vec<i64>, Vec<i64>) = (r.get("f"), r.get("u"));
            let ranges = f
                .into_iter()
                .zip(u)
                .map(|(f, u)| {
                    Ok(Busy {
                        from: timestamp(f)?,
                        until: timestamp(u)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            busy.insert(r.get("staff_id"), ranges);
        }
    }
    tx.commit().await?;
    let mut weekly: HashMap<Uuid, Vec<WeeklySpan>> = HashMap::new();
    for h in &hours {
        let span = i8::try_from(h.get::<i16, _>("weekday"))
            .ok()
            .zip(Minutes::new(
                small(h.get("start_minute")),
                small(h.get("end_minute")),
            ));
        if let Some((weekday, span)) = span {
            weekly
                .entry(h.get("staff_id"))
                .or_default()
                .push(WeeklySpan { weekday, span });
        }
    }
    let staff = ids
        .iter()
        .map(|&id| availability::Staff {
            id,
            weekly: weekly.remove(&id).unwrap_or_default(),
            busy: busy.remove(&id).unwrap_or_default(),
        })
        .collect();
    let exceptions = rules
        .into_iter()
        .map(|(_, r)| Exception {
            staff: r.staff_id,
            first: r.first,
            last: r.last,
            kind: r.kind,
        })
        .collect();
    Ok(Inputs {
        business: business_row,
        service,
        staff,
        exceptions,
        now,
    })
}

// ---------------------------------------------------------------- bookings

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BookingRow {
    pub id: Uuid,
    pub staff_id: Uuid,
    pub service_id: Uuid,
    pub status: String,
    pub starts_at: String,
    pub ends_at: String,
    /// The range the staff member is unavailable: the service and both buffers.
    pub blocked_from: String,
    pub blocked_until: String,
    pub hold_expires_at: Option<String>,
    pub customer_name: String,
    pub customer_email: Option<String>,
    pub idempotency_key: String,
}

const BOOKING_COLUMNS: &str = "id, staff_id, service_id, \
     CASE WHEN status = 'held' AND hold_expires_at <= now() THEN 'expired' ELSE status END AS status, \
     extract(epoch FROM starts_at)::bigint AS s, extract(epoch FROM ends_at)::bigint AS e, \
     extract(epoch FROM lower(blocked))::bigint AS bf, extract(epoch FROM upper(blocked))::bigint AS bu, \
     extract(epoch FROM hold_expires_at)::bigint AS x, customer_name, customer_email, idempotency_key";

fn booking_row(r: &sqlx::postgres::PgRow) -> Result<BookingRow> {
    let at = |c: &str| -> Result<String> { Ok(timestamp(r.get(c))?.to_string()) };
    Ok(BookingRow {
        id: r.get("id"),
        staff_id: r.get("staff_id"),
        service_id: r.get("service_id"),
        status: r.get("status"),
        starts_at: at("s")?,
        ends_at: at("e")?,
        blocked_from: at("bf")?,
        blocked_until: at("bu")?,
        hold_expires_at: r
            .get::<Option<i64>, _>("x")
            .map(timestamp)
            .transpose()?
            .map(|t| t.to_string()),
        customer_name: r.get("customer_name"),
        customer_email: r.get("customer_email"),
        idempotency_key: r.get("idempotency_key"),
    })
}

pub async fn booking(
    db: impl sqlx::PgExecutor<'_>,
    business: Uuid,
    id: Uuid,
) -> Result<Option<BookingRow>> {
    let sql = format!("SELECT {BOOKING_COLUMNS} FROM bookings WHERE business_id = $1 AND id = $2");
    sqlx::query(AssertSqlSafe(sql))
        .bind(business)
        .bind(id)
        .fetch_optional(db)
        .await?
        .as_ref()
        .map(booking_row)
        .transpose()
}

/// Bookings starting in `[from, until)`, oldest first, at most [`MAX_BOOKINGS_LISTED`].
pub async fn bookings(
    db: &PgPool,
    business: Uuid,
    from: Timestamp,
    until: Timestamp,
) -> Result<Vec<BookingRow>> {
    let sql = format!(
        "SELECT {BOOKING_COLUMNS} FROM bookings
          WHERE business_id = $1 AND starts_at >= to_timestamp($2) AND starts_at < to_timestamp($3)
          ORDER BY starts_at, id LIMIT {MAX_BOOKINGS_LISTED}"
    );
    sqlx::query(AssertSqlSafe(sql))
        .bind(business)
        .bind(from.as_second())
        .bind(until.as_second())
        .fetch_all(db)
        .await?
        .iter()
        .map(booking_row)
        .collect()
}

/// Places a hold. The database computes the blocked range, expires stale holds in the way,
/// and refuses an overlap (23P01 → [`SchedulingError::SlotTaken`]). A retry with the same
/// idempotency key returns the first hold (`false`), or a conflict if the request differs.
pub async fn hold(
    db: &PgPool,
    business: Uuid,
    principal: Uuid,
    hold: &Hold,
    hold_minutes: u16,
    open_holds: u32,
) -> Result<(BookingRow, bool)> {
    let mut tx = begin(db).await?;
    // A retry of a key already used is answered below, never refused by a cap.
    let replay: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM bookings WHERE business_id = $1 AND idempotency_key = $2",
    )
    .bind(business)
    .bind(&hold.idempotency_key)
    .fetch_optional(&mut *tx)
    .await?;
    if replay.is_none() {
        // One staff member's holds are decided one at a time: the exclusion check then
        // meets only committed rows and answers 409, never a wait that ends in a deadlock
        // or a lock timeout (503).
        sqlx::query("SELECT 1 FROM staff WHERE business_id = $1 AND id = $2 FOR UPDATE")
            .bind(business)
            .bind(hold.staff_id)
            .fetch_optional(&mut *tx)
            .await?;
    }
    let id = Uuid::now_v7();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO bookings (business_id, id, staff_id, service_id, status, starts_at, ends_at,
                               buffer_before_minutes, buffer_after_minutes, blocked, hold_expires_at,
                               customer_name, customer_email, idempotency_key, created_at, updated_at,
                               held_by)
         VALUES ($1, $2, $3, $4, 'held', to_timestamp($5), to_timestamp($5), 0, 0, 'empty',
                 now() + make_interval(mins => $6), $7, $8, $9, now(), now(), $10)
         ON CONFLICT (business_id, idempotency_key) DO NOTHING
         RETURNING id",
    )
    .bind(business)
    .bind(id)
    .bind(hold.staff_id)
    .bind(hold.service_id)
    .bind(hold.starts_at.as_second())
    .bind(i32::from(hold_minutes))
    .bind(&hold.customer_name)
    .bind(&hold.customer_email)
    .bind(&hold.idempotency_key)
    .bind(principal)
    .fetch_optional(&mut *tx)
    .await?;
    let (id, created) = if let Some(id) = inserted {
        // The principal's open holds, this one included, counted under a lock on the
        // principal taken last (after the slot is won, so losers never wait for it), so
        // concurrent holds cannot pass the cap together.
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended($1::text || '/' || $2::text, 0))",
        )
        .bind(business)
        .bind(principal)
        .execute(&mut *tx)
        .await?;
        let open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM bookings
              WHERE business_id = $1 AND held_by = $2 AND status = 'held' AND hold_expires_at > now()",
        )
        .bind(business)
        .bind(principal)
        .fetch_one(&mut *tx)
        .await?;
        // LIMIT(open-holds)
        if open > i64::from(open_holds) {
            return Err(SchedulingError::Limit("open_holds"));
        }
        (id, true)
    } else {
        let existing: Uuid = sqlx::query_scalar(
            "SELECT id FROM bookings WHERE business_id = $1 AND idempotency_key = $2",
        )
        .bind(business)
        .bind(&hold.idempotency_key)
        .fetch_one(&mut *tx)
        .await?;
        (existing, false)
    };
    let row = booking(&mut *tx, business, id)
        .await?
        .ok_or(SchedulingError::NotFound)?;
    tx.commit().await?;
    if !created {
        let same = row.staff_id == hold.staff_id
            && row.service_id == hold.service_id
            && row.starts_at == hold.starts_at.to_string()
            && row.customer_name == hold.customer_name
            && row.customer_email == hold.customer_email;
        if !same {
            return Err(SchedulingError::IdempotencyConflict);
        }
    }
    Ok((row, created))
}

/// Moves a booking to `status` (`confirmed` or `cancelled`); the database enforces which
/// moves are allowed and that an expired hold cannot be confirmed.
pub async fn transition(db: &PgPool, business: Uuid, id: Uuid, status: &str) -> Result<BookingRow> {
    let mut tx = begin(db).await?;
    let done = sqlx::query("UPDATE bookings SET status = $3 WHERE business_id = $1 AND id = $2")
        .bind(business)
        .bind(id)
        .bind(status)
        .execute(&mut *tx)
        .await?;
    if done.rows_affected() == 0 {
        return Err(SchedulingError::NotFound);
    }
    let row = booking(&mut *tx, business, id)
        .await?
        .ok_or(SchedulingError::NotFound)?;
    tx.commit().await?;
    Ok(row)
}
