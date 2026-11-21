#!/usr/bin/env bash
# Every limit is proven by a test that fails when the limit is removed.
#
# Each bound in the source carries a marker, `// LIMIT(name)` (`-- LIMIT(name)` in a
# migration), on the line that sets it (or on a comment line just above it). For every marker this script removes the limit in a
# scratch copy — one substitution on that line — and runs the one test that proves it,
# which must then FAIL as a test (a build error is not a proof). A marker without a
# mutation below, or a mutation whose marker is gone, fails the script too, so a new limit
# cannot be added without its proof.
#
#   scripts/prove-limits.sh [--out FILE]   (FILE: a JSON record; never names the machine)
#
# Needs BOOKING_TEST_DATABASE_URL (scripts/stack.sh env) for the database-backed tests.
# Builds into target/prove (this copy's own target directory); the scratch source is
# deleted at the end.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
cd "$(dirname "$0")/.."
here="$PWD"
out=""
case "${1:-}" in
  "") ;;
  --out) out="${2:?--out needs a file}" ;;
  *) echo "prove-limits.sh: unknown argument $1 (see --help)" >&2; exit 2 ;;
esac
: "${BOOKING_TEST_DATABASE_URL:?is not set. Start the database and export its address: scripts/stack.sh up; eval \"\$(scripts/stack.sh env)\"}"

work="$(mktemp -d "${TMPDIR:-/tmp}/booking-prove.XXXXXX")"
trap 'rm -rf "$work"' EXIT
tar --exclude ./target --exclude ./.git -cf - . | tar -xf - -C "$work"
export CARGO_TARGET_DIR="$here/target/prove"

