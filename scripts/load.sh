#!/usr/bin/env bash
# Runs a scenario against a release build and keeps the evidence (the workspace's
# load-test standard: a scenario per feature that serves traffic, full runs of at least
# 1,000,000 requests, results with machine and commit, misses kept).
#
#   MODE=smoke scripts/load.sh        100,000 requests
#   MODE=full  scripts/load.sh        1,000,000 requests: only on a quiet machine
#   scripts/load.sh --setup-only      build, set up, seed and check, offer no load
#   SCENARIO=service|availability-read|hold-contention   (default service; perf/scenarios/)
#
# availability-read and hold-contention also seed a business through the API (UTC, 10
# staff open around the clock, one 30-minute service with 15 minutes after, one hold per
# staff member per day ahead); hold-contention afterwards checks the guarantee in the
# database: no two live bookings of a staff member overlap, one winner per hot slot.
#
# It makes a fresh database (bootstrap, migrate), starts a fake SMTP sink and the server on
# 127.0.0.1 (BOOKING_LOAD_PORT, default 63552, and that + 6 for the sink), seeds one
# account, workspace and API token through the API,
# offers load with k6 at RATE requests/s (default 1000), and writes the result: the machine
# (CPU, cores, memory, OS: never the host's name), the commit and whether the tree was
# dirty, the load average before and after (above 6 it is a busy-machine observation), the
# thresholds and the latencies. A threshold miss fails the run and is kept, never re-run
# until green. TOTAL=N makes a plumbing probe, written under target/perf, never perf/results.
#
# Needs BOOKING_TEST_DATABASE_URL (an administrator), k6 (scripts/install-k6.sh), jq, curl.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
cd "$(dirname "$0")/.."
. scripts/drills/lib.sh

setup_only=false
case "${1:-}" in
  "") ;;
  --setup-only) setup_only=true ;;
  *) echo "load.sh: unknown argument $1 (see --help)" >&2; exit 2 ;;
