# Scheduling with Deposits — end goal

**A client books a real slot, pays a deposit, and gets reminded. The business never sees a double booking, never
loses a paid deposit, and can explain every refund or forfeit.**

A multi-tenant booking product for one trade of small service businesses; the trade is not yet chosen (Q-1). Each
business gets a public booking page with trade-specific intake fields. A client picks a service and a slot, the slot
is held, and the client pays a deposit through the business's own payment account. The booking is confirmed only when
the payment provider says so. Cancellation and no-show policies are versioned; the version the client accepted
decides the refund or forfeit. Reminders go by WhatsApp and email at 24 hours and 2 hours. Staff calendars can
subtract their Google Calendar busy time. The core is a pure Rust availability engine (jiff) and a PostgreSQL
exclusion constraint that makes overlapping bookings impossible, even under concurrency.

**Origin.** The v2.0 programme spec `07_Niche_Scheduling_SaaS_with_Deposits` (2026-11-20), merged with the standard
rules in `AGENTS.md`. Verified facts and the spec's errors are in `KNOWLEDGE.md` next to this file. Every number below
is either the spec's (marked "the spec says") or a proposed acceptance target, not a measurement. A target changes
only with a measurement and a decision entry.

## Users and the job to be done

- **The operator** (owner of a one- to five-person business) wants fewer no-shows and less booking chatter on WhatsApp.
  Deposits must reach their own account, with rules they set once.
- **Staff** need a day and week view, the client's history and intake answers, and a quick "completed" or "no-show".
- **The client** wants to see real free times in the business's local time, pay by UPI or card, and know the
  cancellation terms before paying.

The spec says no-shows can be 10–30% of appointments in some trades. That figure is unverified. The pilot measures the
business's own rate before and after.

## In scope (v1)

- **Onboarding:** profile, IANA time zone, services (duration, price and deposit in integer minor units, buffers),
  staff working hours, breaks, time off, holidays, minimum notice, maximum advance.
- **Availability:** pure, deterministic, DST-correct slot generation (spec §4). The same inputs give the same slots.
- **Booking:** a hold (the spec says 10 minutes) when checkout starts, confirmed by a verified payment event.
  Expired holds are released.
- **Double-booking guard:** a partial exclusion constraint over each staff member's *blocked* range (service time plus
  buffers), with holds expired inside the booking transaction (KNOWLEDGE §3).
- **Deposits:** fixed or percentage, through Razorpay on the business's own account (UPI Intent or QR, and cards).
  Balance is due at the visit, off-platform.
- **Policies:** cancellation window, refund rule, no-show forfeit, reschedule limit. Each is versioned; the accepted
  version is stored with the booking and shown before payment.
- **Refunds:** automatic inside the window, idempotent, tracked to a final state.
- **Reminders:** email always; WhatsApp utility templates where the client opted in. Scheduled jobs, each with a key
  and a final state.
- **Dashboard:** day and week view, manual bookings, mark completed or no-show, client profiles with no-show count and
  notes, intake forms, reports (bookings, deposits, forfeits, no-show rate, utilisation).
- **Google Calendar busy-time import** per staff member: read-only free/busy in v1 (Q-11).
- **Data rights:** a business can export and delete a client's data; clients see a privacy notice.

## Later (each needs its own contract)

Two-way calendar write, waitlist auto-fill, packages and memberships, Stripe for non-Indian businesses, SMS,
Razorpay Route payouts, a booking-only WhatsApp assistant, a provider PWA.

## Out of scope (explicit non-goals)

- **Holding or settling money for businesses.** The platform never collects deposits into its own account. That would
  be payment aggregation without RBI authorisation (KNOWLEDGE §5).
- **Health and clinic trades** (the spec says the regulatory burden is too high for an MVP).
- **General-purpose AI chat** on WhatsApp, which Meta's terms forbid.
- **SMS in v1.** DLT registration is per entity, so it does not fit a multi-tenant sender.
- **Card data on our servers.** Checkout is the provider's.
- **Balance collection, invoicing and GST invoices for the business's services.**
- **Pricing and billing of businesses.** The spec's ₹299–1,499/month is a hypothesis to test in interviews, not part of
  this plan.

## Architecture in brief

