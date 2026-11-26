//! Builds the benchmark report of a campaign.
//!
//!   report <results/CAMPAIGN>
//!
//! Reads every run under <campaign>/<scenario>/<label>/rep-<n>/ (meta.env +
//! raw/ written by bench/run.sh) and writes:
//!   <campaign>/runs.csv     one row per run, every metric
//!   <campaign>/summary.md   per scenario: mean ± stddev per label
//!   <campaign>/charts/*.svg time series (repetition 1 of each label) and bars
//!
//! Windows: latency and CPU use the steady window [load start + WARMUP,
//! load end]; counters (WAL, autovacuum, deadlocks) use [load start, drained].

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use hdrhistogram::Histogram;
use plotters::prelude::*;
use plotters::style::text_anchor::{HPos, Pos, VPos};

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

/// Parses pgbench per-transaction logs `<role>.<pid>[.<thread>]`.
fn read_pgbench(run: &Run, role: &str) -> Result<Option<LatencyLog>> {
    let mut histogram = Histogram::<u64>::new_with_bounds(1, 120_000_000, 3)?;
    let mut points = Vec::new();
    let mut found = false;
    let prefix = format!("{role}.");
    for entry in fs::read_dir(run.raw())? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(rest) = name.strip_prefix(&prefix) else { continue };
        if !rest.chars().all(|c| c.is_ascii_digit() || c == '.') {
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

fn pgbench_summary(run: &Run, role: &str) -> (Option<f64>, Option<f64>) {
    let text = fs::read_to_string(run.raw().join(format!("{role}.out"))).unwrap_or_default();
    let mut tps = None;
    let mut processed = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("tps = ") {
            tps = rest.split_whitespace().next().and_then(|v| v.parse().ok());
        }
        if let Some(rest) = line.strip_prefix("number of transactions actually processed: ") {
            processed = rest.split(['/', ' ']).next().and_then(|v| v.parse().ok());
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
    if let (Some(v), _) = pgbench_summary(run, "burst") {
        m.insert("burst_tps", v);
    }
    let mut producer_p99 = Vec::new();
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

fn line_chart(path: &Path, title: &str, y_desc: &str, series: &[Series], log_note: &str) -> Result<bool> {
    let points: Vec<&(f64, f64)> = series.iter().flat_map(|s| s.2.iter()).collect();
    if points.is_empty() {
        return Ok(false);
    }
    let x_max = points.iter().map(|p| p.0).fold(1.0, f64::max);
    let x_min = points.iter().map(|p| p.0).fold(0.0, f64::min);
    let y_max = points.iter().map(|p| p.1).fold(0.0, f64::max).max(1e-6) * 1.08;

    let root = SVGBackend::new(path, (960, 420)).into_drawing_area();
    root.fill(&WHITE)?;
    let caption = if log_note.is_empty() { title.to_string() } else { format!("{title} — {log_note}") };
    let mut chart = ChartBuilder::on(&root)
        .caption(caption, ("sans-serif", 18))
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

/// (label, color, mean, stddev)
fn bar_chart(path: &Path, title: &str, y_desc: &str, bars: &[(String, RGBColor, f64, f64)]) -> Result<bool> {
    if bars.is_empty() {
        return Ok(false);
    }
    let n = bars.len();
    let y_max = bars.iter().map(|b| b.2 + b.3).fold(0.0, f64::max).max(1e-6) * 1.18;
    let root = SVGBackend::new(path, (180 + 120 * n as u32, 380)).into_drawing_area();
    root.fill(&WHITE)?;
    let labels: Vec<String> = bars.iter().map(|b| b.0.clone()).collect();
    let mut chart = ChartBuilder::on(&root)
        .caption(title, ("sans-serif", 16))
        .margin(14)
        .x_label_area_size(36)
        .y_label_area_size(70)
        .build_cartesian_2d((0usize..n).into_segmented(), 0f64..y_max)?;
    chart
        .configure_mesh()
        .disable_x_mesh()
        .light_line_style(RGBColor(235, 235, 235))
        .y_desc(y_desc)
        .y_label_formatter(&|v| fmt_value(*v))
        .x_label_formatter(&|v| match v {
            SegmentValue::CenterOf(i) | SegmentValue::Exact(i) => labels.get(*i).cloned().unwrap_or_default(),
            SegmentValue::Last => String::new(),
        })
        .draw()?;
    chart.draw_series(bars.iter().enumerate().map(|(i, b)| {
        let mut rect = Rectangle::new(
            [(SegmentValue::Exact(i), 0.0), (SegmentValue::Exact(i + 1), b.2)],
            b.1.filled(),
        );
        rect.set_margin(0, 0, 18, 18);
        rect
    }))?;
    chart.draw_series(bars.iter().enumerate().filter(|(_, b)| b.3 > 0.0).map(|(i, b)| {
        PathElement::new(
            vec![(SegmentValue::CenterOf(i), (b.2 - b.3).max(0.0)), (SegmentValue::CenterOf(i), b.2 + b.3)],
            BLACK.stroke_width(2),
        )
    }))?;
    chart.draw_series(bars.iter().enumerate().map(|(i, b)| {
        Text::new(
            fmt_value(b.2),
            (SegmentValue::CenterOf(i), b.2 + b.3 + y_max * 0.01),
            ("sans-serif", 13)
                .into_font()
                .color(&BLACK)
                .pos(Pos::new(HPos::Center, VPos::Bottom)),
        )
    }))?;
    root.present()?;
    Ok(true)
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

        // time series charts (repetition 1 of each label)
        type Pick = fn(&RunResult) -> &Vec<(f64, f64)>;
        let charts: [(&str, &str, &str, Pick); 8] = [
            ("producer-p99", "Producer latency p99 per 5 s", "ms", |r| &r.producer_p99),
            ("backlog", "Backlog (ingested − consumed)", "events", |r| &r.backlog),
            ("lag", "End-to-end lag p95 per batch / cycle", "seconds", |r| &r.lag_p95),
            ("inbound-size", "Inbound table(s) total size", "MB", |r| &r.inbound_mb),
            ("inbound-dead", "Inbound table(s) dead tuples", "tuples", |r| &r.inbound_dead),
            ("orders-dead", "orders dead tuples", "tuples", |r| &r.orders_dead),
            ("wal-rate", "WAL generation rate", "MB/s", |r| &r.wal_rate),
            ("switch-wait", "Switch wait per cycle (project B)", "ms", |r| &r.switch_wait),
        ];
        let mut links = Vec::new();
        for (slug, title, y_desc, pick) in charts {
            let series: Vec<Series> = label_names
                .iter()
                .map(|l| {
                    let res = &results[labels[l][0]].1;
                    (l.clone(), color_for(l, &label_names), pick(res).clone())
                })
                .filter(|s| !s.2.is_empty())
                .collect();
            let file = format!("{scenario}-{slug}.svg");
            if line_chart(&charts_dir.join(&file), &format!("{scenario}: {title}"), y_desc, &series, "")? {
                links.push(format!("![{scenario} {title}](charts/{file})"));
            }
        }

        // bar charts of key metrics
        let bar_metrics = [
            ("producer_tps", "Producer throughput", "tx/s"),
            ("producer_p99_ms", "Producer latency p99", "ms"),
            ("reader_p99_ms", "Reader latency p99", "ms"),
            ("lag_p95_ms", "End-to-end lag p95", "ms"),
            ("wal_bytes_per_event", "WAL per event", "bytes"),
            ("cpu_postgres_pct", "CPU postgres", "% of 1 core"),
        ];
        for (key, title, y_desc) in bar_metrics {
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
            if bar_chart(&charts_dir.join(&file), &format!("{scenario}: {title}"), y_desc, &bars)? {
                links.push(format!("![{scenario} {title}](charts/{file})"));
            }
        }
        md.push_str("Charts:\n\n");
        for link in links {
            md.push_str(&format!("{link}\n\n"));
        }
    }

    fs::write(campaign.join("summary.md"), md)?;
    println!("wrote {}/summary.md, runs.csv and charts/", campaign.display());
    Ok(())
}
