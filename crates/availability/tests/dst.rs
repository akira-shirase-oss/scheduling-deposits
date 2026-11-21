//! Explicit time-zone transition cases: the spring-forward gap and the fall-back fold.
#![expect(
    clippy::unwrap_used,
    reason = "test helpers build fixed, known-good fixtures"
)]

use availability::{
    Busy, Exception, ExceptionKind, Minutes, Query, Service, Slot, Staff, WeeklySpan, slots,
};
use jiff::Timestamp;
use jiff::civil::{Date, date};
use jiff::tz::TimeZone;
use uuid::Uuid;

const STAFF: Uuid = Uuid::from_u128(1);

fn every_day(start: u16, end: u16) -> Vec<WeeklySpan> {
    (1..=7)
        .map(|weekday| WeeklySpan {
            weekday,
            span: Minutes::new(start, end).unwrap(),
        })
        .collect()
}

fn query(from: Date, step: u16) -> Query {
    Query {
        from,
        days: 1,
        now: "2000-01-01T00:00:00Z".parse().unwrap(),
        min_notice_minutes: 0,
        max_advance_days: 100_000,
        step_minutes: step,
    }
}

fn service(duration: u16, before: u16, after: u16) -> Service {
    Service {
        duration_minutes: duration,
        buffer_before_minutes: before,
        buffer_after_minutes: after,
    }
}

fn starts(found: &[Slot]) -> Vec<String> {
    found.iter().map(|s| s.start.to_string()).collect()
}

fn run(zone: &str, day: Date, weekly: Vec<WeeklySpan>, svc: Service, step: u16) -> Vec<Slot> {
    let tz = TimeZone::get(zone).unwrap();
    let staff = [Staff {
        id: STAFF,
        weekly,
        busy: vec![],
    }];
    slots(&tz, &staff, &[], svc, &query(day, step)).unwrap()
}

#[test]
fn spring_forward_gap_has_no_slots_and_hours_shrink() {
    // Europe/London, 2026-03-29: 01:00 GMT jumps to 02:00 BST. Hours 00:00–03:00 are two
    // elapsed hours; 01:00 and 01:30 do not exist.
    let found = run(
        "Europe/London",
        date(2026, 3, 29),
        every_day(0, 180),
        service(30, 0, 0),
        30,
    );
    assert_eq!(
        starts(&found),
        [
            "2026-03-29T00:00:00Z", // 00:00 GMT
            "2026-03-29T00:30:00Z", // 00:30 GMT
            "2026-03-29T01:00:00Z", // 02:00 BST
            "2026-03-29T01:30:00Z", // 02:30 BST
        ]
    );
}

#[test]
fn a_service_runs_contiguously_across_the_gap() {
    // 00:30 GMT + 60 min = 02:30 BST: one elapsed hour, all of it working time.
    let found = run(
        "Europe/London",
        date(2026, 3, 29),
        every_day(0, 180),
        service(60, 0, 0),
        30,
    );
    assert_eq!(
        starts(&found),
        [
            "2026-03-29T00:00:00Z",
            "2026-03-29T00:30:00Z",
            "2026-03-29T01:00:00Z"
        ]
    );
}

#[test]
fn fall_back_fold_yields_both_occurrences() {
    // Europe/London, 2026-10-25: 02:00 BST falls back to 01:00 GMT. 01:00 and 01:30 occur
    // twice; hours 00:00–03:00 are four elapsed hours.
    let found = run(
        "Europe/London",
        date(2026, 10, 25),
        every_day(0, 180),
        service(30, 0, 0),
        30,
    );
    assert_eq!(
        starts(&found),
        [
            "2026-10-24T23:00:00Z", // 00:00 BST
            "2026-10-24T23:30:00Z", // 00:30 BST
            "2026-10-25T00:00:00Z", // 01:00 BST
            "2026-10-25T00:30:00Z", // 01:30 BST
            "2026-10-25T01:00:00Z", // 01:00 GMT
            "2026-10-25T01:30:00Z", // 01:30 GMT
            "2026-10-25T02:00:00Z", // 02:00 GMT
            "2026-10-25T02:30:00Z", // 02:30 GMT
        ]
    );
}