esac
mode="${MODE:-smoke}"
scenario="${SCENARIO:-service}"
case "$scenario" in service|availability-read|hold-contention) ;; *) echo "SCENARIO must be service, availability-read or hold-contention" >&2; exit 2 ;; esac
rate="${RATE:-1000}"
case "$mode" in smoke|full) ;; *) echo "MODE must be smoke or full" >&2; exit 2 ;; esac
# Both reach shell arithmetic, which evaluates expressions (and command substitutions in
# array subscripts): digits only, checked before anything else runs.
[[ "$rate" =~ ^[0-9]+$ ]] && (( 10#$rate > 0 )) || { echo "RATE must be a positive whole number" >&2; exit 2; }
[[ -z "${TOTAL:-}" || "${TOTAL}" =~ ^[0-9]+$ ]] || { echo "TOTAL must be a whole number" >&2; exit 2; }
require_database tools
port="${BOOKING_LOAD_PORT:-63552}"
sink_port=$((port + 6))
base="http://127.0.0.1:$port"
db="booking_load_$(date -u +%Y%m%d%H%M%S)_$$"
work="$(mktemp -d "${TMPDIR:-/tmp}/booking-load.XXXXXX")"
pids=()
cleanup() {
  for pid in ${pids[@]+"${pids[@]}"}; do kill "$pid" 2>/dev/null || true; done
  pg psql -d postgres -XAtq -c "DROP DATABASE IF EXISTS $db WITH (FORCE)" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

cargo build --quiet --release --locked -p booking-server
cargo build --quiet --release --locked -p booking-accounts --example smtp-sink --features test-support
STARTERCTL=target/release/bookingctl
ctl "$db" bootstrap >/dev/null
ctl "$db" migrate >/dev/null
target/release/examples/smtp-sink "127.0.0.1:$sink_port" "$work/mail.log" >"$work/sink.log" 2>&1 &
pids+=($!)
# One session and one token drive the whole run: their per-principal budgets (D-018) are
# opened so the run measures the booking path; tests/budgets.rs proves the budgets.
BOOKING_DATABASE_URL="$(role_url booking_app "$APP_PW" "$db")" BOOKING_LISTEN="127.0.0.1:$port" \
  BOOKING_SMTP_RELAY="127.0.0.1:$sink_port" BOOKING_PUBLIC_URL="$base" BOOKING_COOKIE_SECURE=false \
  BOOKING_READ_RATE_PER_SECOND=100000 BOOKING_HOLD_RATE_PER_SECOND=100000 BOOKING_OPEN_HOLDS_PER_PRINCIPAL=1000000 \
  BOOKING_LOG=warn BOOKING_MAX_IN_FLIGHT="${MAX_IN_FLIGHT:-256}" target/release/booking-server >"$work/server.log" 2>&1 &
server=$!
pids+=($server)
for _ in $(seq 1 120); do curl -fsS -o /dev/null "$base/readyz" 2>/dev/null && break; sleep 0.25; done
curl -fsS -o /dev/null "$base/readyz"

api() { curl -sS --max-time 10 -X "$1" -b "$work/cookies" -c "$work/cookies" -H 'content-type: application/json' ${3:+--data "$3"} "$base$2"; }
invite="$(ctl "$db" invite 1 1)"
api POST /v1/signup "{\"invite\":\"$invite\",\"email\":\"load@example.test\",\"password\":\"a load-test password\"}" >/dev/null
sleep 0.5
verify="$(grep 'to=load@example.test' "$work/mail.log" | tail -1 | sed 's/.*token=//')"
api POST /v1/verify "{\"token\":\"$verify\"}" >/dev/null
api POST /v1/session '{"email":"load@example.test","password":"a load-test password"}' >/dev/null
workspace="$(api POST /v1/workspaces '{"name":"Load"}' | jq -r '.id')"
token="$(api POST "/v1/workspaces/$workspace/tokens" '{"name":"load"}' | jq -r '.token')"
status="$(curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $token" "$base/v1/workspaces/$workspace/hello")"
[ "$status" = 200 ] || { echo "error: the seeded token read answered $status" >&2; exit 1; }
session="$(awk '$6 == "booking_session" { print $7 }' "$work/cookies")"
service="" dates="" slots="" set_a="" set_b=""
if [ "$scenario" != service ]; then
  w="/v1/workspaces/$workspace"
  api PUT "$w/business" '{"name":"Load","time_zone":"UTC","currency":"EUR","slot_step_minutes":15,"min_notice_minutes":0,"max_advance_days":365,"hold_minutes":'"$([ "$scenario" = hold-contention ] && echo 1 || echo 60)"'}' >/dev/null
  service="00000000-0000-7000-8000-000000000001"
  api PUT "$w/services/$service" '{"name":"Load","duration_minutes":30,"buffer_after_minutes":15,"price_minor":4000,"deposit":{"kind":"percent","percent":25}}' | jq -e '.id' >/dev/null
  hours="$(jq -cn '[range(1; 8) | {weekday: ., start: "00:00", end: "24:00"}]')"
  for i in $(seq 1 10); do
    staff="00000000-0000-7000-9000-$(printf '%012d' "$i")"
    api PUT "$w/staff/$staff" "{\"name\":\"Staff $i\"}" | jq -e '.id' >/dev/null
    api PUT "$w/staff/$staff/services" "{\"service_ids\":[\"$service\"]}" >/dev/null
    api PUT "$w/staff/$staff/hours" "{\"hours\":$hours}" >/dev/null
    for d in $(seq 1 28); do
      day="$(date -u -d "+$d day" +%F)"
      api POST "$w/holds" "{\"service_id\":\"$service\",\"staff_id\":\"$staff\",\"starts_at\":\"${day}T12:00:00Z\",\"customer_name\":\"Seed\",\"idempotency_key\":\"seed-$i-$d\"}" | jq -e '.id' >/dev/null
    done
    # Hot starts an hour apart: a 30-minute service with 15 minutes after never overlaps
    # the next, so every hot slot has exactly one winner per round.
    for t in 06:00 07:00 08:00 09:00 10:00; do
      set_a="${set_a:+$set_a,}$staff@$(date -u -d '+3 day' +%F)T$t:00Z"
      set_b="${set_b:+$set_b,}$staff@$(date -u -d '+4 day' +%F)T$t:00Z"
    done
  done
  hot="$(date -u -d '+3 day' +%F)"
  slots="$set_a;$set_b"
  dates="$(for d in $(seq 1 28); do date -u -d "+$d day" +%F; done | paste -sd, -)"
  status="$(curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $token" "$base$w/availability?service=$service&from=${hot}")"
  [ "$status" = 200 ] || { echo "error: the seeded availability read answered $status" >&2; exit 1; }
fi
worst_workspace="" worst_token="" exc_workspace="" exc_token="" exc_from=""
if [ "$scenario" = availability-read ]; then
  # The D-011 worst case: 100 staff open 00:00-24:00 on a 5-minute grid, read 10 days at a
  # time without staff= (288,000 grid points, under the default work budget of 300,000).
  worst_workspace="$(api POST /v1/workspaces '{"name":"Worst"}' | jq -r '.id')"
  worst_token="$(api POST "/v1/workspaces/$worst_workspace/tokens" '{"name":"load"}' | jq -r '.token')"
  w="/v1/workspaces/$worst_workspace"
  api PUT "$w/business" '{"name":"Worst","time_zone":"UTC","currency":"EUR","slot_step_minutes":5,"min_notice_minutes":0,"max_advance_days":365,"hold_minutes":60}' >/dev/null
  api PUT "$w/services/$service" '{"name":"Worst","duration_minutes":30,"buffer_after_minutes":15,"price_minor":4000,"deposit":{"kind":"percent","percent":25}}' | jq -e '.id' >/dev/null
  for i in $(seq 1 100); do
    staff="00000000-0000-7000-a000-$(printf '%012d' "$i")"
    api PUT "$w/staff/$staff" "{\"name\":\"Staff $i\"}" | jq -e '.id' >/dev/null
    api PUT "$w/staff/$staff/services" "{\"service_ids\":[\"$service\"]}" >/dev/null
    api PUT "$w/staff/$staff/hours" "{\"hours\":$hours}" >/dev/null
  done
  status="$(curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $worst_token" "$base$w/availability?service=$service&from=${hot}&days=10")"
  [ "$status" = 200 ] || { echo "error: the worst-case availability read answered $status" >&2; exit 1; }
  # The worst case by exceptions: 100 staff open 00:00-24:00 on a 60-minute grid, each with
  # 10 closures spanning every read's window (1,000 exceptions, the business maximum) that
  # leave only 24-minute gaps: no slot fits, so a 31-day read walks all 74,400 grid points.
  exc_workspace="$(api POST /v1/workspaces '{"name":"Exceptions"}' | jq -r '.id')"
  exc_token="$(api POST "/v1/workspaces/$exc_workspace/tokens" '{"name":"load"}' | jq -r '.token')"
  w="/v1/workspaces/$exc_workspace"
  api PUT "$w/business" '{"name":"Exceptions","time_zone":"UTC","currency":"EUR","slot_step_minutes":60,"min_notice_minutes":0,"max_advance_days":365,"hold_minutes":60}' >/dev/null
  api PUT "$w/services/$service" '{"name":"Exceptions","duration_minutes":30,"buffer_after_minutes":15,"price_minor":4000,"deposit":{"kind":"percent","percent":25}}' | jq -e '.id' >/dev/null
  exc_first="$(date -u -d '-2 day' +%F)" exc_last="$(date -u -d '+70 day' +%F)" exc_from="$(date -u -d '+1 day' +%F)"
  for i in $(seq 1 100); do
    staff="00000000-0000-7000-b000-$(printf '%012d' "$i")"
    api PUT "$w/staff/$staff" "{\"name\":\"Staff $i\"}" | jq -e '.id' >/dev/null
    api PUT "$w/staff/$staff/services" "{\"service_ids\":[\"$service\"]}" >/dev/null
    api PUT "$w/staff/$staff/hours" "{\"hours\":$hours}" >/dev/null
    for k in $(seq 0 9); do
      from=$(( k * 144 )) until=$(( k * 144 + 120 ))
      api PUT "$w/exceptions/00000000-0000-7000-c$(printf '%03d' "$k")-$(printf '%012d' "$i")" \
        "{\"staff_id\":\"$staff\",\"first_date\":\"$exc_first\",\"last_date\":\"$exc_last\",\"kind\":\"closed\",\"start\":\"$(printf '%02d:%02d' $(( from / 60 )) $(( from % 60 )))\",\"end\":\"$(printf '%02d:%02d' $(( until / 60 )) $(( until % 60 )))\"}" | jq -e '.id' >/dev/null
    done
  done
  answer="$(curl -sS -w '\n%{http_code}' -H "Authorization: Bearer $exc_token" "$base$w/availability?service=$service&from=${exc_from}&days=31")"
  [ "$(tail -1 <<<"$answer")" = 200 ] || { echo "error: the exceptions worst-case read answered $(tail -1 <<<"$answer")" >&2; exit 1; }
  head -1 <<<"$answer" | jq -e '.starts == [] and .truncated == false' >/dev/null || { echo "error: the exceptions worst case has a free slot" >&2; exit 1; }