# name|file|pattern=>replacement (a Python regex, applied to the marked line)|package|test target|test
mutations=(
  "connection-cap|crates/booking-http/src/serve.rs|limits\.max_connections=>Semaphore::MAX_PERMITS|booking-http|limits|connections_past_the_cap_wait_in_the_backlog_until_a_slot_frees"
  "header-read|crates/booking-http/src/serve.rs|^.*$=>|booking-http|limits|a_trickled_header_is_cut_off_and_never_answered"
  "requests-per-connection|crates/booking-http/src/serve.rs|nth >= cap=>false|booking-http|limits|pipelined_requests_past_the_cap_close_the_connection"
  "write-stall|crates/booking-http/src/serve.rs|limits\.write_stall=>Duration::from_secs(3600)|booking-http|limits|non_reading_clients_cannot_hold_connection_slots"
  "lifetime|crates/booking-http/src/serve.rs|limits\.max_age=>Duration::from_secs(3600)|booking-http|limits|a_busy_connection_is_closed_after_its_lifetime"
  "drain|crates/booking-http/src/serve.rs|limits\.drain_timeout=>Duration::from_secs(3600)|booking-http|limits|shutdown_lets_a_request_finish_and_cuts_at_its_deadline"
  "in-flight|crates/booking-http/src/stack.rs|max_in_flight\.max\(1\)=>Semaphore::MAX_PERMITS|booking-http|limits|requests_past_the_in_flight_gate_are_shed_at_once"
  "request-timeout|crates/booking-http/src/stack.rs|limits\.request_timeout=>Duration::from_secs(3600)|booking-http|limits|a_stalled_body_is_answered_by_the_request_deadline"
  "body|crates/booking-http/src/stack.rs|limits\.max_body_bytes=>usize::MAX|booking-http|limits|an_oversized_body_is_refused_with_the_envelope"
  "per-key-table|crates/booking-http/src/budget.rs|^.*$=>|booking-http|lib|budget::tests::the_table_never_grows_past_its_capacity_and_keeps_the_throttled"
  "sign-in-per-address|crates/booking-accounts/src/signin.rs|self\.per_address\.take\(address\)\?;=>let _ = self.per_address.take(address);|booking-accounts|budgets|one_address_flood_bounds_only_that_address"
  "sign-in-global|crates/booking-accounts/src/signin.rs|self\.global\.take\(\)=>Ok::<(), RetryAfter>(())|booking-accounts|budgets|a_distributed_flood_cannot_lock_out_a_trusted_owner"
  "mail-per-address|crates/booking-accounts/src/signin.rs|self\.per_address\.take\(address\)\?;=>let _ = self.per_address.take(address);|booking-accounts|budgets|mail_a_client_can_cause_is_bounded"
  "password-concurrency|crates/booking-accounts/src/password.rs|concurrency\.max\(1\)=>Semaphore::MAX_PERMITS|booking-accounts|lib|password::tests::at_most_the_concurrency_hashes_at_once"
  "plan|crates/booking-accounts/src/domain.rs|current < i64::from\(limit\)=>true|booking-accounts|limits|token_and_workspace_limits_hold_under_concurrency"
  "ready-deadline|crates/booking-server/src/health.rs|self\.deadline=>Duration::from_secs(3600)|booking-server|health|readyz_reports_a_silent_database_within_its_deadline"
  "availability-days|crates/availability/src/lib.rs|= 62;=>= 63;|availability|dst|queries_out_of_bounds_are_refused"
  "availability-staff|crates/availability/src/lib.rs|= 1_000;=>= 1_001;|availability|dst|too_many_staff_are_refused"
  "services|crates/booking-scheduling/src/domain.rs|= 200;=>= i64::MAX;|booking-scheduling|limits|services_are_capped_per_business"
  "staff|crates/booking-scheduling/src/domain.rs|= 100;=>= i64::MAX;|booking-scheduling|limits|staff_are_capped_per_business"
  "exceptions|crates/booking-scheduling/src/domain.rs|= 1_000;=>= i64::MAX;|booking-scheduling|limits|exceptions_are_capped_per_business"
  "weekly-hours|crates/booking-scheduling/src/domain.rs|= 50;=>= usize::MAX;|booking-scheduling|lib|domain::tests::weekly_overlaps_are_refused_touching_is_not"
  "availability-read-days|crates/booking-scheduling/src/domain.rs|= 31;=>= u32::MAX;|booking-scheduling|onboarding|a_business_is_set_up_and_offers_slots_across_a_dst_change"
  "slots|crates/booking-scheduling/src/domain.rs|= 5_000;=>= usize::MAX;|booking-scheduling|limits|an_availability_read_returns_at_most_its_cap_and_says_so"
  "availability-cells|crates/booking-scheduling/src/routes.rs|s\.limits\.max_cells=>u64::MAX|booking-scheduling|budgets|a_read_over_the_work_budget_is_refused_before_the_engine_runs"
  "availability-read-rate|crates/booking-scheduling/src/routes.rs|rate_limited\(&s\.reads, principal\)\?;=>let _ = rate_limited(&s.reads, principal);|booking-scheduling|budgets|availability_reads_are_rate_limited_per_principal"
  "hold-rate|crates/booking-scheduling/src/routes.rs|rate_limited\(&s\.holds, principal\)\?;=>let _ = rate_limited(&s.holds, principal);|booking-scheduling|budgets|holds_are_rate_limited_per_principal"
  "open-holds|crates/booking-scheduling/src/store.rs|open > i64::from\(open_holds\)=>false|booking-scheduling|budgets|open_holds_are_capped_per_principal_even_concurrently"
  "hold-start-window|crates/booking-scheduling/src/domain.rs|!\(-HOLD_START_PAST_SECONDS\.\.=HOLD_START_AHEAD_SECONDS\)\.contains\(&offset\)=>false|booking-scheduling|lib|domain::tests::a_hold_starts_within_a_day_before_now_to_366_days_after"
  "availability-work|crates/availability/src/lib.rs|\.checked_sub\(units\)\.ok_or\(Error::TooMuchWork\)\?=>.saturating_sub(units)|booking-scheduling|budgets|exceptions_count_in_the_work_budget"
  "busy-rows|crates/availability/src/lib.rs|member\.busy\.len\(\)=>0|booking-scheduling|budgets|busy_rows_count_in_the_work_budget"
  "compute-share|crates/booking-scheduling/src/compute.rs|share\.clamp\(1, permits\)=>permits|booking-scheduling|budgets|one_workspace_cannot_hold_every_compute_permit"
  "business-lock|crates/booking-scheduling/src/store.rs|FOR NO KEY UPDATE=>FOR UPDATE|booking-scheduling|locks|a_staff_write_and_a_hold_on_the_same_staff_never_deadlock"
  "staff-queue|crates/booking-scheduling/src/queue.rs|Some\(guard\)=>{ drop(guard); None }|booking-scheduling|locks|holds_for_one_staff_member_queue_in_memory_not_on_connections"
  "staff-lock|crates/booking-scheduling/src/store.rs| FOR UPDATE\"=>\"|booking-scheduling|locks|a_hold_waits_on_the_staff_lock_and_the_constraint_refuses_the_overlap"
  "principal-lock|crates/booking-scheduling/src/store.rs|pg_advisory_xact_lock\(=>pg_try_advisory_xact_lock(|booking-scheduling|locks|the_open_hold_cap_counts_a_concurrent_hold_under_the_principal_lock"
  "no-overlap|migrations/0003_scheduling.sql|blocked WITH &&=>blocked WITH =|booking-scheduling|locks|a_hold_waits_on_the_staff_lock_and_the_constraint_refuses_the_overlap"
  "booking-list|crates/booking-scheduling/src/domain.rs|= 500;=>= i64::MAX;|booking-scheduling|limits|a_booking_list_returns_at_most_its_cap"
)

