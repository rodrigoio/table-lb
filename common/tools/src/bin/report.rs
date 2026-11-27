//! Builds the benchmark report of a campaign.
//!
//!   report <results/CAMPAIGN>
//!
//! Reads every run under <campaign>/<scenario>/<label>/rep-<n>/ (meta.env +
//! raw/ written by bench/run.sh) and writes:
//!   <campaign>/runs.csv     one row per run, every metric
//!   <campaign>/summary.md   per scenario: mean ± stddev per label
//!   <campaign>/charts/*.svg time series (repetition 1 of each label), bars,
//!                           WAL per event by statement and, for multi-burst
//!                           scenarios, per-burst bars (short vs long term).
//!                           Every chart prints under its title what it shows
//!                           and what it is for (also under the image in summary.md).
//!
//! Windows: latency and CPU use the steady window [load start + WARMUP,
//! load end]; counters (WAL, autovacuum, deadlocks) use [load start, drained].

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use hdrhistogram::Histogram;
use plotters::coord::Shift;
use plotters::prelude::*;
use plotters::style::text_anchor::{HPos, Pos, VPos};
use plotters::style::FontStyle;

// ---------------------------------------------------------------- run data

struct Run {
    dir: PathBuf,
    meta: HashMap<String, String>,
    scenario: String,
    label: String,
    rep: u32,
}

impl Run {
    fn meta_i64(&self, key: &str) -> Option<i64> {
        self.meta.get(key).and_then(|v| v.parse().ok())
    }
    fn raw(&self) -> PathBuf {
        self.dir.join("raw")
    }
    fn load_start(&self) -> i64 {
        self.meta_i64("load_start_ms").unwrap_or(0)
    }
    fn load_end(&self) -> i64 {
        self.meta_i64("load_end_ms").unwrap_or(i64::MAX)
    }
    fn drained(&self) -> i64 {
        self.meta_i64("drained_ms").unwrap_or_else(|| self.load_end())
    }
    fn steady_start(&self) -> i64 {
        self.load_start() + self.meta_i64("WARMUP").unwrap_or(60) * 1000
    }
}

fn parse_env(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

fn discover(campaign: &Path) -> Result<Vec<Run>> {
    let mut runs = Vec::new();
    for scenario in sorted_dirs(campaign)? {
        for label in sorted_dirs(&scenario)? {
            for rep_dir in sorted_dirs(&label)? {
                let meta_path = rep_dir.join("meta.env");
                if !meta_path.exists() {
                    continue;
                }
                let meta = parse_env(&fs::read_to_string(&meta_path)?);
                let rep = rep_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_prefix("rep-"))
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(1);
                runs.push(Run {
                    scenario: meta.get("scenario").cloned().unwrap_or_default(),
                    label: meta.get("label").cloned().unwrap_or_default(),
                    dir: rep_dir,
                    meta,
                    rep,
                });
            }
        }
    }
    Ok(runs)
}

fn sorted_dirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()) != Some("charts"))
        .collect();
    out.sort();
    Ok(out)
}

/// Reads a CSV file into header-indexed records.
struct Table {
    index: HashMap<String, usize>,
    rows: Vec<Vec<String>>,
}

impl Table {
    fn read(path: &Path, delimiter: u8) -> Result<Self> {
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(delimiter)
            .flexible(true)
            .from_path(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let index = reader
            .headers()?
            .iter()
            .enumerate()
            .map(|(i, h)| (h.to_string(), i))
            .collect();
        let mut rows = Vec::new();
        for record in reader.records() {
            rows.push(record?.iter().map(str::to_string).collect());
        }
        Ok(Self { index, rows })
    }
    fn read_opt(path: &Path, delimiter: u8) -> Option<Self> {
        path.exists().then(|| Self::read(path, delimiter).ok()).flatten()
    }
    fn str<'a>(&self, row: &'a [String], col: &str) -> &'a str {
        self.index.get(col).and_then(|i| row.get(*i)).map(String::as_str).unwrap_or("")
    }
    fn f64(&self, row: &[String], col: &str) -> Option<f64> {
        self.str(row, col).parse().ok()
    }
}

// ---------------------------------------------------------------- pgbench

struct LatencyLog {
    histogram: Histogram<u64>,
    /// (second since load start, latency in microseconds)
    points: Vec<(f64, u64)>,
}

/// Strips `<role>.` or `<role>-<replica>.` from a raw file name: a role may
/// run as several pgbench containers (producer-1, producer-2, ...).
fn strip_role<'a>(name: &'a str, role: &str) -> Option<&'a str> {
    let rest = name.strip_prefix(role)?;
    if let Some(rest) = rest.strip_prefix('.') {
        return Some(rest);
    }
    let rest = rest.strip_prefix('-')?;
    let (replica, rest) = rest.split_once('.')?;
    (!replica.is_empty() && replica.chars().all(|c| c.is_ascii_digit())).then_some(rest)
}

/// Parses pgbench per-transaction logs `<role>[-<replica>].<pid>[.<thread>]`.
fn read_pgbench(run: &Run, role: &str) -> Result<Option<LatencyLog>> {
    let mut histogram = Histogram::<u64>::new_with_bounds(1, 600_000_000, 3)?;
    let mut points = Vec::new();
    let mut found = false;
    for entry in fs::read_dir(run.raw())? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(rest) = strip_role(name, role) else { continue };
        if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit() || c == '.') {
            continue;
        }
        found = true;
        for line in fs::read_to_string(&path)?.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 6 {
                continue;
            }
            let (Ok(latency), Ok(sec), Ok(usec)) = (f[2].parse::<u64>(), f[4].parse::<i64>(), f[5].parse::<i64>())
            else {
                continue;
            };
            let done_ms = sec * 1000 + usec / 1000;
            points.push(((done_ms - run.load_start()) as f64 / 1000.0, latency));
            if done_ms >= run.steady_start() && done_ms <= run.load_end() {
                histogram.saturating_record(latency.max(1));
            }
        }
    }
    Ok(found.then_some(LatencyLog { histogram, points }))
}

/// Throughput and processed transactions from the pgbench output
/// (`<role>.out`, or the sum over `<role>-<replica>.out`).
/// Number of pgbench console outputs (`<role>[-<i>].out`) of a role.
fn pgbench_outputs(run: &Run, role: &str) -> usize {
    let Ok(entries) = fs::read_dir(run.raw()) else { return 0 };
    entries
        .flatten()
        .filter(|e| e.file_name().to_str().and_then(|n| strip_role(n, role)) == Some("out"))
        .count()
}

fn pgbench_summary(run: &Run, role: &str) -> (Option<f64>, Option<f64>) {
    let mut tps: Option<f64> = None;
    let mut processed: Option<f64> = None;
    let Ok(entries) = fs::read_dir(run.raw()) else { return (None, None) };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if strip_role(name, role) != Some("out") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap_or_default();
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("tps = ").and_then(|r| r.split_whitespace().next()) {
                if let Ok(v) = v.parse::<f64>() {
                    *tps.get_or_insert(0.0) += v;
                }
            }
            if let Some(rest) = line.strip_prefix("number of transactions actually processed: ") {
                if let Some(v) = rest.split(['/', ' ']).next().and_then(|v| v.parse::<f64>().ok()) {
                    *processed.get_or_insert(0.0) += v;
                }
            }
        }
    }
    (tps, processed)
}

fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