fi
if $setup_only; then
  echo "setup ok ($scenario): server ready, account, workspace and token seeded; a token read answers 200 (no load offered)"
  exit 0
fi

stamp="$(date -u +%Y%m%dT%H%M%SZ)"
summary="$work/summary.json"
load_before="$(uptime | sed 's/.*load averages*: *//')"
set +e
BASE_URL="$base" TOKEN="$token" SESSION="$session" WORKSPACE="$workspace" SERVICE="$service" \
  DATES="$dates" SLOTS="$slots" WORST_WORKSPACE="$worst_workspace" WORST_TOKEN="$worst_token" \
  EXC_WORKSPACE="$exc_workspace" EXC_TOKEN="$exc_token" EXC_FROM="$exc_from" MODE="$mode" RATE="$rate" TOTAL="${TOTAL:-}" \
  k6 run --quiet --summary-export "$summary" "perf/scenarios/$scenario.js"
k6_status=$?
invariant=null
if [ "$scenario" = hold-contention ]; then
  # Ranges recomputed from the start and the service, not read from the trigger's column.
  overlaps="$(pg psql -d "$db" -XAtq -c "WITH r AS (
      SELECT k.business_id, k.staff_id, k.id, tstzrange(
               k.starts_at - make_interval(mins => s.buffer_before_minutes),
               k.starts_at + make_interval(mins => s.duration_minutes + s.buffer_after_minutes)) AS span
        FROM bookings k JOIN services s ON s.business_id = k.business_id AND s.id = k.service_id
       WHERE k.status IN ('held', 'confirmed'))
    SELECT count(*) FROM r a JOIN r b
      ON a.business_id = b.business_id AND a.staff_id = b.staff_id AND a.id < b.id AND a.span && b.span")"
  winners="$(pg psql -d "$db" -XAtq -c "SELECT count(*) FROM bookings WHERE idempotency_key LIKE 'load-%'")"
  expired="$(pg psql -d "$db" -XAtq -c "SELECT count(*) FROM bookings WHERE idempotency_key LIKE 'load-%' AND status = 'expired'")"
  hot_slots="$(tr ',' '\n' <<<"$set_a" | wc -l | tr -d ' ')"
  total="${TOTAL:-$([ "$mode" = full ] && echo 1000000 || echo 100000)}"
  per_round=$(( rate * 45 )); rounds=$(( (total + per_round - 1) / per_round ))
  expected=$(( rounds * hot_slots ))
  invariant="{\"overlapping_live_pairs\":$overlaps,\"holds_created\":$winners,\"holds_expected\":$expected,\"rounds\":$rounds,\"hot_slots\":$hot_slots,\"expired_by_the_database\":$expired}"
  if [ "$winners" != "$expected" ]; then echo "error: $winners holds created, $expected expected" >&2; k6_status=99; fi
  if (( rounds > 2 )) && [ "$expired" = 0 ]; then echo "error: no hold expired across $rounds rounds" >&2; k6_status=99; fi
  if [ "$overlaps" != 0 ]; then echo "error: $overlaps overlapping live bookings" >&2; k6_status=99; fi