# Every marker in the source has exactly one mutation, and every mutation a marker.
found="$(grep -rhoE 'LIMIT\([a-z-]+\)' crates/*/src migrations | sed 's/LIMIT(\(.*\))/\1/' | sort -u)"
defined="$(printf '%s\n' "${mutations[@]}" | cut -d'|' -f1 | sort -u)"
if [ "$found" != "$defined" ]; then
  echo "error: markers and mutations differ:" >&2
  diff <(echo "$found") <(echo "$defined") >&2 || true
  exit 1
fi

mutate() { # file marker pattern replacement
  python3 -I - "$work/$1" "$2" "$3" "$4" <<'PY'
import re, sys
path, marker, pattern, replacement = sys.argv[1:]
lines = open(path).read().split("\n")
tag = f"LIMIT({marker})"
changed = 0
for i, line in enumerate(lines):
    if tag not in line:
        continue
    target = i if line.split("//")[0].strip() else i + 1
    new = re.sub(pattern, replacement, lines[target], count=1)
    if new != lines[target]:
        lines[target] = new
        changed += 1
if not changed:
    sys.exit(f"the mutation for {marker} changed nothing")
open(path, "w").write("\n".join(lines))
PY
}

failures=0
results=()
for row in "${mutations[@]}"; do
  IFS='|' read -r name file subst package target test <<<"$row"
  pattern="${subst%%=>*}"
  replacement="${subst#*=>}"
  cp "$here/$file" "$work/$file"
  mutate "$file" "$name" "$pattern" "$replacement"
  if [ "$target" = lib ]; then selector=(--lib); else selector=(--test "$target"); fi
  started=$(date +%s)
  set +e
  output="$(cd "$work" && cargo test --locked -q -p "$package" "${selector[@]}" -- --exact "$test" 2>&1)"
  status=$?
  set -e
  seconds=$(( $(date +%s) - started ))
  cp "$here/$file" "$work/$file"
  if [ "$status" -ne 0 ] && grep -q 'test result: FAILED' <<<"$output"; then
    verdict=proven
    printf '  proven   %-24s %s fails without it (%ss)\n' "$name" "$test" "$seconds"
  elif [ "$status" -ne 0 ]; then
    verdict=broken
    failures=$((failures + 1))
    printf '  BROKEN   %-24s the mutation did not build or the test did not run:\n%s\n' "$name" "$(tail -20 <<<"$output")"
  else
    verdict=unproven
    failures=$((failures + 1))
    printf '  UNPROVEN %-24s %s passes without the limit\n' "$name" "$test"
  fi
  results+=("{\"limit\":\"$name\",\"test\":\"$test\",\"verdict\":\"$verdict\",\"seconds\":$seconds}")
done

if [ -n "$out" ]; then
  commit="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
  dirty=false; git diff --quiet HEAD -- . 2>/dev/null || dirty=true
  load="$(uptime | sed 's/.*load averages*: *//')"
  printf '{"proof":"limits","commit":"%s","tree_dirty":%s,"load_average":"%s","os":"%s","results":[%s]}\n' \
    "$commit" "$dirty" "$load" "$(uname -sr)" "$(IFS=,; echo "${results[*]}")" >"$out"
  echo "record: $out"
fi
if (( failures )); then
  echo "error: $failures of ${#mutations[@]} limits are not proven" >&2
  exit 1
fi
echo "limits ok: all ${#mutations[@]} limits are proven by a test that fails without them"
