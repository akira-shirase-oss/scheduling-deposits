// Availability reads: what a booking page asks most. Token-authenticated
// GET …/availability for one service across a seeded business (10 staff, open around the
// clock, a 15-minute grid, one live hold per staff member per day ahead), for a random day in the
// next 28 and one in ten reads covering a week, at a constant arrival rate (an open model:
// k6 keeps offering load whether or not the server keeps up, and counts what it could not
// send as dropped).
//
// Thresholds were written before the first run (D-012) and are not tuned to a result.
//
// Env: BASE_URL, TOKEN, WORKSPACE, SERVICE, DATES (comma-separated YYYY-MM-DD), MODE
// (smoke | full), RATE (requests/s); TOTAL for plumbing probes only.
import http from 'k6/http';
import { check } from 'k6';
import { Counter, Rate } from 'k6/metrics';

const MODE = __ENV.MODE || 'smoke';
const RATE = parseInt(__ENV.RATE || '1000', 10);
const TOTAL = __ENV.TOTAL ? parseInt(__ENV.TOTAL, 10) : MODE === 'full' ? 1000000 : 100000;
const BASE = `${__ENV.BASE_URL}/v1/workspaces/${__ENV.WORKSPACE}/availability?service=${__ENV.SERVICE}`;
const DATES = (__ENV.DATES || '').split(',');
const AUTH = { headers: { Authorization: `Bearer ${__ENV.TOKEN}` } };

const shed = new Rate('shed');
const unexpected = new Counter('unexpected_status');
const empty = new Counter('empty_answers'); // a day with no slots: the seed is broken

export const options = {
  summaryTrendStats: ['min', 'med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max'],
  scenarios: {
    availability: {
      executor: 'constant-arrival-rate',
      rate: RATE,
      timeUnit: '1s',
      duration: `${Math.ceil(TOTAL / RATE)}s`,
      preAllocatedVUs: 100,
      maxVUs: 1000,
    },
  },
  thresholds: {
    http_req_failed: ['rate<0.001'],
    'http_req_duration{days:1}': ['p(95)<50', 'p(99)<100'],
    'http_req_duration{days:7}': ['p(99)<250'],
    shed: ['rate<0.001'],
    unexpected_status: ['count<1'],
    empty_answers: ['count<1'],
    dropped_iterations: ['count<1'],
  },
};

export default function () {
  const date = DATES[Math.floor(Math.random() * DATES.length)];
  const days = Math.random() < 0.1 ? 7 : 1;
  const res = http.get(`${BASE}&from=${date}&days=${days}`, { ...AUTH, tags: { days: `${days}` } });
  shed.add(res.status === 503);
  if (!check(res, { 'status 200': (r) => r.status === 200 })) {
    if (res.status !== 503) unexpected.add(1);
    return;
  }
  if (res.body.indexOf('"start"') < 0) empty.add(1);
}
