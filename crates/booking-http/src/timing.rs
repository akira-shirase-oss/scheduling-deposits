//! Server-side timing: fixed log-scale histograms of where a request's time goes, kept in
//! atomics (no lock, no allocation per sample), reported once at drain so a load run can
//! tell server time from the generator's. Nothing here is served: the numbers leave only
//! through the server's own log.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Bucket `i` counts samples below `BASE_MICROS × 2^(i/4)`; the last bucket is open.
const BUCKETS: usize = 80;
const BASE_MICROS: f64 = 10.0;

/// A stage of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The handler, from admission past the in-flight gate to its response.
    Request,
    /// Authentication and authorization (`Accounts::access`).
    Auth,
    /// Waiting for a pooled database connection.
    PoolWait,
    /// Database work with a connection held.
    Db,
    /// Waiting for a compute permit (availability).
    ComputeWait,
    /// The availability engine and the answer's encoding.
    Compute,
}

impl Stage {
    /// Every stage, in report order.
    pub const ALL: [Self; 6] = [
        Self::Request,
        Self::Auth,
        Self::PoolWait,
        Self::Db,
        Self::ComputeWait,
        Self::Compute,
    ];

    /// The stage's name in the report.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Auth => "auth",
            Self::PoolWait => "pool_wait",
            Self::Db => "db",
            Self::ComputeWait => "compute_wait",
            Self::Compute => "compute",
        }
    }
}

/// One histogram.
#[derive(Debug)]
pub struct Histogram {
    counts: [AtomicU64; BUCKETS],
    total_micros: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            total_micros: AtomicU64::new(0),
        }
    }
}

fn bucket_of(micros: f64) -> usize {
    if micros < BASE_MICROS {
        return 0;
    }
    let i = ((micros / BASE_MICROS).log2() * 4.0).floor() + 1.0;
    #[expect(clippy::cast_precision_loss, reason = "a bucket index is small")]
    let last = (BUCKETS - 1) as f64;
    if i >= last {
        BUCKETS - 1
    } else {
        // In range and non-negative: checked above.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "bounded above"
        )]
        let i = i as usize;
        i
    }
}

fn upper_micros(bucket: usize) -> f64 {
    #[expect(clippy::cast_precision_loss, reason = "a bucket index is small")]
    let i = bucket as f64;
    BASE_MICROS * (i / 4.0).exp2()
}

impl Histogram {
    /// Records one sample.
    pub fn record(&self, elapsed: Duration) {
        let micros = elapsed.as_secs_f64() * 1e6;
        self.counts[bucket_of(micros)].fetch_add(1, Ordering::Relaxed);
        self.total_micros.fetch_add(
            u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// Samples recorded.
    pub fn count(&self) -> u64 {
        self.counts.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }

    /// The `q` quantile (0..=1) in milliseconds, as its bucket's upper bound (at most 19%
    /// above the true value); 0 with no samples.
    pub fn quantile_ms(&self, q: f64) -> f64 {
        let counts: Vec<u64> = self
            .counts
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return 0.0;
        }
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a rank within the count"
        )]
        let rank = ((total as f64) * q.clamp(0.0, 1.0)).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, c) in counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return upper_micros(i) / 1000.0;
            }
        }
        upper_micros(BUCKETS - 1) / 1000.0
    }

    /// The report: count, mean and quantiles in milliseconds.
    pub fn report(&self) -> serde_json::Value {
        let count = self.count();
        #[expect(clippy::cast_precision_loss, reason = "a report")]
        let mean = if count == 0 {
            0.0
        } else {
            self.total_micros.load(Ordering::Relaxed) as f64 / count as f64 / 1000.0
        };
        serde_json::json!({
            "count": count,
            "mean_ms": round3(mean),
            "p50_ms": round3(self.quantile_ms(0.50)),
            "p95_ms": round3(self.quantile_ms(0.95)),
            "p99_ms": round3(self.quantile_ms(0.99)),
        })
    }
}

fn round3(ms: f64) -> f64 {
    (ms * 1000.0).round() / 1000.0
}

/// Every stage's histogram. Cheap to share (`&'static` through [`global`]).
#[derive(Debug, Default)]
pub struct Timings {
    stages: [Histogram; Stage::ALL.len()],
}

impl Timings {
    /// The histogram of `stage`.
    pub fn of(&self, stage: Stage) -> &Histogram {
        &self.stages[stage as usize]
    }

    /// Records `elapsed` for `stage`.
    pub fn record(&self, stage: Stage, elapsed: Duration) {
        self.of(stage).record(elapsed);
    }

    /// The whole report, one object per stage.
    pub fn report(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for stage in Stage::ALL {
            map.insert(stage.name().to_owned(), self.of(stage).report());
        }
        serde_json::Value::Object(map)
    }
}

/// The process's timings.
pub fn global() -> &'static Timings {
    static TIMINGS: std::sync::OnceLock<Timings> = std::sync::OnceLock::new();
    TIMINGS.get_or_init(Timings::default)
}

/// Times `future` as `stage` in the process's timings.
pub async fn timed<T>(stage: Stage, future: impl Future<Output = T>) -> T {
    let started = Instant::now();
    let out = future.await;
    global().record(stage, started.elapsed());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_land_in_the_right_bucket() {
        let h = Histogram::default();
        assert!(h.quantile_ms(0.5).abs() < f64::EPSILON);
        for _ in 0..90 {
            h.record(Duration::from_micros(1_000));
        }
        for _ in 0..10 {
            h.record(Duration::from_millis(300));
        }
        let p50 = h.quantile_ms(0.5);
        assert!((1.0..1.19 + 1e-9).contains(&p50), "{p50}");
        let p95 = h.quantile_ms(0.95);
        assert!((300.0..357.0).contains(&p95), "{p95}");
        assert_eq!(h.count(), 100);
        h.record(Duration::from_secs(10_000));
        assert!(h.quantile_ms(1.0) > 0.0);
    }
}
