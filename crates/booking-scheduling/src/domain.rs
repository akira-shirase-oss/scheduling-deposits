//! The product's input rules, with no I/O and no clock: what a business, a service, a
//! staff member, weekly hours, an exception and a hold may say. Every bound here is also a
//! CHECK in `migrations/0003_scheduling.sql`; this is where a caller learns which rule it
//! broke.

use availability::{ExceptionKind, Minutes};
use jiff::civil::Date;
use serde::{Deserialize, Serialize};

/// Services one business may have. LIMIT(services)
pub const MAX_SERVICES: i64 = 200;
/// Staff one business may have. LIMIT(staff)
pub const MAX_STAFF: i64 = 100;
/// Exceptions one business may have. LIMIT(exceptions)
pub const MAX_EXCEPTIONS: i64 = 1_000;
/// Weekly ranges per staff member. LIMIT(weekly-hours)
pub const MAX_WEEKLY_RANGES: usize = 50;
/// Dates one availability read may cover. LIMIT(availability-read-days)
pub const MAX_READ_DAYS: u32 = 31;
/// Slots one availability read returns; the rest is reported as truncated. LIMIT(slots)
pub const MAX_SLOTS: usize = 5_000;
/// A hold starts at most this long before now (clock skew; the hours check refuses the past).
pub const HOLD_START_PAST_SECONDS: i64 = 86_400;
/// A hold starts at most this long after now, before any database work.
pub const HOLD_START_AHEAD_SECONDS: i64 = 366 * 86_400;
/// Bookings one list returns. LIMIT(booking-list)
pub const MAX_BOOKINGS_LISTED: i64 = 500;
/// Dates one exception may cover (inclusive span, in days).
pub const MAX_EXCEPTION_SPAN_DAYS: i32 = 366;

pub type Invalid = (&'static str, &'static str);

fn name(raw: &str, code: &'static str) -> Result<String, Invalid> {
    let trimmed = raw.trim();
    let chars = trimmed.chars().count();
    if chars == 0 || chars > 200 || trimmed.chars().any(char::is_control) {
        return Err((code, "a name is 1 to 200 characters, no control characters"));
    }
    Ok(trimmed.to_owned())
}

/// `"HH:MM"`, `00:00` to `24:00`, as minutes of the day.
#[must_use]
pub fn parse_hhmm(raw: &str) -> Option<u16> {
    let (h, m) = raw.split_once(':')?;
    if h.len() != 2 || m.len() != 2 || !h.bytes().chain(m.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    let total = h * 60 + m;
    (m < 60 && total <= 1440).then_some(total)
}

#[must_use]
pub fn format_hhmm(minutes: u16) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

fn span(start: &str, end: &str) -> Result<Minutes, Invalid> {
    let parsed = parse_hhmm(start).zip(parse_hhmm(end));
    parsed.and_then(|(s, e)| Minutes::new(s, e)).ok_or((
        "invalid_hours",
        "hours are \"HH:MM\" to \"HH:MM\", start before end",
    ))
}

/// The business's settings, as sent to `PUT …/business`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BusinessInput {
    pub name: String,
    /// IANA zone name; the server also resolves it against the zone database.
    pub time_zone: String,
    pub currency: String,
    #[serde(default = "default_step")]
    pub slot_step_minutes: u16,
    #[serde(default)]
    pub min_notice_minutes: u32,
    #[serde(default = "default_advance")]
    pub max_advance_days: u32,
    #[serde(default = "default_hold")]
    pub hold_minutes: u16,
}