| Piece | Role |
|---|---|
| `crates/availability` | Pure (no I/O): working-hours expansion per local date via jiff 1.x, subtraction of busy ranges and buffers, notice and advance limits, 15-minute steps (the spec's granularity). Property-tested and benchmarked |
| `crates/domain` | Pure: money (`i64` minor units plus currency, checked arithmetic, one stated rounding rule for percentage deposits), policy evaluation (refund or forfeit from version, times and zone), booking state machine |
| `crates/db` | SQLx: migrations, composite `(business_id, id)` keys, the exclusion constraint (`btree_gist`), `23P01` mapped to `SlotTaken`, an append-only payment event log |
| `crates/server` | Axum on Tokio: API, the dashboard and public booking pages on one origin (workspace rule; Q-5), webhooks, sessions, rate limits |
| `crates/worker` | Postgres job table with `FOR UPDATE SKIP LOCKED` and leases: hold expiry, reminders, refund retries, payment reconciliation, calendar sync and channel renewal |
| `crates/payments` | Razorpay over `reqwest`: orders, refunds with `X-Refund-Idempotency`, signature checks on raw bytes, OAuth tokens. A Stripe adapter later |
| `crates/notify` | A `Notifier` trait for WhatsApp Cloud API and email, plus recorded fakes for tests and load |
| `crates/calendar` | Google OAuth, free/busy and events with sync tokens, push-channel renewal |
| `web/` | SvelteKit + TypeScript (strict) + Tailwind CSS v4 + shadcn-svelte, with its own design system (Q-10) |

Hosting follows the standard layout: the Rust server under systemd behind Caddy on the product's droplet; Cloudflare
DNS, TLS and cache in front; landing page and docs site on Cloudflare Pages; Postgres with `verify-full` (Neon or
co-located, Q-6); backups to object storage with a restore drill. Outbound calls go only to Razorpay, Meta's Graph
API, Google APIs and the email provider, on an allowlist with deadlines.

## Non-negotiable invariants

1. **No overlap.** For each staff member, no two `held` or `confirmed` bookings have overlapping blocked ranges. The
   database enforces this, so raw SQL fails too.
2. **Payment truth comes from the provider.** A booking is confirmed only by a signature-verified webhook or an
   authenticated API read, never by the browser.
3. **At most once.** Each provider event is applied at most once (keyed by event id), stored append-only, and replaying
   the log reproduces current payment state.
4. **Exact money.** Integer minor units. Refunds for a payment never exceed its captured amount. A database `CHECK`
   enforces it.
5. **Time.** Instants are stored as UTC `timestamptz` and zones by IANA name. All slot arithmetic happens in the pure
   crate, and the server refuses to start if a business's zone does not resolve.
6. **Policies are immutable per version.** Every refund or forfeit decision records the policy version and the times it
   compared.
7. **Consent before messages.** No WhatsApp send without a stored opt-in that names the business. Every message has a
   key and ends `sent`, `failed` (with a code) or `unknown` (with a reason).
8. **Tenancy by construction.** Every row carries `business_id` in composite keys. A public page exposes busy times,
   never other clients' names.
9. **Bounded everything.** Deadlines on every outbound call, bounded queues and bodies, caps on holds per client and per
   address; admission is refused, never silently queued.
10. **Evidence over claims.** Load results carry machine, commit and `uptime`; misses are kept, never rerun until green.

## Required failure cases (contract)

| # | Trigger | Required result | Proven by | Pass condition | From |
|---|---|---|---|---|---|
| 1 | 64 concurrent holds for one staff slot, repeated 1,000 times (the spec says 20 tasks) | Exactly one wins | `concurrent_holds_admit_exactly_one` (`#[sqlx::test]`) | One success per round; the rest get `409 slot_taken`; the post-run SQL overlap audit finds zero | M2 |
| 2 | Two bookings whose buffers overlap but whose service times do not, concurrently | One wins | `buffers_are_part_of_the_blocked_range` | The second gets `slot_taken`; no overlapping blocked ranges | M2 |
| 3 | Payment captured after the hold expired and the slot was re-booked | No double booking; the client is refunded | Drill `late-payment` | Booking stays unconfirmed; one refund with an idempotency key; client notified; both events in the log | M3 |
| 4 | Webhook duplicates, out-of-order events, replays | Applied once, in effect order | `webhooks_are_idempotent_and_order_tolerant` | Final state equals the serial replay; zero double confirmations or refunds | M3 |
| 5 | Forged signature, tampered body, or a client claiming "paid" | No state change | `forged_webhooks_and_client_claims_change_nothing` | `401` for bad signatures; held bookings stay held | M3 |
| 6 | Webhook endpoint down past Razorpay's 24-hour retries (the webhook gets disabled) | Payments still found | Drill `webhook-outage` | The reconciliation job confirms every held booking with a captured payment via the API; an alert is raised; zero lost paid deposits | M3 |
| 7 | Refund API times out or fails | One refund, eventually visible | `refund_retries_reuse_the_key` | Retries reuse one `X-Refund-Idempotency` key; at most one refund; the failure is shown to the operator | M3 |
| 8 | DST gap and fold days; a client in another zone | Correct local slots | proptest in `availability` across `Europe/London`, `America/New_York`, `Asia/Kolkata` | No slot overlaps busy time or buffers; none inside a gap; fold days are neither double-counted nor skipped; display in the business's zone | M2 |
| 9 | Time-zone database missing on the host | Refuse to start | `startup_refuses_unresolvable_zones` | Process exits with a named error; readiness never green | M2 |
| 10 | Worker `SIGKILL`ed at 20 seeded points during reminder dispatch | Nothing lost; no silent duplicate | Drill `kill-mid-reminders` | Every message is in a final state; duplicates only for `unknown` outcomes, each recorded (Q-8) | M4 |
| 11 | WhatsApp down, template paused, or no opt-in | Email fallback | `messages_fall_back_and_respect_consent` | Zero WhatsApp sends without opt-in; the email is sent; the cause is recorded | M4 |
| 12 | Calendar `410`, channel expiry, `403/429 usageLimits`, Google down | Recover without false availability | Drill `calendar-faults` | Full resync after 410; channels renewed before expiry; backoff honoured; stale busy data handled by policy (Q-9) | M5 |
| 13 | Policy edited after a booking; cancellation exactly at the window boundary | The accepted version decides | `accepted_policy_version_governs` | Same result as a fresh evaluation of the stored version; the boundary rule is stated and tested in two zones | M3 |
| 14 | Database down during checkout | Refuse, never half-accept | Drill `db-down` | Readiness red within 2 s; booking returns `503`; no hold or payment order exists without its row | M0 |
| 15 | Restore from last night's backup | Consistent and aware of the loss | Drill `restore` | Zero overlaps; the payment log verifies; bookings made after the backup are reconciled from provider APIs | M6 |

### Security cases

| Case | Pass condition |
|---|---|
| Cross-business reads and writes over every route | Zero rows readable or writable; raw SQL cross-business references fail in the schema |
| Hold flooding from one client or address | Caps and rate limits refuse it; legitimate slots stay bookable in the load test |
| OAuth tokens (Razorpay, Google, Meta) at rest | Encrypted; never in logs; revocation removes access |
| Logs | A log-capture test finds no phone numbers, emails, intake answers, tokens or secrets |
| Roles | Staff cannot change policies, payouts or refunds; the owner can; every mutation is audited |
| Public page | Shows busy or free only; no client data; CSRF on every mutating request |

## Load scenarios (each at least 1,000,000 units; thresholds proposed)

Third parties are never load-tested; recorded fakes stand in for Razorpay, Meta and Google.

| Scenario | Unit and mix | Pass (proposed) |
|---|---|---|
| `availability-read` | 1,000,000 availability requests over 500 businesses, 2,000 staff, 30 days ahead, at a fixed rate | Zero 5xx; a 1-in-1,000 sample equals the pure engine's output from the same snapshot; server p99 ≤ 50 ms; peak RSS recorded |
| `hold-contention` | 1,000,000 hold attempts in groups of 50 aimed at one slot (some overlapping only by buffers) | At most one success per group; zero overlaps in the post-run audit; zero 5xx; p99 ≤ 100 ms |
| `webhook-ingest` | 1,000,000 signed deliveries: 20% duplicates, 10% out of order, 2% bad signatures | Each valid event applied once (digest equals serial replay); every bad one `401`; ack p99 ≤ 250 ms, well inside Razorpay's 5 s timeout |
| `reminder-dispatch` | 1,000,000 scheduled messages over a simulated day, with 20 worker kills | Every message in a final state; zero known-outcome duplicates; lateness p99 ≤ 60 s |

## Ship gate (all must hold on the release commit)

- [ ] All 15 failure cases and the security cases pass in CI.
- [ ] The four full load runs are committed under `perf/results/` with machine, commit and `uptime`; each threshold is
  met, or its miss is recorded with a decision.
- [ ] Spec tests: availability properties plus a criterion benchmark for a month of slots; Playwright E2E for book →
  pay (test mode) → reminder → cancel → refund; the concurrency-proof GIF explaining the constraint.
- [ ] An independent security review of payments, webhooks, tenancy and tokens, with findings fixed or accepted in
  decisions.
- [ ] Docs site `<project>-docs` (Astro Starlight on Cloudflare Pages): a quick start ending in a real API call with a
  token, concepts, guides, an OpenAPI-generated reference, self-hosting, and a changelog at `/changelog`. Examples run
  in CI.
- [ ] The UI covers every operation, with a UI audit at 1440 and 390, light and dark, as owner and as staff.
- [ ] Live deployment on the product's droplet behind Cloudflare, watched by Service Monitor, with seeded synthetic
  businesses so the first screen is never empty; restore drill done.
- [ ] The overview's Definition of Done: README (pitch, 60–90 s demo, diagram, quick start, live URL, limitations), CI
  badge and coverage, p50/p99 and memory numbers, `docs/ARCHITECTURE.md`, a build post, clean history, MIT or Apache-2.0.
- [ ] Pilot: real bookings at real businesses (the spec says 3 by week 5), with zero double bookings and zero lost paid
  deposits over the pilot. No-show change is reported from the businesses' own numbers. **Blocked** until operators
  are found (Q-1).

## Milestones (from the spec's weeks)

| Milestone | Spec week | Builds | Gate |
|---|---|---|---|
| M0 | 0 | 5–10 operator interviews (spec), trade chosen; workspace, CI, schema with the constraint, a `hold-contention` smoke, the db-down drill | Interview summary written; CI green; failure case 14 |
| M1 | 1 | Auth, onboarding, services, hours, staff | Owner configures a business in the UI |
| M2 | 2 | Availability crate, booking page, holds, constraint | Failure cases 1, 2, 8, 9; `availability-read` and `hold-contention` |
| M3 | 3 | Razorpay test mode, webhooks, policies, refunds, reconciliation job | Failure cases 3–7 and 13; `webhook-ingest` |
| M4 | 4 | Reminders (email plus WhatsApp templates), calendar view, no-show | Failure cases 10–11; `reminder-dispatch`; restart mid-schedule loses nothing (spec) |
| M5 | 5–6 | Profiles, intake forms, reports, Google busy-time import | Failure case 12 |
| M6 | — (workspace) | Evidence battery, security review, docs site, deployment, restore | Failure case 15; every gate item except the pilot |
| M7 | 5–6 (spec's pilot) | Onboard pilot businesses | The pilot line of the ship gate |

## Open questions for the maintainer

1. **Trade, and who to interview.** The spec recommends interviews before code. Which trade, and which operators can
   you reach?
2. **Jurisdiction.** India first (Razorpay, WhatsApp, DPDP) or elsewhere (Stripe, which is invite-only in India)?
3. **Money path.** Each business's own Razorpay via OAuth (needs partner registration), or pasted API keys (we would
   store merchant secrets)? The spec's "small % of deposits" pricing implies taking a cut, which needs Route or an
   aggregator. Is it dropped?
4. **WhatsApp model.** Become a Meta Tech Provider so each business sends from its own number, or one platform number
   naming each business? Who pays Meta's per-message charges (chargeable in-window since 1 October 2026)?
5. **Booking pages.** The spec wants SvelteKit SSR on Cloudflare (`adapter-cloudflare`, which runs on Workers). The
   workspace serves dashboards from the Rust server and uses no Workers for servers. Proposed: the Rust server serves
   booking pages behind Cloudflare's cache, at path URLs (`/b/<slug>`), as the spec allows.
6. **Database.** Neon (no Indian region; Singapore is nearest) or Postgres co-located on a Bangalore droplet? The
   constraint sits in the booking write path, so round-trip latency matters.
7. **Droplets.** The overview's one 2 vCPU / 4 GB droplet for several apps, or the standard's one droplet per product?
8. **Unknown send outcomes.** After a crash between the provider call and the commit, re-send (risk a duplicate) or not
   (risk a missed reminder)?
9. **Stale calendar data.** When Google busy data is older than a limit, hide that staff member's slots or accept the
   risk?
10. **Design identity.** Palette, type and shape options with screenshots for your choice (not Colophon's).
11. **Calendar scope.** Free/busy read-only in v1, or two-way event writes (broader, sensitive scopes and Google
    verification past 100 users)?
12. **Name and domain** for the product, its booking pages and its docs site.
