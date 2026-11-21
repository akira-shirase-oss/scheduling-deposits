//! The availability engine: bookable slots from a business's working hours, exceptions,
//! staff, a service's duration and buffers, and the ranges already taken (bookings and
//! live holds).
//!
//! Pure and deterministic: no clock, no I/O, no global state. The caller passes `now`,
//! the time zone and the busy ranges; the same input always gives the same slots.
//!
//! # Semantics
//!
//! * Working hours are civil (wall-clock) minutes of a day in the business's zone. A
//!   minute of the day that does not exist (the spring-forward gap) is not worked; a
//!   minute that exists twice (the fall-back fold) is worked twice — an hour of working
//!   time is an hour of elapsed time.
//! * The service itself, `[start, start + duration)`, must lie inside working time
//!   (contiguous, possibly across a time-zone transition). Its buffers need not: buffers
//!   are the staff member's turnaround, not customer time.
//! * The blocked range `[start - buffer_before, start + duration + buffer_after)` must not
//!   overlap any busy range. This is exactly the range the database guards.
//! * Candidate starts sit on a civil grid: minute-of-day multiples of `step`, counted from
//!   local midnight, each mapped to the instants at which that wall time occurs. A grid
//!   time in a gap never occurs; one in a fold occurs twice and yields two slots.
//! * A slot belongs to the civil date of its start.
//! * Exceptions: an `Open` exception for a staff member replaces that member's weekly
//!   hours on the dates it covers; `Closed` exceptions (for one member or for everyone)
//!   are then subtracted, whole day or a minute range.

use std::cmp::Ordering;

use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use uuid::Uuid;

const DAY: i64 = 86_400;
/// Wider than any UTC offset (±26 h), so the instant window covers every civil instant
/// of the requested dates and of the days either side.
const SLACK: i64 = 2 * DAY;

/// Longest range one query may cover. LIMIT(availability-days)
pub const MAX_DAYS: u32 = 62;
/// Most staff one query may consider. LIMIT(availability-staff)
pub const MAX_STAFF: usize = 1_000;
/// Grid steps the engine accepts (each divides a day).
pub const STEPS: [u16; 6] = [5, 10, 15, 20, 30, 60];
/// Longest service, and longest single buffer, in minutes.
pub const MAX_DURATION_MINUTES: u16 = 720;
/// Longest buffer, in minutes.
pub const MAX_BUFFER_MINUTES: u16 = 240;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("the range must cover 1 to {MAX_DAYS} days")]
    Range,
    #[error("at most {MAX_STAFF} staff per query")]
    TooManyStaff,
    #[error("the step must be one of 5, 10, 15, 20, 30 or 60 minutes")]
    Step,
    #[error("the service duration or a buffer is out of range")]
    Service,
    #[error("the read needs more work than its budget")]
    TooMuchWork,
    #[error("the dates are outside the supported range")]
    OutOfRange,
}

/// A range of minutes of the civil day, `[start, end)`, `0 <= start < end <= 1440`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Minutes {
    start: u16,
    end: u16,
}

impl Minutes {
    /// `None` unless `start < end <= 1440`.
    #[must_use]
    pub fn new(start: u16, end: u16) -> Option<Self> {
        (start < end && end <= 1440).then_some(Self { start, end })
    }

    #[must_use]
    pub fn start(self) -> u16 {
        self.start
    }

    #[must_use]
    pub fn end(self) -> u16 {
        self.end
    }

    #[must_use]
    pub fn contains(self, minute: u16) -> bool {
        self.start <= minute && minute < self.end
    }
}

/// One range of weekly working hours. `weekday` is ISO: 1 = Monday … 7 = Sunday.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeeklySpan {
    pub weekday: i8,
    pub span: Minutes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionKind {
    /// Not working: the whole day (`None`) or a range of it.
    Closed(Option<Minutes>),
    /// Working these minutes instead of the weekly hours (staff-specific only).
    Open(Minutes),
}

/// An exception to the weekly hours over the civil dates `first..=last`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exception {
    /// `None`: applies to every staff member (a `Closed` business holiday).
    pub staff: Option<Uuid>,
    pub first: Date,
    pub last: Date,
    pub kind: ExceptionKind,
}