/// p99 latency (ms) per `bucket` seconds.
fn latency_series(points: &[(f64, u64)], bucket: f64, q: f64) -> Vec<(f64, f64)> {
    let mut buckets: BTreeMap<i64, Vec<u64>> = BTreeMap::new();
    for (t, lat) in points {
        if *t >= 0.0 {
            buckets.entry((t / bucket) as i64).or_default().push(*lat);
        }
    }
    buckets
        .into_iter()
        .map(|(b, mut v)| {
            v.sort_unstable();
            let idx = ((v.len() as f64 - 1.0) * q).round() as usize;
            ((b as f64 + 0.5) * bucket, ms(v[idx]))
        })
        .collect()
}

// ---------------------------------------------------------------- metrics

/// (key, title, decimals). The order is the order of the summary tables.
const METRICS: &[(&str, &str, usize)] = &[
    ("producer_tps", "Producer throughput (tx/s)", 0),
    ("producer_p50_ms", "Producer latency p50 (ms)", 2),
    ("producer_p95_ms", "Producer latency p95 (ms)", 2),
    ("producer_p99_ms", "Producer latency p99 (ms)", 2),
    ("producer_p999_ms", "Producer latency p99.9 (ms)", 2),
    ("producer_max_ms", "Producer latency max (ms)", 1),
    ("burst_tps", "Burst producer throughput (tx/s)", 0),
    ("reader_p50_ms", "Reader latency p50 (ms)", 2),
    ("reader_p95_ms", "Reader latency p95 (ms)", 2),
    ("reader_p99_ms", "Reader latency p99 (ms)", 2),
    ("lag_p50_ms", "End-to-end lag p50 (ms)", 0),
    ("lag_p95_ms", "End-to-end lag p95 (ms)", 0),
    ("lag_max_ms", "End-to-end lag max (ms)", 0),
    ("backlog_max", "Backlog max (events)", 0),
    ("drain_s", "Drain time after load (s)", 1),
    ("batches", "Batches / cycles", 0),
    ("rows_per_batch", "Rows per batch / cycle", 0),
    ("apply_ms_avg", "MERGE time per batch avg (ms)", 1),
    ("switch_wait_p50_ms", "Switch wait p50 (ms)", 1),
    ("switch_wait_p95_ms", "Switch wait p95 (ms)", 1),
    ("switch_wait_max_ms", "Switch wait max (ms)", 1),
    ("retries", "Consumer retries", 0),
    ("inbound_mb_end", "Inbound table(s) size at end (MB)", 1),
    ("inbound_mb_max", "Inbound table(s) size max (MB)", 1),
    ("inbound_dead_max", "Inbound dead tuples max", 0),
    ("orders_mb_growth", "orders size growth (MB)", 1),
    ("orders_dead_max", "orders dead tuples max", 0),
    ("autovacuum_runs", "Autovacuum runs (all tables)", 0),
    ("autovacuum_s", "Autovacuum time (s, all tables)", 1),
    ("wal_mb", "WAL generated (MB)", 0),
    ("wal_bytes_per_event", "WAL per event (bytes)", 0),
    ("deadlocks", "Deadlocks", 0),
    ("rollbacks", "Rollbacks", 0),
    ("multixact_blks", "MultiXact SLRU blocks zeroed", 0),
    ("cpu_postgres_pct", "CPU postgres (% of 1 core)", 0),
    ("cpu_consumer_pct", "CPU consumer (% of 1 core)", 0),
    ("cpu_load_pct", "CPU pgbench (% of 1 core)", 0),
];

struct RunResult {
    metrics: BTreeMap<&'static str, f64>,
    top_waits: String,
    producer_p99: Vec<(f64, f64)>,
    backlog: Vec<(f64, f64)>,
    lag_p95: Vec<(f64, f64)>,
    inbound_mb: Vec<(f64, f64)>,
    orders_dead: Vec<(f64, f64)>,
    inbound_dead: Vec<(f64, f64)>,
    wal_rate: Vec<(f64, f64)>,
    switch_wait: Vec<(f64, f64)>,
    /// WAL bytes per event for each entry of WAL_STEPS (empty without statements.csv)
    wal_steps: Vec<f64>,
    /// one entry per burst when the scenario runs several (BURST_COUNT > 1)
    bursts: Vec<BurstStats>,
    /// producer p99 (ms) in the quiet period before the first and before the last burst
    steady_p99: (Option<f64>, Option<f64>),
}

/// One burst of a multi-burst scenario.
struct BurstStats {
    tps: Option<f64>,
    p99_ms: Option<f64>,
    /// share of the burst inserts slower than LATE_MS (scheduled send → commit)
    late_pct: Option<f64>,
}

/// Client timeout used to count "late" inserts (see docs/insert-failures.md).
const LATE_MS: u64 = 2_000;

/// Statement groups of the WAL breakdown; the last one is the rest of the WAL.
const WAL_STEPS: [&str; 6] = ["inbound insert", "routing / switch", "claim", "MERGE", "cleanup", "other"];

fn wal_step(query: &str) -> Option<usize> {
    let q = query.trim_start();
    if q.starts_with("INSERT INTO inbox") || q.starts_with("INSERT INTO buffer_") {
        Some(0)
    } else if q.starts_with("SELECT active") || q.starts_with("UPDATE buffer_control") {
        Some(1)
    } else if q.contains("FROM inbox WHERE state") {
        Some(2)
    } else if q.starts_with("MERGE INTO orders") {
        Some(3)
    } else if q.starts_with("UPDATE inbox") || q.starts_with("DELETE FROM inbox") || q.starts_with("TRUNCATE") {
        Some(4)
    } else {
        None
    }
}

/// p-quantile (0..1) of latencies in microseconds, in ms.
fn latency_quantile(mut lat: Vec<u64>, q: f64) -> Option<f64> {
    if lat.is_empty() {
        return None;
    }
    lat.sort_unstable();
    Some(ms(lat[((lat.len() - 1) as f64 * q).round() as usize]))
}

fn rel_s(run: &Run, ts_ms: f64) -> f64 {
    (ts_ms - run.load_start() as f64) / 1000.0
}

fn weighted_quantile(mut values: Vec<(f64, f64)>, q: f64) -> Option<f64> {
    let total: f64 = values.iter().map(|(_, w)| w).sum();
    if values.is_empty() || total <= 0.0 {
        return None;
    }
    values.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut acc = 0.0;
    for (v, w) in &values {
        acc += w;
        if acc >= q * total {
            return Some(*v);
        }
    }
    values.last().map(|v| v.0)
}

fn quantile(mut values: Vec<f64>, q: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    Some(values[((values.len() as f64 - 1.0) * q).round() as usize])
}

const INBOUND: [&str; 3] = ["inbox", "buffer_0", "buffer_1"];

