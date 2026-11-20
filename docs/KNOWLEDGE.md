# 07 — Scheduling SaaS with Deposits: verified domain knowledge

Checked on 2026-11-20 against the v2.0 specification (`07_Niche_Scheduling_SaaS_with_Deposits`, dated 2026-11-20).
Every fact carries a source. **UNVERIFIED** marks what I could not confirm. "The spec says" marks the spec's own
numbers (no-show rates of 10–30%, prices) that I did not check. Vendor and third-party pages are named as such. This is
not legal advice; section 9 turns the regulatory points into constraints on a public demo.

## 1. At a glance

| Spec dependency | State in November 2026 | Consequence |
|---|---|---|
| jiff | **1.0.1** (1.0.0 released 15 Nov 2026); the zoned type was renamed `ZonedDateTime` | Start on 1.x; code samples written for 0.2 (`Zoned`) will not compile |
| Postgres exclusion constraint | Works as the spec writes it, with `btree_gist`; PostgreSQL 18 adds `WITHOUT OVERLAPS` | The spec's constraint ignores buffers and cannot see hold expiry |
| Razorpay | Orders, checkout signature, webhooks (HMAC-SHA256), refunds with an idempotency header; UPI Collect sunset 28 Feb 2026 | Intent/QR only; signature checks on raw bytes |
| Platform collecting money for businesses | RBI's Payment Aggregator Directions 2025: a non-bank needs authorisation to aggregate payments | The spec's "platform collects and settles manually" option is not available to an unauthorised platform |
| Stripe | Invite-only for new Indian businesses since May 2024 | Stripe is the non-India path only |
| WhatsApp | Per-message pricing since 1 Jul 2025; in-window utility and service messages chargeable since 1 Oct 2026; INR billing mandatory by 31 Dec 2026 | The spec's "costs are per conversation" is outdated |
| Google Calendar | 600 req/min/user, 10,000/min/project; push channels expire with no auto-renewal; sync tokens can expire (410) | Renewal and full-resync paths are required |

## 2. Time and the availability engine (jiff)

