// Availability reads: what a booking page asks most. Token-authenticated
// GET …/availability for one service across a seeded business (10 staff, open around the
// clock, a 15-minute grid, one live hold per staff member per day ahead), for a random day in the
// next 28 and one in ten reads covering a week, at a constant arrival rate (an open model:
// k6 keeps offering load whether or not the server keeps up, and counts what it could not
// send as dropped).
//
// Thresholds were written before the first run (D-012) and are not tuned to a result.
//
// Env: BASE_URL, TOKEN, WORKSPACE, SERVICE, WORST_WORKSPACE, WORST_TOKEN, EXC_WORKSPACE, EXC_TOKEN, EXC_FROM, DATES (comma-separated YYYY-MM-DD), MODE
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
const WORST_AUTH = { headers: { Authorization: `Bearer ${__ENV.WORST_TOKEN}` } };
const WORST = `${__ENV.BASE_URL}/v1/workspaces/${__ENV.WORST_WORKSPACE}/availability?service=${__ENV.SERVICE}`;
const EXC_AUTH = { headers: { Authorization: `Bearer ${__ENV.EXC_TOKEN}` } };
const EXC = `${__ENV.BASE_URL}/v1/workspaces/${__ENV.EXC_WORKSPACE}/availability?service=${__ENV.SERVICE}&from=${__ENV.EXC_FROM}&days=31`;

const shed = new Rate('shed');
const unexpected = new Counter('unexpected_status');
const empty = new Counter('empty_answers'); // a day with no slots: the seed is broken
const leaked = new Counter('free_slots_in_closed_fixture'); // the exceptions worst case must stay slotless

export const options = {
  summaryTrendStats: ['min', 'med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max'],
  scenarios: {
    availability: {
      executor: 'constant-arrival-rate',
      rate: RATE,
      timeUnit: '1s',
      duration: `${Math.ceil(TOTAL / RATE)}s`,
      preAllocatedVUs: 500,
      maxVUs: 500,
    },
  },
  thresholds: {
    http_req_failed: ['rate<0.001'],
    'http_req_duration{days:1}': ['p(95)<50', 'p(99)<100'],
    'http_req_duration{days:7}': ['p(99)<250'],
    // D-011's worst case (1% of reads): 100 staff, 5-minute grid, 10 days, one full page.
    'http_req_duration{days:worst}': ['p(99)<250'],
    // The worst case by exceptions (1% of reads): 100 staff, 1,000 exceptions spanning the
    // window, 60-minute grid, 31 days, no free slot, so every grid point is walked.
    'http_req_duration{days:worst_exceptions}': ['p(99)<250'],
    shed: ['rate<0.001'],
    unexpected_status: ['count<1'],
    empty_answers: ['count<1'],
    // The exceptions fixture must offer nothing: a free slot would stop the walk early.
    free_slots_in_closed_fixture: ['count<1'],
    dropped_iterations: ['count<1'],
  },
};

export default function () {
  const date = DATES[Math.floor(Math.random() * DATES.length)];
  const roll = Math.random();
  let tag;
  let res;
  if (roll < 0.01) {
    tag = 'worst';
    res = http.get(`${WORST}&from=${date}&days=10`, { ...WORST_AUTH, tags: { days: tag } });
  } else if (roll < 0.02) {
    tag = 'worst_exceptions';
    res = http.get(EXC, { ...EXC_AUTH, tags: { days: tag } });
  } else {
    tag = `${roll < 0.12 ? 7 : 1}`;
    res = http.get(`${BASE}&from=${date}&days=${tag}`, { ...AUTH, tags: { days: tag } });
  }
  shed.add(res.status === 503);
  if (!check(res, { 'status 200': (r) => r.status === 200 })) {
    if (res.status !== 503) unexpected.add(1);
    return;
  }
  // The exceptions worst case has no free slot by design; every other read has slots.
  if (tag === 'worst_exceptions') {
    if (res.body.indexOf('"at"') >= 0) leaked.add(1);
  } else if (res.body.indexOf('"at"') < 0) empty.add(1);
}