fi
set -e
load_after="$(uptime | sed 's/.*load averages*: *//')"
rss_kb="$(ps -o rss= -p "$server" | tr -d ' ' || echo 0)"
# Server CPU over the whole run (user + system seconds), then a drain so the server logs its
# own request-time and pool-wait histograms: server time, told apart from the generator's.
server_cpu_s=0
if [ -r "/proc/$server/stat" ]; then
  server_cpu_s="$(awk -v hz="$(getconf CLK_TCK)" '{printf "%.1f", ($14 + $15) / hz}' "/proc/$server/stat")"
fi
kill -TERM "$server" 2>/dev/null || true
wait "$server" 2>/dev/null || true
server_timing="$(grep -F '"server timings"' "$work/server.log" | tail -1 | jq -c '.fields.report | fromjson' 2>/dev/null || echo null)"
[ -n "$server_timing" ] || server_timing=null
if [ "$(uname -s)" = Darwin ]; then
  cpu="$(sysctl -n machdep.cpu.brand_string)"; cores="$(sysctl -n hw.ncpu)"
  mem_gb="$(( $(sysctl -n hw.memsize) / 1073741824 ))"; os="macOS $(sw_vers -productVersion)"
else
  cpu="$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"; cores="$(nproc)"
  mem_gb="$(( $(grep MemTotal /proc/meminfo | awk '{print $2}') / 1048576 ))"; os="$(uname -sr)"
