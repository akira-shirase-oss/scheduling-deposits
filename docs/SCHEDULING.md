# The scheduling API (M1 backend, M2 availability and holds)

Every route is under `/v1/workspaces/{workspace}` and goes through the accounts kit
(`Accounts::access`, `docs/ACCOUNTS.md`): a session cookie or an API token. Each workspace
is one business (tenant, D-004). Every query is keyed by the path's workspace, so an id
from another tenant matches nothing. A workspace you cannot see answers `404 not_found`,
the same as one that does not exist. There is no UI (the maintainer designs it).

| Action | Who | Routes |
|---|---|---|
| Read | every member, API tokens | every `GET` |
| Configure | owner, admin (sessions) | `PUT`/`DELETE` of business, services, staff, hours, exceptions |
| Book | every member (sessions) | `POST …/holds`, `…/confirm`, `…/cancel` |

A token that tries to write gets `403 session_required`; a member who configures gets
`403 forbidden` (D-005). Bodies are JSON with unknown fields refused. Times are RFC 3339
UTC instants. Dates are `YYYY-MM-DD` and hours `"HH:MM"` (`24:00` ends a day), both in the
business's zone. Money is integer minor units.

## Set-up (Configure)

Ids are chosen by the client (UUIDs). `PUT` creates (`201`) or replaces (`200`), so a
retry is harmless (D-006).

| Route | Body | Notes |
|---|---|---|
| `PUT /business` | `name`, `time_zone` (IANA), `currency` (ISO 4217), `slot_step_minutes` (5, 10, 15, 20, 30, 60; default 15), `min_notice_minutes` (0–43200; 0), `max_advance_days` (1–365; 60), `hold_minutes` (1–60; 10) | Onboarding. Every other write needs it first (`409 business_required`). The zone must resolve in the host's zone database. |
| `PUT /services/{id}` | `name`, `duration_minutes` (5–720, multiple of 5), `buffer_before_minutes`, `buffer_after_minutes` (0–240, multiple of 5), `price_minor` (0–10^8), `deposit`, `active` (default true) | `deposit` is `{"kind":"none"}`, `{"kind":"fixed","amount_minor":N}` (1..price) or `{"kind":"percent","percent":P}` (1–100). The answer includes `deposit_minor`: integer, half rounded up (D-014). |
| `DELETE /services/{id}` | | `409 in_use` once a booking references it. |
| `PUT /staff/{id}` | `name`, `active` | |
| `DELETE /staff/{id}` | | `409 in_use` once booked. |
| `PUT /staff/{id}/services` | `service_ids` | Replaces the set. Every id must be this business's (`422 unknown_service`). |
| `PUT /staff/{id}/hours` | `hours: [{weekday 1–7 (Mon–Sun), start, end}]` | Replaces the week. At most 50 ranges. Ranges on the same day may touch but not overlap. |
| `PUT /exceptions/{id}` | `staff_id?`, `first_date`, `last_date` (inclusive, ≤ 367 dates), `kind`, `start?`/`end?`, `note?` | `closed` without hours closes whole dates; with hours it removes that range. Without `staff_id` it applies to everyone. `open` names a staff member and hours, and replaces that member's weekly hours on those dates. |
| `DELETE /exceptions/{id}` | | |

Reads: `GET /business`, `/services`, `/services/{id}`, `/staff` and `/staff/{id}`
(including `service_ids` and `hours`), and `/exceptions?from=&to=`.

## Availability (Read)

`GET /availability?service=&staff=&from=YYYY-MM-DD&days=1..31` answers
`{service_id, time_zone, from, days, as_of, slots: [{start, end, staff_id}], truncated}`.
Slots cover the active staff who perform the active service, or the one `staff` given.
They are sorted by start, then staff, and capped at 5,000 (`truncated: true` past that).
The inputs are read in one `REPEATABLE READ` snapshot. "Now" is the database clock
(`as_of`), the same clock that expires holds (D-009).

The rules (crate `availability`, D-007):