#[test]
fn new_york_transitions() {
    // 2026-03-08 02:00 EST → 03:00 EDT; hours 01:00–04:00, hourly step.
    let gap = run(
        "America/New_York",
        date(2026, 3, 8),
        every_day(60, 240),
        service(60, 0, 0),
        60,
    );
    assert_eq!(
        starts(&gap),
        ["2026-03-08T06:00:00Z", "2026-03-08T07:00:00Z"]
    );
    // 2026-11-01 02:00 EDT → 01:00 EST; 01:00 occurs twice.
    let fold = run(
        "America/New_York",
        date(2026, 11, 1),
        every_day(60, 180),
        service(60, 0, 0),
        60,
    );
    assert_eq!(
        starts(&fold),
        [
            "2026-11-01T05:00:00Z",
            "2026-11-01T06:00:00Z",
            "2026-11-01T07:00:00Z"
        ]
    );
}

#[test]
fn a_half_hour_transition_zone() {
    // Australia/Lord_Howe moves by 30 minutes: 2026-04-05 02:00 (+11) → 01:30 (+10:30).
    let found = run(
        "Australia/Lord_Howe",
        date(2026, 4, 5),
        every_day(60, 150),
        service(30, 0, 0),
        30,
    );
    assert_eq!(
        starts(&found),
        [
            "2026-04-04T14:00:00Z", // 01:00 +11
            "2026-04-04T14:30:00Z", // 01:30 +11
            "2026-04-04T15:00:00Z", // 01:30 +10:30
            "2026-04-04T15:30:00Z", // 02:00 +10:30
        ]
    );
}

#[test]
fn the_same_wall_hours_differ_by_zone() {
    let day = date(2026, 6, 1);
    let london = run(
        "Europe/London",
        day,
        every_day(540, 600),
        service(60, 0, 0),
        60,
    );
    let kolkata = run(
        "Asia/Kolkata",
        day,
        every_day(540, 600),
        service(60, 0, 0),
        60,
    );
    assert_eq!(starts(&london), ["2026-06-01T08:00:00Z"]);
    assert_eq!(starts(&kolkata), ["2026-06-01T03:30:00Z"]);
}

#[test]
fn buffers_block_but_need_not_be_working_time() {
    let tz = TimeZone::get("UTC").unwrap();
    let busy: Timestamp = "2026-06-01T10:00:00Z".parse().unwrap();
    let staff = [Staff {
        id: STAFF,
        weekly: every_day(540, 720),
        busy: vec![Busy {
            from: busy,
            until: "2026-06-01T10:30:00Z".parse().unwrap(),
        }],
    }];
    let found = slots(
        &tz,
        &staff,
        &[],
        service(30, 15, 15),
        &query(date(2026, 6, 1), 15),
    )
    .unwrap();
    // 09:00 is bookable although its before-buffer starts at 08:45, outside hours; 09:30's
    // after-buffer reaches 10:15 and so overlaps the busy range; 10:45 is the first start
    // whose before-buffer clears 10:30.
    assert_eq!(
        starts(&found),
        [
            "2026-06-01T09:00:00Z",
            "2026-06-01T09:15:00Z",
            "2026-06-01T10:45:00Z",
            "2026-06-01T11:00:00Z",
            "2026-06-01T11:15:00Z",
            "2026-06-01T11:30:00Z",
        ]
    );
}