fi
commit="$(git rev-parse HEAD)"
dirty=false; git diff --quiet HEAD -- . ':!perf/results' || dirty=true
busy="$(awk '{gsub(",", "", $1); print ($1 > 6) ? "true" : "false"}' <<<"$load_before")"
outdir=perf/results
if [ -n "${TOTAL:-}" ]; then outdir=target/perf; mode="probe-${TOTAL}"; fi
mkdir -p "$outdir"
out="$outdir/${stamp}-${scenario}-${mode}.json"
case "$scenario" in
  service) mix="90% token-authenticated workspace reads, 10% /livez" ;;
  availability-read) mix="token-authenticated availability reads, 88% one day, 10% a week (28 dates, 10 staff), 1% the D-011 worst case (100 staff, 5-minute grid, 10 days), 1% the exceptions worst case (100 staff, 1,000 exceptions, 60-minute grid, 31 days, no free slot)" ;;
  hold-contention) mix="session-authenticated holds racing for 50 non-overlapping hot slots (10 staff x 5 starts) per 45 s round, two alternating sets, one-minute holds, fresh idempotency keys" ;;
esac
jq -n --arg mode "$mode" --arg stamp "$stamp" --arg commit "$commit" --argjson dirty "$dirty" \
  --arg cpu "$cpu" --arg cores "$cores" --arg mem_gb "$mem_gb" --arg os "$os" \
  --arg load_before "$load_before" --arg load_after "$load_after" --argjson busy "$busy" \
  --argjson rate "$rate" --argjson k6_status "$k6_status" --arg rss_kb "$rss_kb" \
  --arg scenario "$scenario" --arg mix "$mix" --argjson invariant "$invariant" \
  --argjson server_timing "$server_timing" --arg server_cpu_s "$server_cpu_s" \
  --slurpfile summary "$summary" '
  { scenario: $scenario, mode: $mode, started_utc: $stamp, commit: $commit, tree_dirty: $dirty,
    machine: { cpu: $cpu, cores: ($cores|tonumber), memory_gb: ($mem_gb|tonumber), os: $os,
               load_average_before: $load_before, load_average_after: $load_after, busy_machine: $busy,
               note: "same-machine load generator: a development observation, not capacity" },
    offered_rate_per_second: $rate,
    mix: $mix, invariant: $invariant,
    thresholds_passed: ($k6_status == 0), k6_exit_status: $k6_status,
    requests: $summary[0].metrics.http_reqs.count,
    requests_per_second: $summary[0].metrics.http_reqs.rate,
    failed_rate: $summary[0].metrics.http_req_failed.value,
    shed_rate: ($summary[0].metrics.shed.value // 0),
    dropped_iterations: ($summary[0].metrics.dropped_iterations.count // 0),
    latency_ms: ($summary[0].metrics.http_req_duration | {min, med, avg, "p(90)", "p(95)", "p(99)", max}),
    rss_kb_at_end: ($rss_kb|tonumber),
    server: { cpu_seconds: ($server_cpu_s|tonumber), timing_ms: $server_timing } }' >"$out"
echo "result: $out"
scripts/check-results.sh "$out" >/dev/null
jq '{scenario, mode, commit, invariant, thresholds_passed, requests, requests_per_second, failed_rate, shed_rate, dropped_iterations, latency_ms, server, machine: {load_average_before: .machine.load_average_before, busy_machine: .machine.busy_machine}}' "$out"
[ "$k6_status" -eq 0 ] || exit 1