fn analyze(run: &Run) -> Result<RunResult> {
    let mut m: BTreeMap<&'static str, f64> = BTreeMap::new();
    let collector = run.raw().join("collector");

    // producers / readers
    let (tps, _) = pgbench_summary(run, "producer");
    if let Some(v) = tps {
        m.insert("producer_tps", v);
    }
    // Bursts run one after the other (BURST_COUNT > 1 → burst-<i>.out):
    // their throughput is the mean of the bursts, not the sum.
    if let (Some(v), _) = pgbench_summary(run, "burst") {
        m.insert("burst_tps", v / pgbench_outputs(run, "burst").max(1) as f64);
    }
    let burst_count = if run.meta.get("BURST").map(String::as_str) == Some("1") {
        run.meta_i64("BURST_COUNT").unwrap_or(1)
    } else {
        0
    };
    let mut producer_p99 = Vec::new();
    let mut steady_p99 = (None, None);
    if let Some(log) = read_pgbench(run, "producer")? {
        let h = &log.histogram;
        if !h.is_empty() {
            m.insert("producer_p50_ms", ms(h.value_at_quantile(0.50)));
            m.insert("producer_p95_ms", ms(h.value_at_quantile(0.95)));
            m.insert("producer_p99_ms", ms(h.value_at_quantile(0.99)));
            m.insert("producer_p999_ms", ms(h.value_at_quantile(0.999)));
            m.insert("producer_max_ms", ms(h.max()));
        }
        producer_p99 = latency_series(&log.points, 5.0, 0.99);
        if burst_count > 1 {
            // quiet periods: after warm-up until the first burst, and from 30 s
            // after the second-to-last burst ends until the last burst starts
            let at = run.meta_i64("BURST_AT").unwrap_or(120) as f64;
            let every = run.meta_i64("BURST_EVERY").unwrap_or(0) as f64;
            let duration = run.meta_i64("BURST_DURATION").unwrap_or(60) as f64;
            let warmup = run.meta_i64("WARMUP").unwrap_or(60) as f64;
            let last = at + (burst_count - 1) as f64 * every;
            let window = |a: f64, b: f64| {
                latency_quantile(log.points.iter().filter(|p| p.0 >= a && p.0 < b).map(|p| p.1).collect(), 0.99)
            };
            steady_p99 = (window(warmup, at), window(last - every + duration + 30.0, last));
        }
    }
    let mut bursts = Vec::new();
    if burst_count > 1 {
        for i in 1..=burst_count {
            let role = format!("burst-{i}");
            let mut b = BurstStats { tps: pgbench_summary(run, &role).0, p99_ms: None, late_pct: None };
            if let Some(log) = read_pgbench(run, &role)? {
                let lat: Vec<u64> = log.points.iter().map(|p| p.1).collect();
                if !lat.is_empty() {
                    let late = lat.iter().filter(|l| **l > LATE_MS * 1000).count();
                    b.late_pct = Some(100.0 * late as f64 / lat.len() as f64);
                    b.p99_ms = latency_quantile(lat, 0.99);
                }
            }
            bursts.push(b);
        }
    }
    if let Some(log) = read_pgbench(run, "reader")? {
        let h = &log.histogram;
        if !h.is_empty() {
            m.insert("reader_p50_ms", ms(h.value_at_quantile(0.50)));
            m.insert("reader_p95_ms", ms(h.value_at_quantile(0.95)));
            m.insert("reader_p99_ms", ms(h.value_at_quantile(0.99)));
        }
    }
    m.insert("drain_s", (run.drained() - run.load_end()) as f64 / 1000.0);

    // batch_log
    let mut lag_p95 = Vec::new();
    let mut switch_wait = Vec::new();
    if let Some(t) = Table::read_opt(&collector.join("batch_log.csv"), b',') {
        let after_warmup = |row: &[String]| t.f64(row, "finished_ms").unwrap_or(0.0) >= run.steady_start() as f64;
        let rows: Vec<&Vec<String>> = t.rows.iter().filter(|r| after_warmup(r)).collect();
        let weight = |r: &[String]| t.f64(r, "rows_read").unwrap_or(0.0);
        let col = |name: &str| -> Vec<(f64, f64)> {
            rows.iter().filter_map(|r| t.f64(r, name).map(|v| (v, weight(r)))).collect()
        };
        if let Some(v) = weighted_quantile(col("lag_p50_ms"), 0.5) {
            m.insert("lag_p50_ms", v);
        }
        if let Some(v) = weighted_quantile(col("lag_p95_ms"), 0.95) {
            m.insert("lag_p95_ms", v);
        }
        if let Some(v) = col("lag_max_ms").into_iter().map(|(v, _)| v).reduce(f64::max) {
            m.insert("lag_max_ms", v);
        }
        let batches = t.rows.len() as f64;
        let total_rows: f64 = t.rows.iter().map(|r| weight(r)).sum();
        m.insert("batches", batches);
        if batches > 0.0 {
            m.insert("rows_per_batch", total_rows / batches);
            let apply: f64 = t.rows.iter().filter_map(|r| t.f64(r, "apply_ms")).sum();
            m.insert("apply_ms_avg", apply / batches);
        }
        m.insert("retries", t.rows.iter().filter_map(|r| t.f64(r, "retries")).sum());
        let waits: Vec<f64> = t.rows.iter().filter_map(|r| t.f64(r, "switch_wait_ms")).collect();
        if !waits.is_empty() {
            m.insert("switch_wait_p50_ms", quantile(waits.clone(), 0.5).unwrap_or(0.0));
            m.insert("switch_wait_p95_ms", quantile(waits.clone(), 0.95).unwrap_or(0.0));
            m.insert("switch_wait_max_ms", waits.iter().copied().fold(0.0, f64::max));
        }
        for r in &t.rows {
            let x = rel_s(run, t.f64(r, "finished_ms").unwrap_or(0.0));
            if let Some(v) = t.f64(r, "lag_p95_ms") {
                lag_p95.push((x, v / 1000.0));
            }
            if let Some(v) = t.f64(r, "switch_wait_ms") {
                switch_wait.push((x, v));
            }
        }
    }

    // progress (backlog)
    let mut backlog = Vec::new();
    if let Some(t) = Table::read_opt(&collector.join("progress.csv"), b',') {
        for r in &t.rows {
            if let (Some(ts), Some(b)) = (t.f64(r, "ts_ms"), t.f64(r, "backlog")) {
                backlog.push((rel_s(run, ts), b));
            }
        }
        m.insert("backlog_max", backlog.iter().map(|p| p.1).fold(0.0, f64::max));
    }

    // tables: sizes, dead tuples, autovacuum
    let mut inbound_mb = Vec::new();
    let mut orders_dead = Vec::new();
    let mut inbound_dead = Vec::new();
    if let Some(t) = Table::read_opt(&collector.join("tables.csv"), b',') {
        let mut by_ts: BTreeMap<i64, Vec<&Vec<String>>> = BTreeMap::new();
        for r in &t.rows {
            by_ts.entry(t.f64(r, "ts_ms").unwrap_or(0.0) as i64).or_default().push(r);
        }
        let in_run = |ts: i64| ts >= run.load_start() && ts <= run.drained() + 5_000;
        let samples: Vec<(i64, Vec<&Vec<String>>)> = by_ts.into_iter().filter(|(ts, _)| in_run(*ts)).collect();
        let sum = |rows: &[&Vec<String>], tables: &[&str], col: &str| -> f64 {
            rows.iter()
                .filter(|r| tables.contains(&t.str(r, "table")))
                .filter_map(|r| t.f64(r, col))
                .sum()
        };
        let all: Vec<&str> = t.rows.iter().map(|r| t.str(r, "table")).collect();
        for (ts, rows) in &samples {
            let x = rel_s(run, *ts as f64);
            inbound_mb.push((x, sum(rows, &INBOUND, "total_bytes") / 1_048_576.0));
            orders_dead.push((x, sum(rows, &["orders"], "n_dead_tup")));
            inbound_dead.push((x, sum(rows, &INBOUND, "n_dead_tup")));
        }
        if let (Some((_, first)), Some((_, last))) = (samples.first(), samples.last()) {
            m.insert("inbound_mb_end", sum(last, &INBOUND, "total_bytes") / 1_048_576.0);
            m.insert(
                "orders_mb_growth",
                (sum(last, &["orders"], "total_bytes") - sum(first, &["orders"], "total_bytes")) / 1_048_576.0,
            );
            m.insert(
                "autovacuum_runs",
                sum(last, &all, "autovacuum_count") - sum(first, &all, "autovacuum_count"),
            );
            m.insert(
                "autovacuum_s",
                (sum(last, &all, "autovacuum_time_ms") - sum(first, &all, "autovacuum_time_ms")) / 1000.0,
            );
        }
        m.insert("inbound_mb_max", inbound_mb.iter().map(|p| p.1).fold(0.0, f64::max));
        m.insert("orders_dead_max", orders_dead.iter().map(|p| p.1).fold(0.0, f64::max));
        m.insert("inbound_dead_max", inbound_dead.iter().map(|p| p.1).fold(0.0, f64::max));
    }

    // database counters
    let mut wal_rate = Vec::new();
    let mut wal_total = None;
    if let Some(t) = Table::read_opt(&collector.join("database.csv"), b',') {
        let rows: Vec<&Vec<String>> = t
            .rows
            .iter()
            .filter(|r| {
                let ts = t.f64(r, "ts_ms").unwrap_or(0.0) as i64;
                ts >= run.load_start() && ts <= run.drained() + 5_000
            })
            .collect();
        if let (Some(first), Some(last)) = (rows.first(), rows.last()) {
            let delta = |col: &str| t.f64(last, col).unwrap_or(0.0) - t.f64(first, col).unwrap_or(0.0);
            let wal = delta("wal_bytes");
            wal_total = Some(wal);
            m.insert("wal_mb", wal / 1_048_576.0);
            let events = backlog_events(run);
            if events > 0.0 {
                m.insert("wal_bytes_per_event", wal / events);
            }
            m.insert("deadlocks", delta("deadlocks"));
            m.insert("rollbacks", delta("xact_rollback"));
            m.insert("multixact_blks", delta("multixact_blks_zeroed"));
        }
        for pair in rows.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let dt = (t.f64(b, "ts_ms").unwrap_or(0.0) - t.f64(a, "ts_ms").unwrap_or(0.0)) / 1000.0;
            if dt > 0.0 {
                let dw = t.f64(b, "wal_bytes").unwrap_or(0.0) - t.f64(a, "wal_bytes").unwrap_or(0.0);
                wal_rate.push((rel_s(run, t.f64(b, "ts_ms").unwrap_or(0.0)), dw / dt / 1_048_576.0));
            }
        }
    }

    // WAL per event by statement group (pg_stat_statements); "other" is the
    // rest of the WAL of the run: vacuum, checkpoints, bookkeeping
    let mut wal_steps = Vec::new();
    let events = backlog_events(run);
    if let (Some(t), Some(total)) = (Table::read_opt(&collector.join("statements.csv"), b','), wal_total) {
        if events > 0.0 {
            let mut bytes = [0.0; WAL_STEPS.len()];
            for r in &t.rows {
                if let Some(i) = wal_step(t.str(r, "query")) {
                    bytes[i] += t.f64(r, "wal_bytes").unwrap_or(0.0);
                }
            }
            let last = WAL_STEPS.len() - 1;
            bytes[last] = (total - bytes[..last].iter().sum::<f64>()).max(0.0);
            wal_steps = bytes.iter().map(|b| b / events).collect();
        }
    }

    // wait events (steady window)
    let mut top_waits = String::new();
    if let Some(t) = Table::read_opt(&collector.join("waits.csv"), b',') {
        let mut acc: BTreeMap<String, f64> = BTreeMap::new();
        let mut ticks = std::collections::BTreeSet::new();
        for r in &t.rows {
            let ts = t.f64(r, "ts_ms").unwrap_or(0.0) as i64;
            if ts >= run.steady_start() && ts <= run.load_end() {
                ticks.insert(ts);
                let key = format!("{}:{}", t.str(r, "wait_event_type"), t.str(r, "wait_event"));
                *acc.entry(key).or_default() += t.f64(r, "avg_sessions").unwrap_or(0.0);
            }
        }
        let n = ticks.len().max(1) as f64;
        let mut waits: Vec<(String, f64)> = acc.into_iter().map(|(k, v)| (k, v / n)).collect();
        waits.sort_by(|a, b| b.1.total_cmp(&a.1));
        top_waits = waits
            .iter()
            .take(5)
            .map(|(k, v)| format!("{k} {v:.2}"))
            .collect::<Vec<_>>()
            .join(", ");
    }

    // docker stats (steady window)
    if let Some(t) = Table::read_opt(&run.raw().join("docker_stats.csv"), b';') {
        let mut per_ts: BTreeMap<i64, [f64; 3]> = BTreeMap::new();
        for r in &t.rows {
            let ts = t.f64(r, "ts_ms").unwrap_or(0.0) as i64;
            if ts < run.steady_start() || ts > run.load_end() {
                continue;
            }
            let cpu: f64 = t.str(r, "cpu_perc").trim_end_matches('%').parse().unwrap_or(0.0);
            let name = t.str(r, "name");
            let slot = if name.contains("-postgres-") {
                0
            } else if name.contains("-consumer-") {
                1
            } else if name.contains("-run-") {
                2
            } else {
                continue;
            };
            per_ts.entry(ts).or_default()[slot] += cpu;
        }
        let n = per_ts.len() as f64;
        if n > 0.0 {
            for (i, key) in ["cpu_postgres_pct", "cpu_consumer_pct", "cpu_load_pct"].iter().enumerate() {
                m.insert(key, per_ts.values().map(|v| v[i]).sum::<f64>() / n);
            }
        }
    }

    Ok(RunResult {
        metrics: m,
        top_waits,
        producer_p99,
        backlog,
        lag_p95,
        inbound_mb,
        orders_dead,
        inbound_dead,
        wal_rate,
        switch_wait,
        wal_steps,
        bursts,
        steady_p99,
    })
}