- Hours are civil time in the business's zone.
- In a spring-forward gap the missing local times have no slots. In a fall-back fold the
  repeated hour offers both instants.
- A start sits on the business's grid (`slot_step_minutes`) of local wall-clock time.
- The service time must lie inside working time. Buffers need not, but the whole
  buffered range must miss every booking and live hold.
- Starts before `now + min_notice` or after `max_advance_days` are left out.

## Holds and bookings (Book)

`POST /holds` takes `{service_id, staff_id, starts_at, customer_name, customer_email?,
idempotency_key}` and answers `201` with the booking (`status: "held"`, `hold_expires_at`,
`blocked_from`/`blocked_until` = start − buffer before … end + buffer after).

- The start must be a slot by the hours, grid, notice and advance rules
  (`422 slot_unavailable`).
- Only the database decides overlap. The exclusion constraint answers concurrent requests
  for the same or overlapping ranges with one `201` and `409 slot_taken` for the rest
  (D-008, D-010).
- Retrying the same `idempotency_key` with the same request gets `200` and the first hold.
  The same key with another request gets `409 idempotency_conflict`.

| Route | Effect |
|---|---|
| `POST /bookings/{id}/confirm` | held → confirmed. After its expiry: `409 hold_expired`. |
| `POST /bookings/{id}/cancel` | held or confirmed → cancelled. This frees the range. |
| `GET /bookings/{id}`, `GET /bookings?from=&to=` (instants, ≤ 500) | The status reads `expired` once a hold's time has passed, even before a writer marks it. |

M3 replaces confirmation with a verified payment webhook. Until then confirm is a plain
Book action.

### What the database guarantees (migration `0003_scheduling.sql`)

- `bookings_no_overlap`: `EXCLUDE USING gist (business_id WITH =, staff_id WITH =,
  blocked WITH &&) WHERE (status IN ('held','confirmed'))`.
- `bookings_prepare` (BEFORE INSERT) computes `ends_at` and the buffered `blocked` range
  from the service. A client's range is ignored. It refuses an inactive service or staff
  member, an unassigned pair, and a hold already past its expiry. It then marks every
  overlapping hold of that staff member that has passed its expiry as `expired`, in the
  same transaction and before the constraint checks the new row.
- `bookings_transition` (BEFORE UPDATE) allows changes to `status` only, along
  held → confirmed | cancelled | expired and confirmed → cancelled. It refuses confirming
  after expiry, and marking `expired` before it.
- The runtime role cannot `DELETE` bookings. The protections manifest pins the constraint,
  both triggers and the grants, and startup refuses a weakened copy (exit 3).

### Errors

| Status | Codes |
|---|---|
| 404 | `not_found` (also every cross-tenant id) |
| 409 | `business_required`, `slot_taken`, `hold_expired`, `idempotency_conflict`, `invalid_transition`, `in_use`, `inactive` |
| 422 | `invalid_*` (the rule broken), `not_assigned`, `unknown_service`, `unknown_staff`, `slot_unavailable` |
| 403 | `forbidden`, `session_required`, `services_limit`, `staff_limit`, `exceptions_limit` |
| 503 | `unavailable` (database) |

## Limits (each proven by `scripts/prove-limits.sh`)

| Limit | Value |
|---|---|
| Services per business | 200 |
| Staff per business | 100 |
| Exceptions per business | 1,000 |
| Weekly ranges per staff member | 50 |
| Dates per availability read | 31 |
| Slots per answer | 5,000 |
| Bookings per list | 500 |
| Engine: dates per query | 62 |
| Engine: staff per query | 1,000 |

## Operations

- Startup resolves every business's time zone. If one is missing from the zone database
  (`/usr/share/zoneinfo`, or `TZDIR`), it exits 5 and names the zones (END_GOAL failure
  case 9).
- Load scenarios: `SCENARIO=availability-read|hold-contention scripts/load.sh`. After a
  run, hold-contention audits the no-overlap invariant in SQL.
