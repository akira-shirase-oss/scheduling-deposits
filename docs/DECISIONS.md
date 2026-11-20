# Decisions

Trade-offs and decisions, D-001 onward. Each entry says what was chosen and why; a
superseded decision is marked, never rewritten.

## D-001 — Base: the Backend Platform starter template

The repository begins as a copy of the Backend Platform's copy-and-own starter template —
the bounded HTTP layer, the three-role PostgreSQL split with its protections manifest,
the accounts kit, releases, restore drills, and the CI + evidence kit — renamed for this
product. The commit it came from is in `TEMPLATE_ORIGIN`; later template changes arrive
only as reviewed diffs. Nothing shares a package, service, login or database with the
template's repository or any sibling product.

## D-002 — Port band 63500-63599

Every port this repository binds on a shared development machine stays inside
63500–63599: the server on 63550, the development PostgreSQL on 63532, the
template's other defaults shifted into the band. `scripts/check-ports.sh` fails on a
literal outside the band, and the gate proves it can fail. Rationale: Linux's ephemeral
range ends at 60999, so CI and a second checkout never collide.

## D-003 — Stack

Rust backend on Axum, SQLx and tokio (the template's stack); SvelteKit + TypeScript
(strict) for the web UI once the maintainer chooses this product's design system — no
`web/` product code before that.
Everything else follows `docs/END_GOAL.md`.

## D-004 — A workspace is a business; tenancy by composite keys

Each accounts-kit workspace is one business (tenant). `businesses` is keyed by
`workspace_id`. Every product table's key is `(business_id, id)`, and every reference
between them is a composite foreign key, so a row cannot point at another tenant's row.
Every route takes the tenant from the path, authorizes it with `Accounts::access`, and
binds it in every query. A cross-tenant id answers `404`, exactly like a missing one.
Proven by `isolation::one_tenant_never_reads_or_writes_anothers_records`.

## D-005 — Two new actions: Configure and Book

`Action::Configure` (owner, admin) changes set-up. `Action::Book` (every member) places,
confirms and cancels. API tokens stay read-only, as the kit decided, so a hold needs a
session. The public booking page's caller (a customer with no account) is not decided
yet: see the PR's questions.

## D-006 — Client-chosen ids and idempotent writes

Set-up resources are written with `PUT /…/{id}`: a create-or-replace, safe to retry. A
hold carries a required `idempotency_key`, unique per business. The same key with the
same request returns the first hold. A different request under that key is a conflict.

## D-007 — The availability engine's rules

The engine is pure: no I/O and no clock. `scripts/check-architecture.sh` enforces this.

- Hours are civil time.
- A spring-forward gap has no slots. A fall-back fold offers both instants.
- Starts sit on a local wall-clock grid.
- The service time must lie inside working time. Buffers need not lie inside working time
  but must miss busy ranges.
- A `closed` exception subtracts a range or whole dates. An `open` exception replaces a
  staff member's weekly hours on its dates.

The tests compare it against a brute-force minute-by-minute oracle over random schedules
in six zones (Lord Howe's 30-minute shift and São Paulo included). There are explicit gap
and fold cases for London and New York.

## D-008 — The guarantee is an exclusion constraint over the buffered range

`blocked = [start − buffer_before, end + buffer_after)` is computed by a trigger from the
service, never taken from the client. `EXCLUDE USING gist (business_id WITH =, staff_id
WITH =, blocked WITH &&) WHERE (status IN ('held','confirmed'))` holds the guarantee.
`btree_gist` lives in its own `extensions` schema. Buffers are inside the guarded range,
per KNOWLEDGE.md's correction.

## D-009 — Hold expiry is enforced in the database, on its clock

A partial constraint's predicate is not re-evaluated as time passes. So the insert
trigger first marks any overlapping hold that is past `hold_expires_at` as `expired`, in
the same transaction, before the new row's constraint check. A hold already past its
expiry cannot be inserted. Confirming after expiry is refused. Reads report an elapsed
hold as `expired` before any writer marks it. Availability's "now" is the database's
`now()`, so Rust and SQL never disagree about whether a hold is live. The
`no-db-expiry` mutant run is in the PR.

## D-010 — The hold path never pre-checks overlap in Rust

The server checks only that the start is a slot by hours, grid, notice and advance: the
engine runs with no busy ranges. The constraint alone decides overlap. A Rust pre-check
would race, and it would hide the database's verdict from the concurrency test.
`concurrent_holds_admit_exactly_one` runs 64 parallel holds × `BOOKING_HOLD_ROUNDS`, with
one winner per round.

## D-011 — Per-business bounds, each with a proof

| Bound | Value |
|---|---|
| Services | 200 |
| Staff | 100 |
| Exceptions | 1,000 |
| Weekly ranges per staff member | 50 |
| Dates per read | 31 |
| Slots per answer (then `truncated`) | 5,000 |
| Bookings per list | 500 |
| Engine: dates per query | 62 |
| Engine: staff per query | 1,000 |

Counted bounds take the business row lock in the writing transaction. Every bound has a
`LIMIT(...)` marker and a mutation in `scripts/prove-limits.sh`.

## D-012 — Load scenarios and thresholds, chosen before any run

**availability-read** (token reads: 90% one day, 10% a week, 10 staff, open around the
clock).

| Metric | Threshold |
|---|---|
| p95, one day | < 50 ms |
| p99, one day | < 100 ms |
| p99, a week | < 250 ms |
| Failed rate | < 0.1% |
| Dropped iterations | 0 |
| Empty answers | 0 |

**hold-contention** (sessions racing for 50 hot slots).

| Metric | Threshold |
|---|---|
| p95 | < 50 ms |
| p99 | < 100 ms |
| Unexpected statuses | 0 |
| Holds created | ≤ hot slots |
| Overlapping live bookings (SQL audit after the run) | 0 |

These thresholds were committed before the first run. Full shape is 1,000,000 requests
each, in CI's `load` job. A local smoke run is 100,000 requests on a shared machine: an
observation, not capacity. Misses are kept.

## D-013 — Zones: IANA names, the system zone database, refusal at startup

`jiff` 1.0.1 runs with `tzdb-zoneinfo` and no bundled database, so zone data comes from
the host's tzdata. A name is shape-checked before lookup, so no path reaches `TZDIR`.
Startup resolves every business's zone and exits 5, naming the missing zones (failure
case 9).

## D-014 — Deposits as a rule on the service

`none`, `fixed` (1..price, in minor units) or `percent` (1–100). The amount is integer
arithmetic, rounded half up to the minor unit (`deposit_minor`). Currency is per business.
Taking the payment is M3.

## D-015 — Bookings are immutable except their status

The runtime role may insert and update bookings but not delete them. The update trigger
lets only `status` change, along the allowed transitions. `hold_expires_at` stays after
confirmation as the record of the hold.

## D-016 — A credential lookup writes only when it must

The first full CI runs of both scenarios (commit b767be2) missed their thresholds.

| Scenario | Failed | p95 |
|---|---|---|
| availability-read | 23% (shed) | 369 ms |
| hold-contention | 0.46% | p99 108 ms |

Both use one credential. Each lookup of the kit was an `UPDATE … SET last_* = CASE …
ELSE last_* END`, so every request wrote a new version of one row. Those writes
serialized on that row's lock and a WAL flush. That caps one token or session at the
disk's commit rate, however idle the rest of the system is.

The lookups now read the row in a CTE and update it only when the timestamp is more than
a minute old. `touches::fresh_credentials_are_read_not_rewritten_and_stale_ones_are_touched`
compares the row's `xmin` across reads. It failed before the change, and both misses are
kept in the PR.

## D-017 — An availability answer lists each time once

The second full CI run of availability-read (after D-016) still missed: 16.4% shed, with
p95 332 ms. That run received 124 GB, about 124 KB per answer. Each (start, end, staff)
was its own ~105-byte object, and the seed has 10 staff free around the clock.

The answer now lists each start once, with the indexes of the staff free at that time
(`starts: [{at, staff: [i…]}]`, `staff_ids`, `duration_minutes`). The end is the start
plus the duration.

- The 5,000 cap still counts (time, staff) pairs.
- The scenario's thresholds are unchanged. Its empty-answer check now looks for `"at"`.
- The `limits` test proves the grouping.