/// Events ingested during the run (producer + burst), from pgbench summaries.
fn backlog_events(run: &Run) -> f64 {
    ["producer", "burst"]
        .iter()
        .filter_map(|role| pgbench_summary(run, role).1)
        .sum()
}

// ---------------------------------------------------------------- charts

const COLORS: [RGBColor; 6] = [
    RGBColor(31, 119, 180),
    RGBColor(255, 127, 14),
    RGBColor(44, 160, 44),
    RGBColor(214, 39, 40),
    RGBColor(148, 103, 189),
    RGBColor(140, 86, 75),
];

fn color_for(label: &str, labels: &[String]) -> RGBColor {
    let idx = labels.iter().position(|l| l == label).unwrap_or(0);
    COLORS[idx % COLORS.len()]
}

type Series = (String, RGBColor, Vec<(f64, f64)>);

/// What a chart shows and what it is for. Printed under the chart's title in
/// the image itself and under the image in summary.md, so a reader of either
/// knows how to read it without the code.
struct Caption<'a> {
    shows: &'a str,
    purpose: &'a str,
}

impl Caption<'_> {
    fn markdown(&self) -> String {
        format!("*Shows:* {} *Purpose:* {}", self.shows, self.purpose)
    }
}

/// Greedy word wrap at `width` characters.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Creates a `width` × (header + `plot_height`) SVG with the title and the
/// caption on top; returns the area left for the plot.
fn canvas<'a>(
    path: &'a Path,
    width: u32,
    plot_height: u32,
    title: &str,
    caption: &Caption,
    note: Option<&str>,
) -> Result<DrawingArea<SVGBackend<'a>, Shift>> {
    // ≈ 6 px per character of 13 px sans-serif
    let chars = ((width as f64 - 28.0) / 6.0) as usize;
    let mut lines = wrap(&format!("Shows: {}", caption.shows), chars);
    lines.extend(wrap(&format!("Purpose: {}", caption.purpose), chars));
    if let Some(note) = note {
        lines.extend(wrap(note, chars));
    }
    let header = 40 + 17 * lines.len() as u32;
    let root = SVGBackend::new(path, (width, header + plot_height)).into_drawing_area();
    root.fill(&WHITE)?;
    let (head, body) = root.split_vertically(header);
    head.draw_text(title, &("sans-serif", 18, FontStyle::Bold).into_font().color(&BLACK), (14, 10))?;
    let text = ("sans-serif", 13).into_font().color(&RGBColor(70, 70, 70));
    for (i, line) in lines.iter().enumerate() {
        head.draw_text(line, &text, (14, 36 + 17 * i as i32))?;
    }
    Ok(body)
}