const fn default_step() -> u16 {
    15
}
const fn default_advance() -> u32 {
    60
}
const fn default_hold() -> u16 {
    10
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Business {
    pub name: String,
    pub time_zone: String,
    pub currency: String,
    pub slot_step_minutes: u16,
    pub min_notice_minutes: u32,
    pub max_advance_days: u32,
    pub hold_minutes: u16,
}

/// Shape only: `Area/City`-like, the characters IANA names use. Whether the zone exists is
/// the zone database's answer, asked by the caller.
#[must_use]
pub fn zone_name_shaped(raw: &str) -> bool {
    (1..=64).contains(&raw.len())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_+-/".contains(&b))
        && !raw.starts_with('/')
        && !raw.contains("..")
}

impl BusinessInput {
    /// # Errors
    /// The first rule broken.
    pub fn validate(self) -> Result<Business, Invalid> {
        let name = name(&self.name, "invalid_name")?;
        if !zone_name_shaped(&self.time_zone) {
            return Err(("invalid_time_zone", "the time zone is not an IANA name"));
        }
        if !(self.currency.len() == 3 && self.currency.bytes().all(|b| b.is_ascii_uppercase())) {
            return Err((
                "invalid_currency",
                "the currency is an ISO 4217 code, e.g. EUR",
            ));
        }
        if !availability::STEPS.contains(&self.slot_step_minutes) {
            return Err((
                "invalid_step",
                "the slot step is 5, 10, 15, 20, 30 or 60 minutes",
            ));
        }
        if self.min_notice_minutes > 43_200 {
            return Err(("invalid_notice", "the minimum notice is 0 to 43200 minutes"));
        }
        if !(1..=365).contains(&self.max_advance_days) {
            return Err(("invalid_advance", "the maximum advance is 1 to 365 days"));
        }
        if !(1..=60).contains(&self.hold_minutes) {
            return Err(("invalid_hold", "a hold lasts 1 to 60 minutes"));
        }
        Ok(Business {
            name,
            time_zone: self.time_zone,
            currency: self.currency,
            slot_step_minutes: self.slot_step_minutes,
            min_notice_minutes: self.min_notice_minutes,
            max_advance_days: self.max_advance_days,
            hold_minutes: self.hold_minutes,
        })
    }
}

/// How much of the price is taken up front.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Deposit {
    None,
    /// A fixed amount in minor units, `1..=price`.
    Fixed {
        amount_minor: i64,
    },
    /// A percentage of the price, `1..=100`, rounded half up to the minor unit.
    Percent {
        percent: i64,
    },
}

impl Deposit {
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Fixed { .. } => "fixed",
            Self::Percent { .. } => "percent",
        }
    }

    #[must_use]
    pub const fn value(self) -> i64 {
        match self {
            Self::None => 0,
            Self::Fixed { amount_minor } => amount_minor,
            Self::Percent { percent } => percent,
        }
    }

    #[must_use]
    pub fn from_stored(kind: &str, value: i64) -> Option<Self> {
        match kind {
            "none" => Some(Self::None),
            "fixed" => Some(Self::Fixed {
                amount_minor: value,
            }),
            "percent" => Some(Self::Percent { percent: value }),
            _ => None,
        }
    }

    /// The deposit on `price_minor`, in minor units. Integer arithmetic only; the price is
    /// at most 10^8, so `price × 100` cannot overflow.
    #[must_use]
    pub const fn amount(self, price_minor: i64) -> i64 {
        match self {
            Self::None => 0,
            Self::Fixed { amount_minor } => amount_minor,
            Self::Percent { percent } => (price_minor * percent + 50) / 100,
        }
    }
}

/// Highest price, in minor units.
pub const MAX_PRICE_MINOR: i64 = 100_000_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceInput {
    pub name: String,
    pub duration_minutes: u16,
    #[serde(default)]
    pub buffer_before_minutes: u16,
    #[serde(default)]
    pub buffer_after_minutes: u16,
    pub price_minor: i64,
    pub deposit: Deposit,
    #[serde(default = "yes")]
    pub active: bool,
}

const fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub name: String,
    pub duration_minutes: u16,
    pub buffer_before_minutes: u16,
    pub buffer_after_minutes: u16,
    pub price_minor: i64,
    pub deposit: Deposit,
    pub active: bool,
}

