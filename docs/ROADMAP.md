# Scheduling with Deposits — roadmap

M0 is this base setup; the later milestones come from the end goal (`docs/END_GOAL.md`).
A milestone's status cell says *Done* only with named tests and evidence.

| M | Spec week | Deliverable | Gate | Status |
|---|---|---|---|---|
| M0 | — | Base setup from the starter template: CI, gate, port band 63500–63599, drills and evidence kit | CI green on the empty product | **In progress — this base setup** |
| M0 | 0 | 5–10 operator interviews (spec), trade chosen; workspace, CI, schema with the constraint, a `hold-contention` smoke, the db-down drill | Interview summary written; CI green; failure case 14 | Planned |
| M1 | 1 | Auth, onboarding, services, hours, staff | Owner configures a business in the UI | **Backend done** (API in `docs/SCHEDULING.md`; `onboarding`, `isolation`, `limits` tests); the UI waits for the maintainer's design |
| M2 | 2 | Availability crate, booking page, holds, constraint | Failure cases 1, 2, 8, 9; `availability-read` and `hold-contention` | **In progress**: engine, holds, constraint and both scenarios done (`concurrent_holds_admit_exactly_one`, `buffers_are_part_of_the_blocked_range`, `availability` proptest, `startup_refuses_unresolvable_zones`); booking page not started |
| M3 | 3 | Razorpay test mode, webhooks, policies, refunds, reconciliation job; **blockers (D-020)**: confirm only through payment, idempotency keys scoped per caller | Failure cases 3–7 and 13; `webhook-ingest` | Planned |
| M4 | 4 | Reminders (email plus WhatsApp templates), calendar view, no-show | Failure cases 10–11; `reminder-dispatch`; restart mid-schedule loses nothing (spec) | Planned |
| M5 | 5–6 | Profiles, intake forms, reports, Google busy-time import | Failure case 12 | Planned |
| M6 | — (workspace) | Evidence battery, security review, docs site, deployment, restore | Failure case 15; every gate item except the pilot | Planned |
| M7 | 5–6 (spec's pilot) | Onboard pilot businesses | The pilot line of the ship gate | Planned |