#[test]
fn exceptions_replace_and_subtract() {
    let tz = TimeZone::get("UTC").unwrap();
    let day = date(2026, 6, 1);
    let staff = [Staff {
        id: STAFF,
        weekly: every_day(540, 600),
        busy: vec![],
    }];
    let open = Exception {
        staff: Some(STAFF),
        first: day,
        last: day,
        kind: ExceptionKind::Open(Minutes::new(780, 900).unwrap()),
    };
    let lunch = Exception {
        staff: None,
        first: day,
        last: day,
        kind: ExceptionKind::Closed(Some(Minutes::new(810, 840).unwrap())),
    };
    let found = slots(
        &tz,
        &staff,
        &[open, lunch],
        service(30, 0, 0),
        &query(day, 30),
    )
    .unwrap();
    assert_eq!(
        starts(&found),
        [
            "2026-06-01T13:00:00Z",
            "2026-06-01T14:00:00Z",
            "2026-06-01T14:30:00Z"
        ]
    );
    let holiday = Exception {
        staff: None,
        first: day,
        last: day,
        kind: ExceptionKind::Closed(None),
    };
    assert_eq!(
        slots(
            &tz,
            &staff,
            &[open, holiday],
            service(30, 0, 0),
            &query(day, 30)
        )
        .unwrap(),
        [] as [availability::Slot; 0]
    );
}

#[test]
fn notice_and_advance_bound_the_starts() {
    let tz = TimeZone::get("UTC").unwrap();
    let staff = [Staff {
        id: STAFF,
        weekly: every_day(540, 720),
        busy: vec![],
    }];
    let mut q = query(date(2026, 6, 1), 60);
    q.now = "2026-06-01T08:00:00Z".parse().unwrap();
    q.min_notice_minutes = 120;
    let found = slots(&tz, &staff, &[], service(60, 0, 0), &q).unwrap();
    assert_eq!(
        starts(&found),
        ["2026-06-01T10:00:00Z", "2026-06-01T11:00:00Z"]
    );
    q.max_advance_days = 0;
    assert_eq!(
        slots(&tz, &staff, &[], service(60, 0, 0), &q).unwrap(),
        [] as [availability::Slot; 0]
    );
}

#[test]
fn queries_out_of_bounds_are_refused() {
    let tz = TimeZone::get("UTC").unwrap();
    let mut q = query(date(2026, 6, 1), 60);
    q.days = 0;
    assert_eq!(
        slots(&tz, &[], &[], service(60, 0, 0), &q),
        Err(availability::Error::Range)
    );
    // Literals, not the constant: the limit's proof moves the constant.
    q.days = 63;
    assert_eq!(
        slots(&tz, &[], &[], service(60, 0, 0), &q),
        Err(availability::Error::Range)
    );
    q.days = 62;
    assert!(slots(&tz, &[], &[], service(60, 0, 0), &q).is_ok());
    q.days = 1;
    q.step_minutes = 7;
    assert_eq!(
        slots(&tz, &[], &[], service(60, 0, 0), &q),
        Err(availability::Error::Step)
    );
    q.step_minutes = 60;
    assert_eq!(
        slots(&tz, &[], &[], service(0, 0, 0), &q),
        Err(availability::Error::Service)
    );
    assert_eq!(
        slots(&tz, &[], &[], service(60, 241, 0), &q),
        Err(availability::Error::Service)
    );
}

#[test]
fn too_many_staff_are_refused() {
    let tz = TimeZone::get("UTC").unwrap();
    let crowd: Vec<Staff> = (0..1_001_usize)
        .map(|i| Staff {
            id: Uuid::from_u128(u128::try_from(i).unwrap()),
            weekly: Vec::new(),
            busy: Vec::new(),
        })
        .collect();
    assert_eq!(
        slots(
            &tz,
            &crowd,
            &[],
            service(60, 0, 0),
            &query(date(2026, 6, 1), 60)
        ),
        Err(availability::Error::TooManyStaff)
    );
    assert_eq!(
        slots(
            &tz,
            &crowd[1..],
            &[],
            service(60, 0, 0),
            &query(date(2026, 6, 1), 60)
        ),
        Ok(Vec::new())
    );
}
