//! End-to-end lag: time between an event being received (received_at,
//! set by the database on insert) and its batch being applied.

use chrono::{NaiveDateTime, Utc};

#[derive(Debug, Clone, Copy, Default)]
pub struct LagStats {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

/// Current UTC time; the database runs with timezone = 'UTC'.
pub fn now_utc() -> NaiveDateTime {
    Utc::now().naive_utc()
}

fn ms_between(later: NaiveDateTime, earlier: NaiveDateTime) -> f64 {
    let delta = later - earlier;
    delta
        .num_microseconds()
        .map(|us| us as f64 / 1000.0)
        .unwrap_or_else(|| delta.num_milliseconds() as f64)
        .max(0.0)
}

/// Lag from receive timestamps already summarized by the database:
/// `oldest` = MIN(received_at), `p50`/`p05` = PERCENTILE_DISC(0.5 / 0.05).
pub fn from_percentiles(
    now: NaiveDateTime,
    oldest: NaiveDateTime,
    p50: NaiveDateTime,
    p05: NaiveDateTime,
) -> LagStats {
    LagStats {
        p50_ms: ms_between(now, p50),
        p95_ms: ms_between(now, p05),
        max_ms: ms_between(now, oldest),
    }
}

/// Lag from raw receive timestamps (project A batches are small).
pub fn from_samples(now: NaiveDateTime, mut received: Vec<NaiveDateTime>) -> LagStats {
    if received.is_empty() {
        return LagStats::default();
    }
    received.sort_unstable();
    let pick = |q: f64| {
        let idx = ((received.len() as f64 - 1.0) * q).round() as usize;
        received[idx.min(received.len() - 1)]
    };
    from_percentiles(now, received[0], pick(0.5), pick(0.05))
}