fn line_chart(path: &Path, title: &str, caption: &Caption, y_desc: &str, series: &[Series]) -> Result<bool> {
    let points: Vec<&(f64, f64)> = series.iter().flat_map(|s| s.2.iter()).collect();
    if points.is_empty() {
        return Ok(false);
    }
    let x_max = points.iter().map(|p| p.0).fold(1.0, f64::max);
    let x_min = points.iter().map(|p| p.0).fold(0.0, f64::min);
    let y_max = points.iter().map(|p| p.1).fold(0.0, f64::max).max(1e-6) * 1.08;

    let root = canvas(path, 960, 420, title, caption, None)?;
    let mut chart = ChartBuilder::on(&root)
        .margin(14)
        .x_label_area_size(40)
        .y_label_area_size(70)
        .build_cartesian_2d(x_min..x_max, 0f64..y_max)?;
    chart
        .configure_mesh()
        .x_desc("seconds since load start")
        .y_desc(y_desc)
        .x_label_formatter(&|v| format!("{v:.0}"))
        .y_label_formatter(&|v| fmt_value(*v))
        .light_line_style(RGBColor(235, 235, 235))
        .draw()?;
    for (name, color, pts) in series {
        let c = *color;
        chart
            .draw_series(LineSeries::new(pts.iter().copied(), c.stroke_width(2)))?
            .label(name.as_str())
            .legend(move |(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], c.stroke_width(3)));
    }
    chart
        .configure_series_labels()
        .position(SeriesLabelPosition::UpperLeft)
        .background_style(WHITE.mix(0.85))
        .border_style(BLACK.mix(0.4))
        .draw()?;
    root.present()?;
    Ok(true)
}

/// Name of the category centred on `x` (x axis of the bar charts: category i at x = i).
fn category(names: &[String], x: f64) -> String {
    if x < -1e-9 || (x - x.round()).abs() > 1e-9 {
        return String::new();
    }
    names.get(x.round() as usize).cloned().unwrap_or_default()
}

/// (label, color, mean, stddev)
fn bar_chart(
    path: &Path,
    title: &str,
    caption: &Caption,
    y_desc: &str,
    bars: &[(String, RGBColor, f64, f64)],
) -> Result<bool> {
    if bars.is_empty() {
        return Ok(false);
    }
    let n = bars.len();
    let y_max = bars.iter().map(|b| b.2 + b.3).fold(0.0, f64::max).max(1e-6) * 1.18;
    let note = bars
        .iter()
        .any(|b| b.3 > 0.0)
        .then_some("Bar: mean of the repetitions; whisker: ± 1 standard deviation.");
    let root = canvas(path, (180 + 120 * n as u32).max(720), 380, title, caption, note)?;
    let labels: Vec<String> = bars.iter().map(|b| b.0.clone()).collect();
    let mut chart = ChartBuilder::on(&root)
        .margin(14)
        .x_label_area_size(36)
        .y_label_area_size(70)
        .build_cartesian_2d(-0.5..n as f64 - 0.5, 0f64..y_max)?;
    chart
        .configure_mesh()
        .disable_x_mesh()
        .light_line_style(RGBColor(235, 235, 235))
        .x_labels(n + 1)
        .y_desc(y_desc)
        .y_label_formatter(&|v| fmt_value(*v))
        .x_label_formatter(&|v| category(&labels, *v))
        .draw()?;
    chart.draw_series(
        bars.iter()
            .enumerate()
            .map(|(i, b)| Rectangle::new([(i as f64 - 0.3, 0.0), (i as f64 + 0.3, b.2)], b.1.filled())),
    )?;
    chart.draw_series(bars.iter().enumerate().filter(|(_, b)| b.3 > 0.0).map(|(i, b)| {
        PathElement::new(vec![(i as f64, (b.2 - b.3).max(0.0)), (i as f64, b.2 + b.3)], BLACK.stroke_width(2))
    }))?;
    chart.draw_series(bars.iter().enumerate().map(|(i, b)| {
        Text::new(
            fmt_value(b.2),
            (i as f64, b.2 + b.3 + y_max * 0.01),
            ("sans-serif", 13)
                .into_font()
                .color(&BLACK)
                .pos(Pos::new(HPos::Center, VPos::Bottom)),
        )
    }))?;
    root.present()?;
    Ok(true)
}

/// (label, color, one value per group; None = no bar)
type GroupSeries = (String, RGBColor, Vec<Option<f64>>);

