// Hold contention: many customers racing for the same few slots. Session-authenticated
// POST …/holds, each with a fresh idempotency key, for a start drawn from a small hot set
// (HOT slots across the seeded staff), at a constant arrival rate. Almost every request
// loses to the exclusion constraint (409 slot_taken): this measures the losing path and
// that the database stays the only arbiter under load. scripts/load.sh then checks the
// invariant itself: no two live bookings of one staff member overlap, and at most one
// hold was created per hot slot.
//
// Thresholds were written before the first run (D-012) and are not tuned to a result.
//
// Env: BASE_URL, SESSION, WORKSPACE, SERVICE, SLOTS (comma-separated staff@RFC3339 pairs),
// MODE (smoke | full), RATE (requests/s); TOTAL for plumbing probes only.
import http from 'k6/http';
import { check } from 'k6';
import { Counter, Rate } from 'k6/metrics';
import exec from 'k6/execution';

const MODE = __ENV.MODE || 'smoke';
const RATE = parseInt(__ENV.RATE || '1000', 10);
const TOTAL = __ENV.TOTAL ? parseInt(__ENV.TOTAL, 10) : MODE === 'full' ? 1000000 : 100000;
const URL = `${__ENV.BASE_URL}/v1/workspaces/${__ENV.WORKSPACE}/holds`;
const SLOTS = (__ENV.SLOTS || '').split(',').map((s) => s.split('@'));
const PARAMS = {
  headers: { Cookie: `booking_session=${__ENV.SESSION}`, 'content-type': 'application/json' },
  responseCallback: http.expectedStatuses(201, 409),
};

const shed = new Rate('shed');
const unexpected = new Counter('unexpected_status');
const won = new Counter('holds_created');
const lost = new Counter('slot_taken');

export const options = {
  summaryTrendStats: ['min', 'med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max'],
  scenarios: {
    contention: {
      executor: 'constant-arrival-rate',
      rate: RATE,
      timeUnit: '1s',
      duration: `${Math.ceil(TOTAL / RATE)}s`,
      preAllocatedVUs: 1000,
      maxVUs: 1000,
    },
  },
  thresholds: {
    http_req_failed: ['rate<0.001'],
    http_req_duration: ['p(95)<50', 'p(99)<100'],
    shed: ['rate<0.001'],
    unexpected_status: ['count<1'],
    dropped_iterations: ['count<1'],
    // The guarantee: never more winners than slots (a hold lasts longer than the run's
    // slice of each slot, so each hot slot is won at most once).
    holds_created: [`count<=${SLOTS.length}`],
  },
};

export default function () {
  const [staff, start] = SLOTS[Math.floor(Math.random() * SLOTS.length)];
  const key = `load-${exec.vu.idInTest}-${exec.scenario.iterationInTest}`;
  const body = JSON.stringify({
    service_id: __ENV.SERVICE, staff_id: staff, starts_at: start,
    customer_name: 'Load', idempotency_key: key,
  });
  const res = http.post(URL, body, PARAMS);
  shed.add(res.status === 503);
  if (res.status === 201) {
    won.add(1);
  } else if (res.status === 409 && res.body.indexOf('slot_taken') >= 0) {
    lost.add(1);
  } else if (!check(res, { 'status 201 or 409 slot_taken': () => false }) && res.status !== 503) {
    unexpected.add(1);
  }
}