/// A range already taken, `[from, until)`: a confirmed booking or a live hold, buffers
/// included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy {
    pub from: Timestamp,
    pub until: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staff {
    pub id: Uuid,
    pub weekly: Vec<WeeklySpan>,
    pub busy: Vec<Busy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Service {
    pub duration_minutes: u16,
    pub buffer_before_minutes: u16,
    pub buffer_after_minutes: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Query {
    /// First civil date, in the business's zone.
    pub from: Date,
    /// Number of dates, `1..=MAX_DAYS`.
    pub days: u32,
    pub now: Timestamp,
    /// A slot starts no earlier than `now + min_notice_minutes`.
    pub min_notice_minutes: u32,
    /// A slot starts no later than `now + max_advance_days × 24 h`.
    pub max_advance_days: u32,
    /// Grid step in minutes; one of [`STEPS`].
    pub step_minutes: u16,
}

/// A bookable slot: the service runs `[start, end)`; the blocked range adds the buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot {
    pub start: Timestamp,
    pub end: Timestamp,
    pub staff: Uuid,
}

/// Every bookable slot for `service` among `staff` on the query's dates, ordered by start
/// then staff id.
///
/// # Errors
///
/// [`Error`] on a query outside the engine's bounds; never on any schedule content.
pub fn slots(
    tz: &TimeZone,
    staff: &[Staff],
    exceptions: &[Exception],
    service: Service,
    query: &Query,
) -> Result<Vec<Slot>, Error> {
    slots_after(tz, staff, exceptions, service, query, None, usize::MAX)
}

/// Grid points a read walks: `staff × days × points a day`. A caller budgets work by it
/// before it calls [`slots_after`].
#[must_use]
pub fn cells(staff: usize, days: u32, step_minutes: u16) -> u64 {
    let per_day = 1440 / u64::from(step_minutes.max(1));
    u64::try_from(staff)
        .unwrap_or(u64::MAX)
        .saturating_mul(u64::from(days))
        .saturating_mul(per_day)
}

/// The first `limit` slots of [`slots`] that start after `after`, in the same order (start,
/// then staff id). The walk stops at `limit`; it never builds the whole answer.
///
/// # Errors
///
/// As [`slots`].
pub fn slots_after(
    tz: &TimeZone,
    staff: &[Staff],
    exceptions: &[Exception],
    service: Service,
    query: &Query,
    after: Option<Timestamp>,
    limit: usize,
) -> Result<Vec<Slot>, Error> {
    slots_within(
        tz,
        staff,
        exceptions,
        service,
        query,
        after,
        limit,
        u64::MAX,
    )
}

/// [`slots_after`] with a work budget: every step of the read — each civil date, each
/// exception's covered dates, each weekly range, each working or busy range, each grid
/// point and each (start, member) pair walked — costs one unit, and the read stops with
/// [`Error::TooMuchWork`] as soon as it has spent `budget`. A read's work is therefore
/// bounded by its budget whatever the schedule holds.
///
/// # Errors
///
/// As [`slots`], and [`Error::TooMuchWork`] past the budget.
#[allow(
    clippy::too_many_arguments,
    reason = "the engine's inputs, one per argument like `slots_after`"
)]
pub fn slots_within(
    tz: &TimeZone,
    staff: &[Staff],
    exceptions: &[Exception],
    service: Service,
    query: &Query,
    after: Option<Timestamp>,
    limit: usize,
    budget: u64,
) -> Result<Vec<Slot>, Error> {
    if query.days == 0 || query.days > MAX_DAYS {
        return Err(Error::Range);
    }
    if staff.len() > MAX_STAFF {
        return Err(Error::TooManyStaff);
    }
    if !STEPS.contains(&query.step_minutes) {
        return Err(Error::Step);
    }
    if service.duration_minutes == 0
        || service.duration_minutes > MAX_DURATION_MINUTES
        || service.buffer_before_minutes > MAX_BUFFER_MINUTES
        || service.buffer_after_minutes > MAX_BUFFER_MINUTES
    {
        return Err(Error::Service);
    }
    let mut meter = Meter { left: budget };
    let window = Window::new(tz, query)?;
    meter.charge(window.dates.len() + window.pieces.len())?;
    let rules = window.rules(exceptions, &mut meter)?;
    let duration = i64::from(service.duration_minutes) * 60;
    let before = i64::from(service.buffer_before_minutes) * 60;
    let after_buffer = i64::from(service.buffer_after_minutes) * 60;
    let earliest = query
        .now
        .as_second()
        .saturating_add(i64::from(query.min_notice_minutes) * 60);
    let latest = query
        .now
        .as_second()
        .saturating_add(i64::from(query.max_advance_days) * DAY);

    let mut members: Vec<(Uuid, Ranges, Ranges)> = Vec::with_capacity(staff.len());
    for member in staff {
        let hours = window.day_hours(member, &rules, &mut meter)?;
        let working = window.working(&hours, &mut meter)?;
        meter.charge(member.busy.len())?; // LIMIT(busy-rows)
        let busy = merged(
            member
                .busy
                .iter()
                .map(|b| (b.from.as_second(), b.until.as_second()))
                .filter(|(a, b)| a < b)
                .collect(),
        );
        members.push((member.id, working, busy));
    }
    members.sort_unstable_by_key(|m| m.0);
    // One grid for every member, walked in start order: the answer comes out sorted, and
    // generation stops at `limit`.
    let grid = window.grid(query.step_minutes);
    meter.charge(grid.len())?;
    let skip = after.map_or(0, |a| grid.partition_point(|&g| g <= a.as_second()));
    let mut out = Vec::new();
    for &start in &grid[skip..] {
        if start > latest {
            break;
        }
        if start < earliest {
            continue;
        }
        meter.charge(members.len())?;
        let end = start + duration;
        for (id, working, busy) in &members {
            if !inside(working, start, end) || overlaps(busy, start - before, end + after_buffer) {
                continue;
            }
            if out.len() == limit {
                return Ok(out);
            }
            out.push(Slot {
                start: ts(start)?,
                end: ts(end)?,
                staff: *id,
            });
        }
    }
    Ok(out)
}