/// One group of bars per entry of `groups`, one bar per series in each group.
/// `log_y` plots log10 of the values (ticks show the real values);
/// `reference` draws a horizontal line with a legend entry (e.g. a target).
#[allow(clippy::too_many_arguments)]
fn grouped_bar_chart(
    path: &Path,
    title: &str,
    caption: &Caption,
    y_desc: &str,
    groups: &[String],
    series: &[GroupSeries],
    log_y: bool,
    reference: Option<(f64, &str)>,
) -> Result<bool> {
    let values: Vec<f64> = series.iter().flat_map(|s| s.2.iter().flatten().copied()).collect();
    if groups.is_empty() || series.is_empty() || values.is_empty() {
        return Ok(false);
    }
    let (n, k) = (groups.len(), series.len());
    let positive: Vec<f64> = values.iter().copied().filter(|v| *v > 0.0).collect();
    let log_y = log_y && !positive.is_empty();
    let floor = if log_y { positive.iter().copied().fold(f64::INFINITY, f64::min).log10().floor() } else { 0.0 };
    let ty = |v: f64| if log_y { v.max(10f64.powf(floor)).log10() } else { v };
    let top = values.iter().copied().chain(reference.map(|r| r.0)).fold(0.0, f64::max);
    // head room for the legend in the upper right corner
    let y_max = if log_y { top.max(1e-9).log10() + 1.0 } else { top.max(1e-6) * 1.45 };

    let width = (200 + (n * (k * 34 + 30)) as u32).clamp(720, 1200);
    let root = canvas(path, width, 420, title, caption, None)?;
    // group i is centred on x = i; its bars share 84 % of the unit
    let mut chart = ChartBuilder::on(&root)
        .margin(14)
        .x_label_area_size(36)
        .y_label_area_size(70)
        .build_cartesian_2d(-0.5..n as f64 - 0.5, floor..y_max)?;
    chart
        .configure_mesh()
        .disable_x_mesh()
        .light_line_style(RGBColor(235, 235, 235))
        .x_labels(n + 1)
        .y_desc(y_desc)
        .x_label_formatter(&|v| category(groups, *v))
        .y_label_formatter(&|v| {
            if !log_y {
                fmt_value(*v)
            } else if (v - v.round()).abs() < 1e-9 {
                fmt_value(10f64.powf(v.round()))
            } else {
                String::new()
            }
        })
        .draw()?;
    let bar = 0.84 / k as f64;
    for (j, (name, color, vals)) in series.iter().enumerate() {
        let c = *color;
        let x0 = |i: usize| i as f64 - 0.42 + j as f64 * bar;
        let present = || vals.iter().enumerate().filter_map(|(i, v)| v.map(|v| (i, v)));
        chart
            .draw_series(present().map(|(i, v)| {
                Rectangle::new([(x0(i) + bar * 0.06, floor), (x0(i) + bar * 0.94, ty(v))], c.filled())
            }))?
            .label(name.as_str())
            .legend(move |(x, y)| Rectangle::new([(x, y - 5), (x + 14, y + 5)], c.filled()));
        chart.draw_series(present().map(|(i, v)| {
            EmptyElement::at((x0(i) + bar / 2.0, ty(v)))
                + Text::new(
                    fmt_value(v),
                    (0, -3),
                    ("sans-serif", 10).into_font().color(&BLACK).pos(Pos::new(HPos::Center, VPos::Bottom)),
                )
        }))?;
    }
    if let Some((r, name)) = reference {
        let style = BLACK.mix(0.6).stroke_width(2);
        chart
            .draw_series(LineSeries::new([(-0.5, ty(r)), (n as f64 - 0.5, ty(r))], style))?
            .label(name)
            .legend(move |(x, y)| PathElement::new(vec![(x, y), (x + 14, y)], style));
    }
    chart
        .configure_series_labels()
        .position(SeriesLabelPosition::UpperRight)
        .background_style(WHITE.mix(0.85))
        .border_style(BLACK.mix(0.4))
        .draw()?;
    root.present()?;
    Ok(true)
}

/// Markdown of a chart: the image and its caption.
fn chart_link(scenario: &str, title: &str, file: &str, caption: &Caption) -> String {
    format!("![{scenario} {title}](charts/{file})\n\n{}", caption.markdown())
}

fn fmt_value(v: f64) -> String {
    if v.abs() >= 100.0 {
        format!("{v:.0}")
    } else if v.abs() >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

// ---------------------------------------------------------------- summary

fn mean_sd(values: &[f64]) -> (f64, f64) {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let var = if values.len() > 1 {
        values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0)
    } else {
        0.0
    };
    (mean, var.sqrt())
}

fn fmt_cell(values: &[f64], decimals: usize) -> String {
    if values.is_empty() {
        return "–".to_string();
    }
    let (mean, sd) = mean_sd(values);
    if values.len() > 1 {
        format!("{mean:.decimals$} ± {sd:.decimals$}")
    } else {
        format!("{mean:.decimals$}")
    }
}

