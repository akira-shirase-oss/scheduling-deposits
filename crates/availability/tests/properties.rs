//! The engine against a brute-force oracle over random schedules, zones and dates around
//! DST transitions. The oracle walks every minute of elapsed time and asks the time zone
//! what wall time it is — no offset arithmetic — so it shares no logic with the engine.
#![expect(
    clippy::unwrap_used,
    reason = "strategies and the oracle build fixed, known-good values"
)]

use std::collections::BTreeSet;

use availability::{
    Busy, Exception, ExceptionKind, Minutes, Query, STEPS, Service, Staff, WeeklySpan, slots,
};
use jiff::Timestamp;
use jiff::civil::{Date, date};
use jiff::tz::TimeZone;
use proptest::prelude::*;
use uuid::Uuid;

const ZONES: [&str; 6] = [
    "UTC",
    "Europe/London",
    "America/New_York",
    "Asia/Kolkata",
    "Australia/Lord_Howe",
    "America/Sao_Paulo",
];

/// Dates on or next to transitions in the zones above, and plain dates.
fn dates() -> Vec<Date> {
    vec![
        date(2026, 3, 28),
        date(2026, 3, 29),
        date(2026, 10, 24),
        date(2026, 10, 25),
        date(2026, 3, 7),
        date(2026, 3, 8),
        date(2026, 11, 1),
        date(2026, 4, 4),
        date(2026, 4, 5),
        date(2026, 10, 4),
        date(2026, 6, 15),
        date(2026, 12, 31),
    ]
}

fn minutes() -> impl Strategy<Value = Minutes> {
    (0u16..288, 1u16..96).prop_map(|(s, len)| {
        let start = s * 5;
        Minutes::new(start, (start + len * 5).min(1440)).unwrap()
    })
}

#[derive(Debug, Clone)]
struct Case {
    zone: &'static str,
    query: Query,
    service: Service,
    staff: Vec<Staff>,
    exceptions: Vec<Exception>,
}

fn case() -> impl Strategy<Value = Case> {
    let staff_ids = [Uuid::from_u128(1), Uuid::from_u128(2)];
    (
        prop::sample::select(ZONES.to_vec()),
        prop::sample::select(dates()),
        1u32..=3,
        prop::sample::select(STEPS.to_vec()),
        (1u16..=36, 0u16..=12, 0u16..=12),
        (-3000i64..3000, 0u32..=600, 0u32..=4),
        prop::collection::vec(
            (
                prop::collection::vec((1i8..=7, minutes()), 0..6),
                prop::collection::vec((-2000i64..6000, 1i64..300), 0..5),
            ),
            1..=2,
        ),
        prop::collection::vec(
            (
                0usize..3,
                -1i64..4,
                0i64..2,
                prop::option::of(minutes()),
                any::<bool>(),
            ),
            0..4,
        ),
    )
        .prop_map(move |(zone, from, days, step, svc, clock, members, excs)| {
            let base = TimeZone::get(zone)
                .unwrap()
                .to_timestamp(from.at(0, 0, 0, 0))
                .unwrap()
                .as_second();
            let minute = |m: i64| Timestamp::from_second(base + m * 60).unwrap();
            let staff: Vec<Staff> = members
                .into_iter()
                .enumerate()
                .map(|(i, (weekly, busy))| Staff {
                    id: staff_ids[i],
                    weekly: weekly
                        .into_iter()
                        .map(|(weekday, span)| WeeklySpan { weekday, span })
                        .collect(),
                    busy: busy
                        .into_iter()
                        .map(|(at, len)| Busy {
                            from: minute(at),
                            until: minute(at + len),
                        })
                        .collect(),
                })
                .collect();
            let exceptions = excs
                .into_iter()
                .filter_map(|(who, first, len, span, open)| {
                    let first_date = from.checked_add(jiff::Span::new().days(first)).unwrap();
                    let last = first_date.checked_add(jiff::Span::new().days(len)).unwrap();
                    let staff = staff_ids.get(who).copied();
                    let kind = match (open, span) {
                        (true, Some(m)) if staff.is_some() => ExceptionKind::Open(m),
                        (true, _) => return None,
                        (false, span) => ExceptionKind::Closed(span),
                    };
                    Some(Exception {
                        staff,
                        first: first_date,
                        last,
                        kind,
                    })
                })
                .collect();
            Case {
                zone,
                query: Query {
                    from,
                    days,
                    now: minute(clock.0),
                    min_notice_minutes: clock.1,
                    max_advance_days: clock.2,
                    step_minutes: step,
                },
                service: Service {
                    duration_minutes: svc.0 * 5,
                    buffer_before_minutes: svc.1 * 5,
                    buffer_after_minutes: svc.2 * 5,
                },
                staff,
                exceptions,
            }
        })
}