/// Units of work left to a read.
struct Meter {
    left: u64,
}

impl Meter {
    fn charge(&mut self, units: usize) -> Result<(), Error> {
        let units = u64::try_from(units).unwrap_or(u64::MAX);
        // LIMIT(availability-work)
        self.left = self.left.checked_sub(units).ok_or(Error::TooMuchWork)?;
        Ok(())
    }
}

/// The exceptions of a window, bucketed once by civil date: the business-wide closures of
/// each date (merged minute ranges; `None` for the whole day) and each member's own
/// exceptions of each date.
struct Rules {
    everyone: Vec<Option<Vec<(u16, u16)>>>,
    own: std::collections::HashMap<Uuid, Vec<Vec<ExceptionKind>>>,
}

/// Merged `[from, until)` ranges in seconds.
type Ranges = Vec<(i64, i64)>;

/// The civil days of a query and the offset pieces of the instants around them.
struct Window {
    /// Days since 1970-01-01 of `query.from`.
    first_day: i64,
    days: i64,
    /// `dates[k]` is the civil date `first_day - 1 + k`, for `k` in `0..days + 2`.
    dates: Vec<Date>,
    /// Instant ranges `[from, until)` of constant UTC offset (seconds) covering the window.
    pieces: Vec<(i64, i64, i64)>,
}

impl Window {
    fn new(tz: &TimeZone, query: &Query) -> Result<Self, Error> {
        let first_day = days_from_civil(query.from);
        let days = i64::from(query.days);
        let mut dates = Vec::with_capacity(usize::try_from(days + 2).map_err(|_| Error::Range)?);
        let mut date = query.from.yesterday().map_err(|_| Error::OutOfRange)?;
        for _ in 0..days + 2 {
            dates.push(date);
            date = date.tomorrow().map_err(|_| Error::OutOfRange)?;
        }
        let from = (first_day - 1) * DAY - SLACK;
        let until = (first_day + days + 1) * DAY + SLACK;
        let mut pieces = Vec::new();
        let mut cursor = from;
        for transition in tz.following(ts(from)?) {
            let at = transition.timestamp().as_second();
            if at >= until {
                break;
            }
            if at > cursor {
                pieces.push((cursor, at, offset(tz, cursor)?));
                cursor = at;
            }
        }
        pieces.push((cursor, until, offset(tz, cursor)?));
        Ok(Self {
            first_day,
            days,
            dates,
            pieces,
        })
    }

    /// The window's dates an exception covers, as indexes into `dates`.
    fn covered(&self, e: &Exception) -> std::ops::Range<usize> {
        let base = self.first_day - 1;
        let len = i64::try_from(self.dates.len()).unwrap_or(i64::MAX);
        let lo = (days_from_civil(e.first) - base).clamp(0, len);
        let hi = (days_from_civil(e.last) - base + 1).clamp(lo, len);
        usize::try_from(lo).unwrap_or(0)..usize::try_from(hi).unwrap_or(0)
    }