fn main() -> Result<()> {
    let campaign = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "/results".into()));
    let runs = discover(&campaign)?;
    if runs.is_empty() {
        bail!("no runs found under {}", campaign.display());
    }
    let charts_dir = campaign.join("charts");
    fs::create_dir_all(&charts_dir)?;

    let mut results: Vec<(Run, RunResult)> = Vec::new();
    for run in runs {
        let result = analyze(&run).with_context(|| format!("analyzing {}", run.dir.display()))?;
        eprintln!("analyzed {}/{}/rep-{}", run.scenario, run.label, run.rep);
        results.push((run, result));
    }

    // runs.csv
    let mut w = csv::Writer::from_path(campaign.join("runs.csv"))?;
    let mut header = vec!["scenario", "label", "rep"];
    header.extend(METRICS.iter().map(|m| m.0));
    header.push("top_waits");
    w.write_record(&header)?;
    for (run, res) in &results {
        let mut rec = vec![run.scenario.clone(), run.label.clone(), run.rep.to_string()];
        for (key, _, _) in METRICS {
            rec.push(res.metrics.get(key).map(|v| format!("{v:.3}")).unwrap_or_default());
        }
        rec.push(res.top_waits.clone());
        w.write_record(&rec)?;
    }
    w.flush()?;

    // group by scenario → label
    let mut scenarios: BTreeMap<String, BTreeMap<String, Vec<usize>>> = BTreeMap::new();
    for (i, (run, _)) in results.iter().enumerate() {
        scenarios
            .entry(run.scenario.clone())
            .or_default()
            .entry(run.label.clone())
            .or_default()
            .push(i);
    }

    let campaign_name = campaign.file_name().and_then(|n| n.to_str()).unwrap_or("campaign");
    let mut md = String::new();
    md.push_str(&format!("# Benchmark summary — campaign `{campaign_name}`\n\n"));
    md.push_str("Generated by `common/tools/src/bin/report.rs` from the raw data of each run.\n");
    md.push_str("Cells: mean ± sample standard deviation across repetitions.\n");
    md.push_str("Latencies use the steady window (after WARMUP until the end of the load); ");
    md.push_str("counters (WAL, autovacuum, deadlocks) cover the load plus the final drain.\n\n");

    for (scenario, labels) in &scenarios {
        let label_names: Vec<String> = labels.keys().cloned().collect();
        let first_run = &results[labels.values().next().expect("label")[0]].0;
        let description = first_run.meta.get("DESCRIPTION").cloned().unwrap_or_default();
        md.push_str(&format!("## {scenario}\n\n"));
        if !description.is_empty() {
            md.push_str(&format!("{description}\n\n"));
        }
        let reps: Vec<String> = label_names
            .iter()
            .map(|l| format!("{l}: {} rep.", labels[l].len()))
            .collect();
        md.push_str(&format!("Runs — {}\n\n", reps.join(", ")));

        md.push_str(&format!("| Metric | {} |\n", label_names.join(" | ")));
        md.push_str(&format!("|---|{}\n", "---:|".repeat(label_names.len())));
        for (key, title, decimals) in METRICS {
            let cells: Vec<Vec<f64>> = label_names
                .iter()
                .map(|l| labels[l].iter().filter_map(|i| results[*i].1.metrics.get(key).copied()).collect())
                .collect();
            if cells.iter().all(|c| c.is_empty()) {
                continue;
            }
            let row: Vec<String> = cells.iter().map(|c| fmt_cell(c, *decimals)).collect();
            md.push_str(&format!("| {title} | {} |\n", row.join(" | ")));
        }
        md.push_str("\nTop wait events (avg active sessions, repetition 1):\n\n");
        for l in &label_names {
            let res = &results[labels[l][0]].1;
            md.push_str(&format!("- **{l}**: {}\n", res.top_waits));
        }
        md.push('\n');

        // mean of a value over the repetitions of a label
        let mean_of = |l: &String, f: &dyn Fn(&RunResult) -> Option<f64>| -> Option<f64> {
            let v: Vec<f64> = labels[l].iter().filter_map(|i| f(&results[*i].1)).collect();
            (!v.is_empty()).then(|| mean_sd(&v).0)
        };
        let n_bursts = labels.values().flatten().map(|i| results[*i].1.bursts.len()).max().unwrap_or(0);
        if n_bursts > 1 {
            md.push_str(&format!(
                "Bursts (mean of the repetitions; late = slower than {} s from the scheduled send to the commit, \
                 i.e. failed by a client with that timeout):\n\n",
                LATE_MS / 1000
            ));
            md.push_str(&format!("| Metric | {} |\n", label_names.join(" | ")));
            md.push_str(&format!("|---|{}\n", "---:|".repeat(label_names.len())));
            let mut row = |title: String, f: &dyn Fn(&RunResult) -> Option<f64>, decimals: usize| {
                let cells: Vec<String> = label_names
                    .iter()
                    .map(|l| mean_of(l, f).map(|v| format!("{v:.decimals$}")).unwrap_or_else(|| "–".into()))
                    .collect();
                md.push_str(&format!("| {title} | {} |\n", cells.join(" | ")));
            };
            row("Steady producer p99 before the first burst (ms)".into(), &|r| r.steady_p99.0, 1);
            row("Steady producer p99 before the last burst (ms)".into(), &|r| r.steady_p99.1, 1);
            for b in 0..n_bursts {
                row(format!("Burst {}: throughput (tx/s)", b + 1), &|r| r.bursts.get(b).and_then(|s| s.tps), 0);
                row(format!("Burst {}: p99 (ms)", b + 1), &|r| r.bursts.get(b).and_then(|s| s.p99_ms), 0);
                row(format!("Burst {}: late inserts (%)", b + 1), &|r| r.bursts.get(b).and_then(|s| s.late_pct), 2);
            }
            md.push('\n');
        }

        // time series charts (repetition 1 of each label)
        type Pick = fn(&RunResult) -> &Vec<(f64, f64)>;
        let charts: [(&str, &str, &str, Caption, Pick); 8] = [
            (
                "producer-p99",
                "Producer latency p99 per 5 s",
                "ms",
                Caption {
                    shows: "p99 latency of the base producer's inserts in 5 s windows (repetition 1), from the \
                            scheduled send time to the commit: time queued in the client is included.",
                    purpose: "See whether insert latency stays flat or degrades over the run, and how bursts and \
                              B's switches show up. Values above a client's timeout would be failed inserts.",
                },
                |r| &r.producer_p99,
            ),
            (
                "backlog",
                "Backlog (ingested − consumed)",
                "events",
                Caption {
                    shows: "Events committed by the producers minus events applied to orders, sampled every second \
                            (repetition 1).",
                    purpose: "Check that the consumer keeps up: A stays near zero, B saws up and down once per \
                              cycle. A line that keeps rising means the consumer is falling behind.",
                },
                |r| &r.backlog,
            ),
            (
                "lag",
                "End-to-end lag p95 per batch / cycle",
                "seconds",
                Caption {
                    shows: "p95 age of the events (commit → applied to orders) of each batch (A) or cycle (B), at \
                            the time the batch finished (repetition 1).",
                    purpose: "Freshness of orders over time. B's floor is one cycle; peaks in A are bursts the \
                              workers could not absorb at once.",
                },
                |r| &r.lag_p95,
            ),
            (
                "inbound-size",
                "Inbound table(s) total size",
                "MB",
                Caption {
                    shows: "Total size (heap + indexes + TOAST) of the inbound tables: inbox in A, buffer_0 + \
                            buffer_1 in B (repetition 1).",
                    purpose: "Growth and bloat on the ingestion side: A's inbox keeps growing in update mode, B \
                              returns to zero at every TRUNCATE.",
                },
                |r| &r.inbound_mb,
            ),
            (
                "inbound-dead",
                "Inbound table(s) dead tuples",
                "tuples",
                Caption {
                    shows: "Dead tuples in the inbound tables (repetition 1).",
                    purpose: "Vacuum debt created by A's UPDATE / DELETE cleanup; B's buffers never hold dead \
                              tuples.",
                },
                |r| &r.inbound_dead,
            ),
            (
                "orders-dead",
                "orders dead tuples",
                "tuples",
                Caption {
                    shows: "Dead tuples in the business table orders (repetition 1).",
                    purpose: "Vacuum debt left by the MERGE updates; fewer writes per event (B coalesces the \
                              events of an order within a cycle) leave less debt.",
                },
                |r| &r.orders_dead,
            ),
            (
                "wal-rate",
                "WAL generation rate",
                "MB/s",
                Caption {
                    shows: "WAL written per second (repetition 1).",
                    purpose: "Write amplification over time; peaks are checkpoints (full-page writes) and, in B, \
                              the large MERGE of each cycle.",
                },
                |r| &r.wal_rate,
            ),
            (
                "switch-wait",
                "Switch wait per cycle (project B)",
                "ms",
                Caption {
                    shows: "Time the consumer's switch (UPDATE of the controller rows) waited for in-flight \
                            inserts, per cycle (project B only, repetition 1).",
                    purpose: "Cost of the pessimistic lock: producers arriving during a switch wait about as long \
                              (risk of failed inserts with a short lock or statement timeout).",
                },
                |r| &r.switch_wait,
            ),
        ];
        let mut links = Vec::new();
        for (slug, title, y_desc, caption, pick) in charts {
            let series: Vec<Series> = label_names
                .iter()
                .map(|l| {
                    let res = &results[labels[l][0]].1;
                    (l.clone(), color_for(l, &label_names), pick(res).clone())
                })
                .filter(|s| !s.2.is_empty())
                .collect();
            let file = format!("{scenario}-{slug}.svg");
            if line_chart(&charts_dir.join(&file), &format!("{scenario}: {title}"), &caption, y_desc, &series)? {
                links.push(chart_link(scenario, title, &file, &caption));
            }
        }

        // bar charts of key metrics
        let bar_metrics = [
            (
                "producer_tps",
                "Producer throughput",
                "tx/s",
                Caption {
                    shows: "Mean committed inserts per second of the base producer over the whole load.",
                    purpose: "Was the offered rate delivered? Below the configured rate means saturation.",
                },
            ),
            (
                "producer_p99_ms",
                "Producer latency p99",
                "ms",
                Caption {
                    shows: "p99 latency of the base producer's inserts in the steady window (after warm-up), \
                            client queueing included.",
                    purpose: "Tail latency seen by producers: the number to compare with a client's timeout.",
                },
            ),
            (
                "reader_p99_ms",
                "Reader latency p99",
                "ms",
                Caption {
                    shows: "p99 latency of the business reads on orders (by id and by status), steady window.",
                    purpose: "Impact of ingestion and consumption on the readers of the business table.",
                },
            ),
            (
                "lag_p95_ms",
                "End-to-end lag p95",
                "ms",
                Caption {
                    shows: "p95 age of the events when applied to orders (steady window, weighted by rows).",
                    purpose: "Freshness: the price of B's cycle against A's continuous workers.",
                },
            ),
            (
                "wal_bytes_per_event",
                "WAL per event",
                "bytes",
                Caption {
                    shows: "WAL bytes per ingested event, from the load start to the end of the drain.",
                    purpose: "Write amplification: I/O, replication and archiving cost of each design.",
                },
            ),
            (
                "cpu_postgres_pct",
                "CPU postgres",
                "% of 1 core",
                Caption {
                    shows: "Mean CPU of the PostgreSQL container in the steady window (100 = one core).",
                    purpose: "Database cost of the same work; lower means more headroom for peaks.",
                },
            ),
        ];
        for (key, title, y_desc, caption) in bar_metrics {
            let bars: Vec<(String, RGBColor, f64, f64)> = label_names
                .iter()
                .filter_map(|l| {
                    let values: Vec<f64> =
                        labels[l].iter().filter_map(|i| results[*i].1.metrics.get(key).copied()).collect();
                    (!values.is_empty()).then(|| {
                        let (mean, sd) = mean_sd(&values);
                        (l.clone(), color_for(l, &label_names), mean, sd)
                    })
                })
                .collect();
            let file = format!("{scenario}-bar-{}.svg", key.replace('_', "-"));
            if bar_chart(&charts_dir.join(&file), &format!("{scenario}: {title}"), &caption, y_desc, &bars)? {
                links.push(chart_link(scenario, title, &file, &caption));
            }
        }

        // WAL per event by statement group (mean of the repetitions)
        let wal_series: Vec<GroupSeries> = label_names
            .iter()
            .filter_map(|l| {
                let runs: Vec<&RunResult> =
                    labels[l].iter().map(|i| &results[*i].1).filter(|r| !r.wal_steps.is_empty()).collect();
                (!runs.is_empty()).then(|| {
                    let values = (0..WAL_STEPS.len())
                        .map(|s| Some(runs.iter().map(|r| r.wal_steps[s]).sum::<f64>() / runs.len() as f64))
                        .collect();
                    (l.clone(), color_for(l, &label_names), values)
                })
            })
            .collect();
        let steps: Vec<String> = WAL_STEPS.iter().map(|s| s.to_string()).collect();
        let caption = Caption {
            shows: "WAL bytes per event split by statement (pg_stat_statements, mean of the repetitions): inbound \
                    INSERT, routing / switch (B), claim (A), MERGE into orders, cleanup (UPDATE / DELETE / \
                    TRUNCATE) and other = total WAL − those statements (vacuum, checkpoints, bookkeeping).",
            purpose: "Where each design's write amplification comes from (docs/comparison.md).",
        };
        let (file, title) = (format!("{scenario}-wal-steps.svg"), "WAL per event by statement");
        let path = charts_dir.join(&file);
        let title_full = format!("{scenario}: {title}");
        if grouped_bar_chart(&path, &title_full, &caption, "bytes / event", &steps, &wal_series, false, None)? {
            links.push(chart_link(scenario, title, &file, &caption));
        }

        // short term vs long term: one group per burst (multi-burst scenarios)
        if n_bursts > 1 {
            let groups: Vec<String> = (1..=n_bursts).map(|i| format!("burst {i}")).collect();
            let per_burst = |f: &dyn Fn(&BurstStats) -> Option<f64>| -> Vec<GroupSeries> {
                label_names
                    .iter()
                    .map(|l| {
                        let values =
                            (0..n_bursts).map(|b| mean_of(l, &|r| r.bursts.get(b).and_then(|s| f(s)))).collect();
                        (l.clone(), color_for(l, &label_names), values)
                    })
                    .collect()
            };
            let rate: Option<f64> = first_run.meta.get("BURST_RATE").and_then(|v| v.parse().ok());
            let rate_name = rate.map(|r| format!("configured rate {r:.0} tx/s")).unwrap_or_default();
            let reference = rate.map(|r| (r, rate_name.as_str()));
            let late = format!(
                "Share of the burst inserts slower than {} s in each burst, from the scheduled send time to the \
                 commit (mean of the repetitions).",
                LATE_MS / 1000
            );
            let late_purpose = format!(
                "Inserts that a client with a {} s timeout would have failed: the zero-failure requirement \
                 (docs/insert-failures.md), short term versus long term.",
                LATE_MS / 1000
            );
            let burst_charts: [(&str, &str, &str, Caption, Vec<GroupSeries>, bool, Option<(f64, &str)>); 3] = [
                (
                    "bursts-tps",
                    "Burst throughput, short vs long term",
                    "tx/s",
                    Caption {
                        shows: "Throughput of the burst producer group in each burst (mean of the repetitions); \
                                the line is the configured burst rate.",
                        purpose: "Short term (first burst) versus long term (last burst): does the design keep \
                                  absorbing peaks? Below the line, inserts queue in the client.",
                    },
                    per_burst(&|s| s.tps),
                    false,
                    reference,
                ),
                (
                    "bursts-p99",
                    "Burst insert latency p99, short vs long term",
                    "ms (log scale)",
                    Caption {
                        shows: "p99 latency of the burst inserts in each burst, from the scheduled send time to \
                                the commit (mean of the repetitions, logarithmic axis).",
                        purpose: "Tail latency at the peaks, short term versus long term.",
                    },
                    per_burst(&|s| s.p99_ms),
                    true,
                    None,
                ),
                (
                    "bursts-late",
                    "Late burst inserts (client-timeout risk)",
                    "% of burst inserts",
                    Caption { shows: &late, purpose: &late_purpose },
                    per_burst(&|s| s.late_pct),
                    false,
                    None,
                ),
            ];
            for (slug, title, y_desc, caption, series, log_y, reference) in burst_charts {
                let file = format!("{scenario}-{slug}.svg");
                let path = charts_dir.join(&file);
                let title_full = format!("{scenario}: {title}");
                if grouped_bar_chart(&path, &title_full, &caption, y_desc, &groups, &series, log_y, reference)? {
                    links.push(chart_link(scenario, title, &file, &caption));
                }
            }
            let steady: Vec<GroupSeries> = label_names
                .iter()
                .map(|l| {
                    let values = vec![mean_of(l, &|r| r.steady_p99.0), mean_of(l, &|r| r.steady_p99.1)];
                    (l.clone(), color_for(l, &label_names), values)
                })
                .collect();
            let groups = vec!["before the first burst".to_string(), "before the last burst".to_string()];
            let caption = Caption {
                shows: "p99 latency of the base producer in the quiet period before the first burst and in the \
                        quiet period before the last burst (mean of the repetitions, logarithmic axis).",
                purpose: "Degradation over time, outside the bursts: equal bars mean the design does not wear out \
                          during the run.",
            };
            let (file, title) = (format!("{scenario}-steady-p99.svg"), "Steady insert latency p99, start vs end");
            let path = charts_dir.join(&file);
            let title_full = format!("{scenario}: {title}");
            if grouped_bar_chart(&path, &title_full, &caption, "ms (log scale)", &groups, &steady, true, None)? {
                links.push(chart_link(scenario, title, &file, &caption));
            }
        }

        md.push_str("Charts (each one states under its title what it shows and what it is for):\n\n");
        for link in links {
            md.push_str(&format!("{link}\n\n"));
        }
    }

    fs::write(campaign.join("summary.md"), md)?;
    println!("wrote {}/summary.md, runs.csv and charts/", campaign.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::strip_role;

    #[test]
    fn role_files_with_and_without_replicas() {
        assert_eq!(strip_role("producer.123", "producer"), Some("123"));
        assert_eq!(strip_role("producer.123.4", "producer"), Some("123.4"));
        assert_eq!(strip_role("producer-2.77.1", "producer"), Some("77.1"));
        assert_eq!(strip_role("producer-12.out", "producer"), Some("out"));
        assert_eq!(strip_role("producer.out", "producer"), Some("out"));
        assert_eq!(strip_role("producer-x.out", "producer"), None);
        assert_eq!(strip_role("burst.out", "producer"), None);
        assert_eq!(strip_role("producerx.1", "producer"), None);
    }
}