impl ServiceInput {
    /// # Errors
    /// The first rule broken.
    pub fn validate(self) -> Result<Service, Invalid> {
        let name = name(&self.name, "invalid_name")?;
        if !(5..=availability::MAX_DURATION_MINUTES).contains(&self.duration_minutes)
            || !self.duration_minutes.is_multiple_of(5)
        {
            return Err((
                "invalid_duration",
                "the duration is 5 to 720 minutes, a multiple of 5",
            ));
        }
        for buffer in [self.buffer_before_minutes, self.buffer_after_minutes] {
            if buffer > availability::MAX_BUFFER_MINUTES || !buffer.is_multiple_of(5) {
                return Err((
                    "invalid_buffer",
                    "a buffer is 0 to 240 minutes, a multiple of 5",
                ));
            }
        }
        if !(0..=MAX_PRICE_MINOR).contains(&self.price_minor) {
            return Err(("invalid_price", "the price is 0 to 100000000 minor units"));
        }
        let ok = match self.deposit {
            Deposit::None => true,
            Deposit::Fixed { amount_minor } => (1..=self.price_minor).contains(&amount_minor),
            Deposit::Percent { percent } => (1..=100).contains(&percent),
        };
        if !ok {
            return Err((
                "invalid_deposit",
                "a fixed deposit is 1 to the price; a percentage is 1 to 100",
            ));
        }
        Ok(Service {
            name,
            duration_minutes: self.duration_minutes,
            buffer_before_minutes: self.buffer_before_minutes,
            buffer_after_minutes: self.buffer_after_minutes,
            price_minor: self.price_minor,
            deposit: self.deposit,
            active: self.active,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaffInput {
    pub name: String,
    #[serde(default = "yes")]
    pub active: bool,
}

impl StaffInput {
    /// # Errors
    /// The name rule.
    pub fn validate(self) -> Result<(String, bool), Invalid> {
        Ok((name(&self.name, "invalid_name")?, self.active))
    }
}

/// One weekly range as sent and returned: ISO weekday (1 = Monday), `"HH:MM"` bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeeklyRange {
    pub weekday: i8,
    pub start: String,
    pub end: String,
}

/// Weekly hours: at most [`MAX_WEEKLY_RANGES`], weekdays 1–7, no two overlapping on a day
/// (touching is fine). Sorted by weekday and start.
///
/// # Errors
/// The first rule broken.
pub fn weekly_hours(ranges: &[WeeklyRange]) -> Result<Vec<(i8, Minutes)>, Invalid> {
    if ranges.len() > MAX_WEEKLY_RANGES {
        return Err((
            "too_many_ranges",
            "at most 50 weekly ranges per staff member",
        ));
    }
    let mut out = Vec::with_capacity(ranges.len());
    for r in ranges {
        if !(1..=7).contains(&r.weekday) {
            return Err(("invalid_weekday", "the weekday is 1 (Monday) to 7 (Sunday)"));
        }
        out.push((r.weekday, span(&r.start, &r.end)?));
    }
    out.sort_by_key(|(d, m)| (*d, m.start()));
    if out
        .windows(2)
        .any(|w| w[0].0 == w[1].0 && w[0].1.end() > w[1].1.start())
    {
        return Err(("overlapping_hours", "two ranges on one day overlap"));
    }
    Ok(out)
}

/// An exception as sent and returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExceptionInput {
    /// Absent: the whole business (closed only).
    #[serde(default)]
    pub staff_id: Option<uuid::Uuid>,
    /// `YYYY-MM-DD`, inclusive.
    pub first_date: String,
    pub last_date: String,
    /// `"closed"` or `"open"`.
    pub kind: String,
    /// Both or neither; `open` needs both.
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default)]
    pub end: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptionRule {
    pub staff_id: Option<uuid::Uuid>,
    pub first: Date,
    pub last: Date,
    pub kind: ExceptionKind,
    pub note: Option<String>,
}

/// A civil date, `YYYY-MM-DD`, years 2000–2200.
#[must_use]
pub fn parse_date(raw: &str) -> Option<Date> {
    if raw.len() != 10 {
        return None;
    }
    let date: Date = raw.parse().ok()?;
    (2000..=2200).contains(&date.year()).then_some(date)
}

impl ExceptionInput {
    /// # Errors
    /// The first rule broken.
    pub fn validate(self) -> Result<ExceptionRule, Invalid> {
        let bad_date = (
            "invalid_date",
            "dates are YYYY-MM-DD, first on or before last",
        );
        let first = parse_date(&self.first_date).ok_or(bad_date)?;
        let last = parse_date(&self.last_date).ok_or(bad_date)?;
        let days = first.until(last).map_err(|_| bad_date)?.get_days();
        if last < first || days > MAX_EXCEPTION_SPAN_DAYS {
            return Err((
                "invalid_date",
                "an exception covers 1 to 367 dates, first on or before last",
            ));
        }
        let minutes = match (self.start.as_deref(), self.end.as_deref()) {
            (None, None) => None,
            (Some(s), Some(e)) => Some(span(s, e)?),
            _ => return Err(("invalid_hours", "give both start and end, or neither")),
        };
        let kind = match (self.kind.as_str(), minutes) {
            ("closed", m) => ExceptionKind::Closed(m),
            ("open", Some(m)) if self.staff_id.is_some() => ExceptionKind::Open(m),
            ("open", _) => {
                return Err((
                    "invalid_exception",
                    "an open exception names a staff member and its hours",
                ));
            }
            _ => return Err(("invalid_exception", "the kind is \"closed\" or \"open\"")),
        };
        let note = match self.note {
            Some(n) if n.chars().count() > 200 || n.chars().any(char::is_control) => {
                return Err(("invalid_note", "a note is at most 200 characters"));
            }
            other => other,
        };
        Ok(ExceptionRule {
            staff_id: self.staff_id,
            first,
            last,
            kind,
            note,
        })
    }
}

/// A hold request.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldInput {
    pub service_id: uuid::Uuid,
    pub staff_id: uuid::Uuid,
    /// RFC 3339 instant, e.g. `2026-06-01T09:00:00Z`; whole minutes.
    pub starts_at: String,
    pub customer_name: String,
    #[serde(default)]
    pub customer_email: Option<String>,
    /// 1–64 of `A-Z a-z 0-9 . _ : -`; a retry with the same key and body returns the same hold.
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub service_id: uuid::Uuid,
    pub staff_id: uuid::Uuid,
    pub starts_at: jiff::Timestamp,
    pub customer_name: String,
    pub customer_email: Option<String>,
    pub idempotency_key: String,
}