    /// Buckets `exceptions` by date once; each exception costs the dates it covers.
    fn rules(&self, exceptions: &[Exception], meter: &mut Meter) -> Result<Rules, Error> {
        let n = self.dates.len();
        let mut closed: Vec<Vec<(u16, u16)>> = vec![Vec::new(); n];
        let mut whole = vec![false; n];
        let mut own: std::collections::HashMap<Uuid, Vec<Vec<ExceptionKind>>> =
            std::collections::HashMap::new();
        for e in exceptions {
            let dates = self.covered(e);
            meter.charge(dates.len().max(1))?;
            match (e.staff, e.kind) {
                (None, ExceptionKind::Closed(None)) => whole[dates].fill(true),
                (None, ExceptionKind::Closed(Some(m))) => {
                    for k in dates {
                        closed[k].push((m.start, m.end));
                    }
                }
                // An open exception is staff-specific; one for everyone has no meaning.
                (None, ExceptionKind::Open(_)) => {}
                (Some(id), kind) => {
                    let days = own.entry(id).or_insert_with(|| vec![Vec::new(); n]);
                    for k in dates {
                        days[k].push(kind);
                    }
                }
            }
        }
        let everyone = closed
            .into_iter()
            .zip(whole)
            .map(|(ranges, whole)| (!whole).then(|| merged(ranges)))
            .collect();
        Ok(Rules { everyone, own })
    }

    /// The working minutes of `member` on each civil date of the window, sorted and merged.
    fn day_hours(
        &self,
        member: &Staff,
        rules: &Rules,
        meter: &mut Meter,
    ) -> Result<Vec<Vec<(u16, u16)>>, Error> {
        meter.charge(member.weekly.len())?;
        let mut weekly: [Vec<(u16, u16)>; 7] = Default::default();
        for w in &member.weekly {
            if let Some(day) = usize::try_from(w.weekday - 1)
                .ok()
                .and_then(|d| weekly.get_mut(d))
            {
                day.push((w.span.start, w.span.end));
            }
        }
        let own = rules.own.get(&member.id);
        let mut out = Vec::with_capacity(self.dates.len());
        for (k, &date) in self.dates.iter().enumerate() {
            let mine: &[ExceptionKind] = own.and_then(|d| d.get(k)).map_or(&[], Vec::as_slice);
            let opens: Vec<(u16, u16)> = mine
                .iter()
                .filter_map(|kind| match kind {
                    ExceptionKind::Open(m) => Some((m.start, m.end)),
                    ExceptionKind::Closed(_) => None,
                })
                .collect();
            let weekday = usize::try_from(date.weekday().to_monday_one_offset() - 1).unwrap_or(0);
            let spans = if opens.is_empty() {
                weekly.get(weekday).cloned().unwrap_or_default()
            } else {
                opens
            };
            meter.charge(1 + mine.len() + spans.len())?;
            // Closures commute: the member's own and the business's, subtracted in one pass.
            let mut closed = Vec::new();
            let mut whole = false;
            for kind in mine {
                match *kind {
                    ExceptionKind::Closed(None) => whole = true,
                    ExceptionKind::Closed(Some(m)) => closed.push((m.start, m.end)),
                    ExceptionKind::Open(_) => {}
                }
            }
            match rules.everyone.get(k) {
                Some(Some(everyone)) => closed.extend_from_slice(everyone),
                _ => whole = true,
            }
            let spans = if whole {
                Vec::new()
            } else {
                meter.charge(closed.len())?;
                subtract_all(&merged(spans), &merged(closed))
            };
            out.push(merged(spans));
        }
        Ok(out)
    }

    /// Working time as merged instant ranges (seconds).
    fn working(&self, hours: &[Vec<(u16, u16)>], meter: &mut Meter) -> Result<Ranges, Error> {
        let mut ranges = Vec::new();
        for &(from, until, offset) in &self.pieces {
            let (civil_from, civil_until) = (from + offset, until + offset);
            meter.charge(hours.len())?;
            for (day, spans) in (self.first_day - 1..).zip(hours) {
                let day_start = day * DAY;
                if day_start + DAY <= civil_from || day_start >= civil_until {
                    continue;
                }
                meter.charge(spans.len())?;
                for &(s, e) in spans {
                    let a = (day_start + i64::from(s) * 60).max(civil_from);
                    let b = (day_start + i64::from(e) * 60).min(civil_until);
                    if a < b {
                        ranges.push((a - offset, b - offset));
                    }
                }
            }
        }
        Ok(merged(ranges))
    }