/// Whether `member` works the wall minute `minute` of `day`, straight from the rules.
fn works(member: &Staff, exceptions: &[Exception], day: Date, minute: u16) -> bool {
    let covering = |e: &&Exception| e.first <= day && day <= e.last;
    let opens: Vec<Minutes> = exceptions
        .iter()
        .filter(covering)
        .filter(|e| e.staff == Some(member.id))
        .filter_map(|e| match e.kind {
            ExceptionKind::Open(m) => Some(m),
            ExceptionKind::Closed(_) => None,
        })
        .collect();
    let base = if opens.is_empty() {
        let weekday = day.weekday().to_monday_one_offset();
        member
            .weekly
            .iter()
            .any(|w| w.weekday == weekday && w.span.contains(minute))
    } else {
        opens.iter().any(|m| m.contains(minute))
    };
    let closed = exceptions.iter().filter(covering).any(|e| {
        e.staff.is_none_or(|s| s == member.id)
            && matches!(e.kind, ExceptionKind::Closed(span) if span.is_none_or(|m| m.contains(minute)))
    });
    base && !closed
}

#[expect(
    clippy::many_single_char_names,
    reason = "interval arithmetic reads best terse"
)]
fn oracle(c: &Case) -> BTreeSet<(i64, Uuid)> {
    let tz = TimeZone::get(c.zone).unwrap();
    let q = &c.query;
    let last = q
        .from
        .checked_add(jiff::Span::new().days(i64::from(q.days)))
        .unwrap();
    let from = tz
        .to_timestamp(q.from.yesterday().unwrap().at(0, 0, 0, 0))
        .unwrap()
        .as_second();
    let until = tz
        .to_timestamp(last.tomorrow().unwrap().at(0, 0, 0, 0))
        .unwrap()
        .as_second();
    let wall = |t: i64| tz.to_datetime(Timestamp::from_second(t).unwrap());
    let dur = i64::from(c.service.duration_minutes) * 60;
    let (bb, ba) = (
        i64::from(c.service.buffer_before_minutes) * 60,
        i64::from(c.service.buffer_after_minutes) * 60,
    );
    let now = q.now.as_second();
    let mut out = BTreeSet::new();
    for member in &c.staff {
        let working = |t: i64| {
            let w = wall(t);
            works(
                member,
                &c.exceptions,
                w.date(),
                u16::try_from(w.hour()).unwrap() * 60 + u16::try_from(w.minute()).unwrap(),
            )
        };
        let mut t = from;
        while t < until {
            let w = wall(t);
            let minute = i64::from(w.hour()) * 60 + i64::from(w.minute());
            let on_grid = w.second() == 0 && minute % i64::from(q.step_minutes) == 0;
            let in_dates = q.from <= w.date() && w.date() < last;
            let in_bounds = t >= now + i64::from(q.min_notice_minutes) * 60
                && t <= now + i64::from(q.max_advance_days) * 86_400;
            if on_grid && in_dates && in_bounds {
                let serviced = (0..dur / 60).all(|m| working(t + m * 60));
                let free = member.busy.iter().all(|b| {
                    let (a, z) = (b.from.as_second(), b.until.as_second());
                    a >= z || z <= t - bb || a >= t + dur + ba
                });
                if serviced && free {
                    out.insert((t, member.id));
                }
            }
            t += 60;
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn the_engine_matches_the_oracle(c in case()) {
        let tz = TimeZone::get(c.zone).unwrap();
        let found = slots(&tz, &c.staff, &c.exceptions, c.service, &c.query).unwrap();
        let dur = i64::from(c.service.duration_minutes) * 60;
        for s in &found {
            prop_assert_eq!(s.end.as_second() - s.start.as_second(), dur);
        }
        let got: Vec<(i64, Uuid)> = found.iter().map(|s| (s.start.as_second(), s.staff)).collect();
        let mut sorted = got.clone();
        sorted.sort_unstable();
        prop_assert_eq!(&got, &sorted, "ordered by start then staff");
        let got: BTreeSet<_> = got.into_iter().collect();
        prop_assert_eq!(got.len(), found.len(), "no duplicates");
        prop_assert_eq!(got, oracle(&c));
    }

    #[test]
    fn the_engine_is_deterministic(c in case()) {
        let tz = TimeZone::get(c.zone).unwrap();
        let once = slots(&tz, &c.staff, &c.exceptions, c.service, &c.query).unwrap();
        let mut reversed = c.staff.clone();
        reversed.reverse();
        let twice = slots(&tz, &reversed, &c.exceptions, c.service, &c.query).unwrap();
        prop_assert_eq!(once, twice);
    }
}