- **Version and licence.** jiff 1.0.0 (15 November 2026) "marks an indefinite commitment to 1.x.y". 1.0.1 (17 November
  2026) fixed two bugs, one of them a parse that could produce an out-of-bounds timestamp. MSRV is Rust 1.85
  ([changelog](https://github.com/BurntSushi/jiff/blob/master/CHANGELOG.md);
  [crates.io](https://crates.io/crates/jiff)). Licence: Unlicense OR MIT.
- **Breaking renames from 0.2.** `Zoned` became `ZonedDateTime`, and `Timestamp::to_zoned` became `to_zoned_datetime`.
  `Timestamp − Timestamp` now yields a `SignedDuration`
  ([changelog](https://github.com/BurntSushi/jiff/blob/master/CHANGELOG.md)). Examples on the web from before 15
  November 2026 use the old names.
- **Time-zone database.** On Unix, jiff reads the system database, typically `/usr/share/zoneinfo`, at runtime. On
  Windows it embeds one. Feature `tzdb-bundle-always` forces an embedded copy, which the docs advise only with a reason
  ([jiff docs](https://docs.rs/jiff/latest/jiff/); [tz module](https://docs.rs/jiff/latest/jiff/tz/index.html)).
  **Pitfall:** the overview's Dockerfile uses `debian:bookworm-slim`, and the standard runs under systemd. Either way
  the host must have `tzdata`, or the build must enable `tzdb-bundle-always`. **UNVERIFIED:** what jiff does when no
  database exists on Unix. A startup check that resolves every business's zone and refuses to start otherwise removes
  the question.
- **DST.** The default disambiguation is `Compatible`. A civil time inside a spring-forward gap moves forward past the
  gap; a time inside a fall-back fold resolves to the earlier instant ([jiff docs](https://docs.rs/jiff/latest/jiff/)).
  For booking, a working-hours rule such as "09:00–17:00 local" must be expanded per date in the business's zone,
  then converted to UTC. A gap day is shorter and a fold day longer. `Asia/Kolkata` has no DST, so the property tests
  need a DST zone (for example `Europe/London` or `America/New_York`) to mean anything.
- **SQLx.** `jiff-sqlx` 1.0.0 provides wrapper types (`Timestamp`, `DateTime`, `Date`, `Time`, `Span`) for PostgreSQL
  and SQLite against sqlx 0.9 ([docs](https://docs.rs/jiff-sqlx/latest/jiff_sqlx/)). It has no `ZonedDateTime`
  wrapper. Store `timestamptz` (an instant) plus the business's IANA zone name as text, which is what the spec
  intends.

## 3. Double-booking prevention in PostgreSQL

- **The spec's constraint is valid SQL.** `EXCLUDE USING gist (staff_id WITH =, tstzrange(starts_at, ends_at) WITH &&)
  WHERE (status IN ('held','confirmed'))` is a partial exclusion constraint. The predicate creates a partial index and
  must be parenthesised ([CREATE TABLE](https://www.postgresql.org/docs/18/sql-createtable.html)). `btree_gist` provides
  GiST equality for `uuid`, `bool`, enums and more, and it is a trusted extension that a database owner with `CREATE`
  can install ([btree_gist](https://www.postgresql.org/docs/current/btree-gist.html)). Neon ships `btree_gist` 1.8 on
  PG 18 and 1.9 on PG 19 ([Neon extensions](https://neon.com/docs/extensions/pg-extensions)). A violation raises
  SQLSTATE `23P01` (`exclusion_violation`), which SQLx should map to the typed `SlotTaken`
  ([error codes](https://www.postgresql.org/docs/current/errcodes-appendix.html)).
- **PostgreSQL 18 alternative.** `PRIMARY KEY`/`UNIQUE (staff_id, during WITHOUT OVERLAPS)` behaves like `EXCLUDE
  USING GIST (… WITH =, during WITH &&)`. It needs a range column, rejects empty ranges, still needs `btree_gist` for
  scalar columns, and has no `WHERE` clause ([CREATE TABLE](https://www.postgresql.org/docs/18/sql-createtable.html)).
  Because cancelled bookings must not block, the partial `EXCLUDE` form is the right one here.
- **Pitfall 1: buffers.** The engine applies `buffer_before` and `buffer_after`, but the constraint covers only
  `[starts_at, ends_at)`. Two concurrent bookings whose buffers overlap but whose service times do not will both
  commit. Store a `blocked` range, `[starts_at − buffer_before, ends_at + buffer_after)`, and put the constraint on that.
- **Pitfall 2: hold expiry.** A predicate cannot express "held and not expired", because the time changes while the
  index stays fixed. An expired hold keeps blocking until something flips its status. The booking transaction should
  first expire stale holds for that staff member and range, then insert. A sweeper job alone leaves a window in which
  free slots look taken.
- **Pitfall 3: external busy time.** Google Calendar busy blocks are not rows in `bookings`, so the constraint cannot
  protect against them. Re-check external busy data at hold time (section 7), and record the staleness of the data used.

## 4. Razorpay (India)

- **Orders and signature.** Create an order server-side with `amount` in the currency subunit (paise). After checkout,
  verify `HMAC_SHA256(order_id + "|" + razorpay_payment_id, key_secret)` against `razorpay_signature`, using the order
  id stored server-side ([integration steps](https://razorpay.com/docs/payments/payment-gateway/web-integration/standard/integration-steps/)).
- **Capture.** Authorised payments must be captured, and "payments that are not captured are auto-refunded after a
  fixed time". Razorpay recommends auto-capture
  ([same page](https://razorpay.com/docs/payments/payment-gateway/web-integration/standard/integration-steps/)).
- **Webhooks** ([validate](https://razorpay.com/docs/webhooks/validate-test/); [FAQ](https://razorpay.com/docs/webhooks/faqs/)):
  - The header is `X-Razorpay-Signature`, an HMAC-SHA256 over the **raw** body with the webhook secret. After a secret
    rotation, retried old events still verify only against the old secret.
  - Duplicates are "expected behaviour"; deduplicate on `x-razorpay-event-id`.
  - Events can arrive out of order (`payment.captured` before `payment.authorized`).
  - Any non-2xx response is a failure, retried with exponential backoff for 24 hours, after which the webhook is
    **disabled**. A reply slower than 5 seconds counts as a timeout and is resent.
  - Replay windows of 7 and 15 days are both documented; the sources conflict.
- **Refunds.** Normal refunds take 5–7 working days. Instant refunds exist. Partial refunds may not exceed the captured
  amount. Normal refunds are impossible for payments older than 6 months
  ([refunds](https://razorpay.com/docs/payments/refunds/)). The `X-Refund-Idempotency` header makes a retry safe; a
  reused key with different parameters is a 409
  ([idempotent refunds](https://razorpay.com/docs/api/refunds/normal-refunds-idempotent)). **Consequence:**
  `max_advance_days` above about 180 collides with the 6-month refund limit for a deposit paid at booking.
- **UPI.** NPCI is sunsetting UPI Collect; Razorpay gives 28 February 2026 as the cutoff. Use Intent on mobile and QR
  on desktop. Standard Checkout disables Collect automatically
  ([Razorpay UPI FAQ](https://razorpay.com/docs/payments/payment-methods/upi/faqs)).
- **Pricing (vendor claim).** The standard rate is 2% plus 18% GST on the fee
  ([Razorpay blog](https://razorpay.com/blog/razorpay-payment-gateway-pricing-explained/)). **UNVERIFIED:** reports of
  a 2026 UPI MDR change, which came from vendor posts only.
- **Rust SDK.** No official crate. The community `razorpay` crate is 0.3.3 from June 2023
  ([crates.io](https://crates.io/crates/razorpay)). Write a small `reqwest` client, as the spec says.

## 5. Who may hold the money

- **RBI.** The Payment Aggregators Directions 2025 (issued 15 September 2025; updated 11 November 2026) say a non-bank
  "shall seek authorisation for operating as a PA". A PA must hold merchant funds in an escrow account with a
  scheduled commercial bank ([RBI master direction](https://rbi.org.in/Scripts/BS_ViewMasDirections.aspx?id=12896)).
  The spec's fallback, "the platform collecting and settling manually for pilots", would make the platform an
  unauthorised aggregator. Treat it as not allowed.
- **Allowed shapes.**
  1. **Each business uses its own Razorpay account.** Through Razorpay OAuth the platform acts on the account. Only the
     account owner can grant access. The platform must register as a partner. Access tokens last 90 days and refresh
     tokens 180 ([OAuth steps](https://razorpay.com/docs/partners/technology-partners/onboard-businesses/integrate-oauth/integration-steps)).
     The alternative, pasting API keys, makes the platform a store of merchant secrets.
  2. **Razorpay Route.** For splitting payments to linked accounts. Under the 2025 rules a Route user must show
     domestic turnover above ₹40 lakh (or exports above ₹5 lakh), and Razorpay reviews a written use case
     ([Route](https://razorpay.com/docs/payments/route/)). A new platform will not qualify at first.

## 6. Stripe (the alternative)

- **India.** Since May 2024 new Indian businesses need an invitation. New Indian connected accounts on platforms are
  also invite-only ([Stripe](https://support.stripe.com/questions/moving-to-invite-only-in-india)). From 1 January 2026
  new Indian users go through video KYC ([Stripe](https://support.stripe.com/questions/video-kyc-for-india-onboarding)).
- **Webhooks.** `Stripe-Signature` carries `t=` and `v1=` entries, and verification uses the exact raw body. Integrations
  must tolerate unknown entries and any order. Undelivered events are retried for up to three days. Duplicates happen,
  so deduplicate on the event id (sometimes the object id plus type). Stripe now recommends "thin" events for new
  integrations ([manage endpoints](https://docs.stripe.com/events/manage-webhook-endpoints);
  [set up events](https://docs.stripe.com/events/set-up-events)). **UNVERIFIED** from the fetched pages: the
  five-minute timestamp tolerance the official libraries use.
- **Rust.** `async-stripe` 0.41.0 is stable; 1.0.0-rc.10 (26 October 2026) is the active line, MSRV 1.89
  ([crates.io](https://crates.io/crates/async-stripe)).

## 7. Messaging

**WhatsApp Business Platform** ([pricing](https://developers.facebook.com/documentation/business-messaging/whatsapp/pricing)):

- **Billing.** Meta charges per delivered message since 1 July 2025, by category (marketing, utility, authentication,
  service) and recipient market. The spec's "per conversation" is outdated.
- **What became chargeable on 1 October 2026.**
  - Utility templates sent inside an open 24-hour window, free from July 2025, became chargeable again.
  - Service messages became chargeable beyond 1,000 free per business phone number per month.
- **India billing.** INR billing launched on 1 January 2026. Accounts must move to INR by 31 December 2026, or Meta
  stops delivering their messages from 1 January 2027.
- **Rates.** Rates change only on the first day of a quarter. I did not confirm current India per-message rates
  (third-party figures conflict).

Other WhatsApp rules that shape the design:

- **Opt-in.** The business must state that the person is opting in to messages from it, by name. The opt-in may be
  general rather than WhatsApp-specific, and it can be collected on a website or on paper
  ([opt-in](https://developers.facebook.com/documentation/business-messaging/whatsapp/getting-opt-in)). Store the opt-in
  with its text and time per client and business.
- **Templates.** Outside the 24-hour window only approved templates may be sent. A "utility" template with any
  promotional content is approved as marketing ([template categorisation](https://developers.facebook.com/documentation/business-messaging/whatsapp/templates/template-categorization)).
  Keep reminders free of offers.
- **Limits.** Messaging limits apply per business portfolio (since 7 October 2025) and count unique users per 24 hours
  ([limits](https://developers.facebook.com/documentation/business-messaging/whatsapp/messaging-limits)). Provider
  summaries say new portfolios start at 250 ([360dialog](https://docs.360dialog.com/docs/resources/wabas/messaging-limits)).
- **Multi-tenant onboarding.** Each business's own WhatsApp account is onboarded through Embedded Signup. That requires
  being a Tech Provider or Solution Partner. v2 of Embedded Signup was deprecated on 15 October 2026 in favour of v4. A
  phone number must be registered within 14 days of signup
  ([Embedded Signup](https://developers.facebook.com/documentation/business-messaging/whatsapp/embedded-signup/overview/);
  [Tech Providers](https://developers.facebook.com/documentation/business-messaging/whatsapp/solution-providers/get-started-for-tech-providers)).
- **Webhook signatures.** `X-Hub-Signature-256: sha256=<hex>` is an HMAC-SHA256 of the raw body keyed by the **app
  secret** ([Meta](https://developers.facebook.com/documentation/business-messaging/whatsapp/webhooks/create-webhook-endpoint)).
- **AI assistants (the spec's stretch goal).** From 15 January 2026 the Business Solution Terms bar AI providers from
  using the platform for general-purpose assistants. Business-specific bots (booking, support) remain allowed
  ([TechCrunch](https://techcrunch.com/2025/10/18/whatssapp-changes-its-terms-to-bar-general-purpose-chatbots-from-its-platform/)).
  A booking-only assistant fits. Meta's own paid "Meta Business Agent" message type appeared on 1 July 2026
  ([pricing](https://developers.facebook.com/documentation/business-messaging/whatsapp/pricing)).

**SMS in India.** Commercial SMS needs DLT registration of the entity, headers and templates. URLs in messages must be
whitelisted, per TRAI's 2024 direction ([TRAI](https://trai.gov.in/sites/default/files/Direction_28102024.pdf)). TRAI
mandated pre-tagging of variables in templates in November 2025
([TRAI PR 133/2025](https://trai.gov.in/sites/default/files/2025-11/PR_No.133_of_2025.PDF)). A third-party guide says a
header must match the registering entity ([Plivo](https://support.plivo.com/hc/en-us/articles/360046769131-DLT-Registration-Process-for-Sending-SMS-to-India)),
so a platform cannot send under each business's brand from its own registration. Leave SMS out of v1.

**Email.** Resend's free plan allows 100 emails a day and 3,000 a month; paid plans have no daily cap
([Resend](https://resend.com/docs/knowledge-base/what-is-resend-pricing)). Two reminders per booking exhaust the free
daily cap at 50 bookings a day. `lettre` 0.11.24 covers SMTP ([crates.io](https://crates.io/crates/lettre)).

## 8. Google Calendar sync

- **Quotas.** 10,000 requests per minute per project and 600 per minute per user per project. 1,000,000 per day is a
  billing threshold, "planned to incur charges later in 2026" with 90 days' notice. Over-quota calls return
  `403 usageLimits` or `429`; use truncated exponential backoff
  ([quota](https://developers.google.com/workspace/calendar/api/guides/quota)).
- **Push notifications.** The webhook needs HTTPS with a valid certificate. Notifications carry no event data, only
  headers (`X-Goog-Channel-ID`, `X-Goog-Resource-State`, `X-Goog-Message-Number`, an optional `X-Goog-Channel-Token`).
  Channels expire and are not renewed automatically: call `watch` again with a new id before expiry
  ([push](https://developers.google.com/workspace/calendar/api/guides/push)). The maximum channel lifetime is not
  stated on the page (**UNVERIFIED**).
- **Incremental sync.** Store `nextSyncToken`. Deleted items are included. A `410` means "clear and full-sync again".
  Most filters cannot be combined with a sync token ([sync](https://developers.google.com/workspace/calendar/api/guides/sync);
  [errors](https://developers.google.com/calendar/api/guides/errors)).
- **Free/busy alternative.** `freeBusy.query` takes at most 50 calendars per request. Narrow scopes exist
  (`calendar.freebusy`, `calendar.events.freebusy`; and `calendar.events.owned` or `calendar.app.created` for writing
  events) ([freebusy](https://developers.google.com/workspace/calendar/api/v3/reference/freebusy/query);
  [scopes](https://developers.google.com/workspace/calendar/api/auth)).
- **App verification.** An app requesting sensitive or restricted scopes without verification shows a warning screen.
  It is limited to 100 new users over the project's lifetime, a cap that cannot be reset
  ([unverified apps](https://support.google.com/cloud/answer/7454865)). Reading calendar events is Google's own example
  of a sensitive scope ([sensitive scopes](https://developers.google.com/identity/protocols/oauth2/production-readiness/sensitive-scope-verification)).
  **UNVERIFIED:** the classification of the two free/busy scopes.

## 9. Personal data, and constraints on a public demo

- The DPDP Rules 2025 were notified on 13–14 November 2025; most duties (notice, consent, security, breach reporting)
  apply around 13 May 2027 ([PIB](https://static.pib.gov.in/WriteReadData/specificdocs/documents/2025/nov/doc20251117695301.pdf);
  [Mondaq](https://www.mondaq.com/dpdp-act-and-rules-2025-the-2026-compliance-milestones-businesses-cant-afford-to-miss/1830402)).
  The business is the data fiduciary for its clients; the platform is its processor.
- **Demo constraints:**
  - payments in Razorpay and Stripe **test mode** only;
  - no live WhatsApp sends to unconsented numbers: use the test number and recorded fakes;
  - synthetic businesses and clients only;
  - no health trades (the spec already avoids clinics);
  - no handling of real deposits until each business connects its own payment account (section 5);
  - a visible privacy notice and a "delete my data" path before any real client books.

## 10. Crates (crates.io, 2026-11-20)

| Crate | Version | Licence |
|---|---|---|
| [jiff](https://crates.io/crates/jiff) / [jiff-sqlx](https://crates.io/crates/jiff-sqlx) | 1.0.1 / 1.0.0 | Unlicense OR MIT |
| [sqlx](https://crates.io/crates/sqlx) | 0.9.1 | MIT/Apache-2.0 |
| [axum](https://crates.io/crates/axum) / [tokio](https://crates.io/crates/tokio) | 0.8.9 / 1.54.1 | MIT |
| [hmac](https://crates.io/crates/hmac) / [sha2](https://crates.io/crates/sha2) / [subtle](https://crates.io/crates/subtle) | 0.13.0 / 0.11.1 / 2.6.1 | MIT/Apache-2.0; subtle BSD-3-Clause |
| [oauth2](https://crates.io/crates/oauth2) | 5.0.0 (Jan 2025) | MIT/Apache-2.0 |
| [async-stripe](https://crates.io/crates/async-stripe) | 0.41.0; 1.0.0-rc.10 | MIT/Apache-2.0 |
| [apalis-postgres](https://crates.io/crates/apalis-postgres) | 1.1.1 | crates.io says "non-standard" |
| [proptest](https://crates.io/crates/proptest) / [criterion](https://crates.io/crates/criterion) | 1.12.0 / 0.8.2 | MIT/Apache-2.0 |
| [tower_governor](https://crates.io/crates/tower_governor) | 0.8.0 (Aug 2025) | MIT/Apache-2.0 |

## 11. Where the spec is wrong or outdated

1. **"WhatsApp costs are per conversation."** Billing is per message (since July 2025). In-window utility and service
   messages became chargeable on 1 October 2026, and INR billing is mandatory for Indian accounts by 31 December 2026.
2. **"The platform collecting and settling manually for pilots."** That is payment aggregation, which needs RBI
   authorisation. Use each business's own account (OAuth), or Route once eligible.
3. **The exclusion constraint misses buffers and hold expiry.** Constrain a stored blocked range, and expire holds
   inside the booking transaction.
4. **"Razorpay/Stripe" as equals.** Stripe is invite-only for Indian businesses. Razorpay is the only practical Indian
   rail, and UPI Collect is gone (Intent and QR only).
5. **jiff.** 1.0 renamed `Zoned` to `ZonedDateTime`. Minimal runtime images need `tzdata` or the bundled database.
6. **The "AI assistant replying on WhatsApp" stretch goal.** Allowed only as a business-specific booking assistant;
   general-purpose assistants are banned.
7. **Google Calendar "two-way sync" at P1.** Needs app verification beyond 100 users, channel renewal, 410 recovery and
   quota backoff. It is larger than one week's item.