    /// Every instant at which a grid wall time of a requested date occurs, ascending.
    fn grid(&self, step_minutes: u16) -> Vec<i64> {
        let step = i64::from(step_minutes) * 60;
        let mut out = Vec::new();
        for &(from, until, offset) in &self.pieces {
            let (civil_from, civil_until) = (from + offset, until + offset);
            for day in self.first_day..self.first_day + self.days {
                let day_start = day * DAY;
                let lo = civil_from.max(day_start);
                let hi = civil_until.min(day_start + DAY);
                let mut g = day_start + (lo - day_start + step - 1).div_euclid(step) * step;
                while g < hi {
                    out.push(g - offset);
                    g += step;
                }
            }
        }
        out.sort_unstable();
        out
    }
}

fn ts(second: i64) -> Result<Timestamp, Error> {
    Timestamp::from_second(second).map_err(|_| Error::OutOfRange)
}

fn offset(tz: &TimeZone, second: i64) -> Result<i64, Error> {
    Ok(i64::from(tz.to_offset(ts(second)?).seconds()))
}

/// Days from 1970-01-01 to `date` (proleptic Gregorian).
fn days_from_civil(date: Date) -> i64 {
    let (y, m, d) = (
        i64::from(date.year()),
        i64::from(date.month()),
        i64::from(date.day()),
    );
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Sorted, with overlapping or touching ranges joined.
fn merged<T: Ord + Copy>(mut ranges: Vec<(T, T)>) -> Vec<(T, T)> {
    ranges.retain(|(a, b)| a < b);
    ranges.sort_unstable();
    let mut out: Vec<(T, T)> = Vec::with_capacity(ranges.len());
    for (a, b) in ranges {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// `spans` minus every range of `closed`; both sorted and merged, one pass over each.
fn subtract_all(spans: &[(u16, u16)], closed: &[(u16, u16)]) -> Vec<(u16, u16)> {
    let mut out = Vec::with_capacity(spans.len() + closed.len());
    let mut j = 0;
    for &(a, b) in spans {
        let mut cursor = a;
        while j < closed.len() && closed[j].1 <= cursor {
            j += 1;
        }
        let mut i = j;
        while i < closed.len() && closed[i].0 < b {
            if closed[i].0 > cursor {
                out.push((cursor, closed[i].0));
            }
            cursor = cursor.max(closed[i].1);
            i += 1;
        }
        if cursor < b {
            out.push((cursor, b));
        }
    }
    out
}

/// Whether `[from, until)` lies inside one of the merged `ranges`.
fn inside(ranges: &[(i64, i64)], from: i64, until: i64) -> bool {
    let i = ranges.partition_point(|&(a, _)| a <= from);
    i > 0 && ranges[i - 1].1 >= until
}

/// Whether `[from, until)` overlaps any of the merged `ranges` (half-open, so touching is
/// not overlapping — the same rule as PostgreSQL's `tstzrange &&`).
fn overlaps(ranges: &[(i64, i64)], from: i64, until: i64) -> bool {
    let i = ranges.partition_point(|&(_, b)| b <= from);
    ranges
        .get(i)
        .is_some_and(|&(a, _)| a.cmp(&until) == Ordering::Less)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_from_civil_matches_known_dates() {
        for (date, days) in [
            (jiff::civil::date(1970, 1, 1), 0),
            (jiff::civil::date(2000, 3, 1), 11_017),
            (jiff::civil::date(2026, 3, 29), 20_541),
            (jiff::civil::date(1969, 12, 31), -1),
        ] {
            assert_eq!(days_from_civil(date), days, "{date}");
        }
    }

    #[test]
    fn interval_helpers() {
        assert_eq!(
            merged(vec![(5, 7), (1, 3), (3, 4), (6, 9)]),
            vec![(1, 4), (5, 9)]
        );
        assert_eq!(subtract_all(&[(0, 10)], &[(3, 5)]), vec![(0, 3), (5, 10)]);
        assert_eq!(subtract_all(&[(0, 10)], &[(0, 10)]), vec![]);
        assert_eq!(
            subtract_all(&[(0, 10), (20, 30)], &[(5, 8), (9, 22), (25, 26), (40, 50)]),
            vec![(0, 5), (8, 9), (22, 25), (26, 30)]
        );
        let r = [(0, 10), (20, 30)];
        assert!(
            inside(&r, 0, 10) && inside(&r, 22, 30) && !inside(&r, 5, 21) && !inside(&r, -1, 3)
        );
        assert!(
            !overlaps(&r, 10, 20)
                && overlaps(&r, 9, 11)
                && overlaps(&r, 19, 21)
                && !overlaps(&r, 30, 40)
        );
    }
}