#[must_use]
pub fn idempotency_key_shaped(raw: &str) -> bool {
    (1..=64).contains(&raw.len())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}

impl HoldInput {
    /// # Errors
    /// The first rule broken.
    pub fn validate(self, now: jiff::Timestamp) -> Result<Hold, Invalid> {
        let starts_at: jiff::Timestamp = (self.starts_at.len() <= 40)
            .then(|| self.starts_at.parse().ok())
            .flatten()
            .ok_or(("invalid_start", "starts_at is an RFC 3339 instant"))?;
        let offset = starts_at.as_second().saturating_sub(now.as_second());
        // LIMIT(hold-start-window)
        if !(-HOLD_START_PAST_SECONDS..=HOLD_START_AHEAD_SECONDS).contains(&offset) {
            return Err((
                "invalid_start",
                "starts_at is within a day before now to 366 days after",
            ));
        }
        if starts_at.as_second() % 60 != 0 || starts_at.subsec_nanosecond() != 0 {
            return Err(("invalid_start", "starts_at is a whole minute"));
        }
        let customer_name = name(&self.customer_name, "invalid_customer")?;
        let customer_email = match self.customer_email {
            None => None,
            Some(raw) => Some(
                booking_accounts::domain::normalize_email(&raw)
                    .ok_or(("invalid_customer", "the customer email is not an address"))?,
            ),
        };
        if !idempotency_key_shaped(&self.idempotency_key) {
            return Err((
                "invalid_idempotency_key",
                "the idempotency key is 1 to 64 of A-Z a-z 0-9 . _ : -",
            ));
        }
        Ok(Hold {
            service_id: self.service_id,
            staff_id: self.staff_id,
            starts_at,
            customer_name,
            customer_email,
            idempotency_key: self.idempotency_key,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_hold_starts_within_a_day_before_now_to_366_days_after() {
        let now: jiff::Timestamp = "2027-06-01T10:00:00Z".parse().unwrap();
        let at = |starts_at: &str| {
            HoldInput {
                service_id: uuid::Uuid::nil(),
                staff_id: uuid::Uuid::nil(),
                starts_at: starts_at.into(),
                customer_name: "C".into(),
                customer_email: None,
                idempotency_key: "k".into(),
            }
            .validate(now)
            .map(|h| h.starts_at.to_string())
            .map_err(|e| e.0)
        };
        assert_eq!(
            at("2027-05-31T10:00:00Z"),
            Ok("2027-05-31T10:00:00Z".into())
        );
        assert_eq!(
            at("2028-06-01T10:00:00Z"),
            Ok("2028-06-01T10:00:00Z".into())
        );
        for out in [
            "2027-05-31T09:59:00Z",
            "2028-06-01T10:01:00Z",
            "0001-01-01T00:00:00Z",
            "9999-12-31T23:59:00Z",
            "-000001-01-01T00:00:00Z",
        ] {
            assert_eq!(at(out), Err("invalid_start"), "{out}");
        }
    }

    use super::*;
    use proptest::prelude::*;

    #[test]
    fn hours_parse_and_format() {
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("09:30"), Some(570));
        assert_eq!(parse_hhmm("24:00"), Some(1440));
        for bad in [
            "24:01", "9:30", "09:60", "0930", "", "+1:00", "09:3a", "99:99",
        ] {
            assert_eq!(parse_hhmm(bad), None, "{bad}");
        }
        assert_eq!(format_hhmm(570), "09:30");
    }

    #[test]
    fn deposits_round_half_up_in_integers() {
        assert_eq!(Deposit::Percent { percent: 25 }.amount(1_002), 251); // 250.5
        assert_eq!(Deposit::Percent { percent: 25 }.amount(1_001), 250); // 250.25
        assert_eq!(
            Deposit::Percent { percent: 100 }.amount(MAX_PRICE_MINOR),
            MAX_PRICE_MINOR
        );
        assert_eq!(Deposit::Fixed { amount_minor: 500 }.amount(4_000), 500);
        assert_eq!(Deposit::None.amount(4_000), 0);
    }

    #[test]
    fn weekly_overlaps_are_refused_touching_is_not() {
        let r = |d, s: &str, e: &str| WeeklyRange {
            weekday: d,
            start: s.into(),
            end: e.into(),
        };
        assert!(weekly_hours(&[r(1, "09:00", "12:00"), r(1, "12:00", "17:00")]).is_ok());
        assert_eq!(
            weekly_hours(&[r(1, "09:00", "12:30"), r(1, "12:00", "17:00")])
                .unwrap_err()
                .0,
            "overlapping_hours"
        );
        assert!(weekly_hours(&[r(1, "09:00", "12:30"), r(2, "12:00", "17:00")]).is_ok());
        assert_eq!(
            weekly_hours(&[r(8, "09:00", "10:00")]).unwrap_err().0,
            "invalid_weekday"
        );
        assert_eq!(
            weekly_hours(&[r(1, "10:00", "10:00")]).unwrap_err().0,
            "invalid_hours"
        );
        let many: Vec<_> = (0..51).map(|_| r(1, "09:00", "10:00")).collect();
        assert_eq!(weekly_hours(&many).unwrap_err().0, "too_many_ranges");
    }

    #[test]
    fn zone_names_are_shaped_like_iana_names() {
        for good in [
            "Europe/London",
            "America/Argentina/Buenos_Aires",
            "UTC",
            "Etc/GMT+5",
        ] {
            assert!(zone_name_shaped(good), "{good}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../../etc/passwd",
            "Europe/London\n",
            "a b",
            &"A".repeat(65),
        ] {
            assert!(!zone_name_shaped(bad), "{bad:?}");
        }
    }

    proptest! {
        #[test]
        fn hhmm_round_trips(m in 0u16..=1440) {
            prop_assert_eq!(parse_hhmm(&format_hhmm(m)), Some(m));
        }

        // Untrusted input: no string panics a parser or a validator.
        #[test]
        fn parsers_never_panic(s in "\\PC{0,48}", t in "\\PC{0,48}") {
            let _ = parse_hhmm(&s);
            let _ = parse_date(&s);
            let _ = zone_name_shaped(&s);
            let _ = idempotency_key_shaped(&s);
            let _ = ExceptionInput {
                staff_id: None, first_date: s.clone(), last_date: t.clone(),
                kind: "closed".into(), start: Some(s.clone()), end: Some(t.clone()), note: None,
            }.validate();
            let _ = HoldInput {
                service_id: uuid::Uuid::nil(), staff_id: uuid::Uuid::nil(), starts_at: s.clone(),
                customer_name: t.clone(), customer_email: Some(s.clone()), idempotency_key: t,
            }.validate(jiff::Timestamp::UNIX_EPOCH);
        }

        #[test]
        fn validated_hours_never_overlap(
            raw in prop::collection::vec((1i8..=7, 0u16..=1440, 0u16..=1440), 0..20)
        ) {
            let ranges: Vec<WeeklyRange> = raw.iter()
                .map(|&(d, s, e)| WeeklyRange { weekday: d, start: format_hhmm(s), end: format_hhmm(e) })
                .collect();
            if let Ok(hours) = weekly_hours(&ranges) {
                for w in hours.windows(2) {
                    prop_assert!(w[0].0 != w[1].0 || w[0].1.end() <= w[1].1.start());
                }
            }
        }
    }
}
