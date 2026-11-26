//! Samples PostgreSQL statistics views into CSV files while a run is active.
//!
//! This is instrumentation, so it is the one place where PostgreSQL specific
//! catalog views (pg_stat_*) are used. It never reads the buffer tables
//! themselves: their size comes from the catalog, and the backlog is derived
//! from insert counters minus the rows reported in batch_log.
//!
//! Output (RESULTS_DIR/raw/collector/):
//!   tables.csv      per table counters and sizes, every INTERVAL_MS
//!   database.csv    database, WAL, checkpointer and MultiXact counters
//!   waits.csv       wait events of active client backends (sampled 10x/tick)
//!   progress.csv    ingested vs consumed events (backlog)
//!   batch_log.csv   export of batch_log at shutdown
//!   statements.csv  top statements from pg_stat_statements at shutdown

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::NaiveDateTime;
use csv::Writer;
use tlb_core::config;
use tlb_core::db;
use tlb_core::shutdown::Shutdown;
use tokio_postgres::Client;

const TABLES_SQL: &str = "
SELECT s.relname::text,
       s.n_tup_ins, s.n_tup_upd, s.n_tup_hot_upd, s.n_tup_del,
       s.n_live_tup, s.n_dead_tup,
       s.autovacuum_count, s.autoanalyze_count,
       COALESCE(s.total_autovacuum_time, 0)::float8,
       COALESCE(s.seq_scan, 0), COALESCE(s.idx_scan, 0),
       pg_relation_size(s.relid), pg_indexes_size(s.relid), pg_total_relation_size(s.relid)
  FROM pg_stat_user_tables s
 ORDER BY s.relname";

const DATABASE_SQL: &str = "
SELECT d.xact_commit, d.xact_rollback, d.blks_read, d.blks_hit,
       d.tup_inserted, d.tup_updated, d.tup_deleted, d.temp_bytes, d.deadlocks,
       w.wal_records, w.wal_fpi, w.wal_bytes::bigint,
       c.num_timed, c.num_requested, c.buffers_written,
       (SELECT COALESCE(SUM(blks_zeroed), 0)::bigint FROM pg_stat_slru WHERE lower(name) LIKE 'multixact%')
  FROM pg_stat_database d, pg_stat_wal w, pg_stat_checkpointer c
 WHERE d.datname = current_database()";

const WAITS_SQL: &str = "
SELECT COALESCE(wait_event_type, 'CPU'), COALESCE(wait_event, 'running'), COUNT(*)
  FROM pg_stat_activity
 WHERE state = 'active' AND backend_type = 'client backend' AND pid <> pg_backend_pid()
 GROUP BY 1, 2";

const PROGRESS_SQL: &str = "
SELECT (SELECT COALESCE(SUM(n_tup_ins), 0)::bigint FROM pg_stat_user_tables
         WHERE relname IN ('inbox', 'buffer_0', 'buffer_1')),
       (SELECT COALESCE(SUM(rows_read), 0)::bigint FROM batch_log),
       (SELECT COALESCE(SUM(rows_applied), 0)::bigint FROM batch_log),
       (SELECT COUNT(*) FROM batch_log)";

const BATCH_LOG_SQL: &str = "
SELECT id, project, worker, source_table, generation, started_at, finished_at,
       rows_read, rows_distinct, rows_applied, lag_p50_ms, lag_p95_ms, lag_max_ms,
       apply_ms, cleanup_ms, switch_wait_ms, wait_for_data_ms, retries
  FROM batch_log ORDER BY id";

const STATEMENTS_SQL: &str = "
SELECT s.calls, s.total_exec_time, s.mean_exec_time, s.rows,
       s.shared_blks_hit, s.shared_blks_read, s.wal_bytes::bigint,
       left(regexp_replace(s.query, '\\s+', ' ', 'g'), 400)
  FROM pg_stat_statements s
 WHERE s.dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
 ORDER BY s.total_exec_time DESC
 LIMIT 50";

fn epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn ts_ms(ts: NaiveDateTime) -> i64 {
    ts.and_utc().timestamp_millis()
}

fn writer(dir: &Path, name: &str, header: &[&str]) -> Result<Writer<File>> {
    let path = dir.join(name);
    let mut w = Writer::from_path(&path).with_context(|| format!("creating {}", path.display()))?;
    w.write_record(header)?;
    Ok(w)
}

struct Collector {
    client: Client,
    tables: Writer<File>,
    database: Writer<File>,
    waits: Writer<File>,
    progress: Writer<File>,
}

impl Collector {
    async fn sample_tables(&mut self, ts: i64) -> Result<()> {
        for row in self.client.query(TABLES_SQL, &[]).await? {
            let name: String = row.get(0);
            let mut rec = vec![ts.to_string(), name];
            for i in 1..=8 {
                rec.push(row.get::<_, i64>(i).to_string());
            }
            rec.push(format!("{:.1}", row.get::<_, f64>(9)));
            for i in 10..=14 {
                rec.push(row.get::<_, i64>(i).to_string());
            }
            self.tables.write_record(&rec)?;
        }
        self.tables.flush()?;
        Ok(())
    }

    async fn sample_database(&mut self, ts: i64) -> Result<()> {
        let row = self.client.query_one(DATABASE_SQL, &[]).await?;
        let mut rec = vec![ts.to_string()];
        for i in 0..16 {
            rec.push(row.get::<_, i64>(i).to_string());
        }
        self.database.write_record(&rec)?;
        self.database.flush()?;
        Ok(())
    }

    async fn sample_waits(&mut self, acc: &mut BTreeMap<(String, String), i64>) -> Result<()> {
        for row in self.client.query(WAITS_SQL, &[]).await? {
            let key = (row.get::<_, String>(0), row.get::<_, String>(1));
            *acc.entry(key).or_default() += row.get::<_, i64>(2);
        }
        Ok(())
    }

    fn write_waits(&mut self, ts: i64, samples: u32, acc: &BTreeMap<(String, String), i64>) -> Result<()> {
        for ((kind, event), count) in acc {
            self.waits.write_record([
                ts.to_string(),
                kind.clone(),
                event.clone(),
                format!("{:.3}", *count as f64 / samples.max(1) as f64),
            ])?;
        }
        self.waits.flush()?;
        Ok(())
    }

    async fn sample_progress(&mut self, ts: i64) -> Result<()> {
        let row = self.client.query_one(PROGRESS_SQL, &[]).await?;
        let ingested: i64 = row.get(0);
        let consumed: i64 = row.get(1);
        let applied: i64 = row.get(2);
        let batches: i64 = row.get(3);
        self.progress.write_record([
            ts.to_string(),
            ingested.to_string(),
            consumed.to_string(),
            applied.to_string(),
            (ingested - consumed).max(0).to_string(),
            batches.to_string(),
        ])?;
        self.progress.flush()?;
        Ok(())
    }

    async fn export(&mut self, dir: &Path) -> Result<()> {
        let mut w = writer(
            dir,
            "batch_log.csv",
            &[
                "id", "project", "worker", "source_table", "generation", "started_ms", "finished_ms",
                "rows_read", "rows_distinct", "rows_applied", "lag_p50_ms", "lag_p95_ms", "lag_max_ms",
                "apply_ms", "cleanup_ms", "switch_wait_ms", "wait_for_data_ms", "retries",
            ],
        )?;
        let opt = |v: Option<f64>| v.map(|x| format!("{x:.3}")).unwrap_or_default();
        for row in self.client.query(BATCH_LOG_SQL, &[]).await? {
            w.write_record([
                row.get::<_, i64>(0).to_string(),
                row.get::<_, String>(1),
                row.get::<_, i16>(2).to_string(),
                row.get::<_, String>(3),
                row.get::<_, Option<i64>>(4).map(|g| g.to_string()).unwrap_or_default(),
                ts_ms(row.get(5)).to_string(),
                ts_ms(row.get(6)).to_string(),
                row.get::<_, i32>(7).to_string(),
                row.get::<_, i32>(8).to_string(),
                row.get::<_, i32>(9).to_string(),
                format!("{:.3}", row.get::<_, f64>(10)),
                format!("{:.3}", row.get::<_, f64>(11)),
                format!("{:.3}", row.get::<_, f64>(12)),
                format!("{:.3}", row.get::<_, f64>(13)),
                opt(row.get(14)),
                opt(row.get(15)),
                opt(row.get(16)),
                row.get::<_, i32>(17).to_string(),
            ])?;
        }
        w.flush()?;

        let mut w = writer(
            dir,
            "statements.csv",
            &[
                "calls", "total_exec_ms", "mean_exec_ms", "rows", "shared_blks_hit",
                "shared_blks_read", "wal_bytes", "query",
            ],
        )?;
        for row in self.client.query(STATEMENTS_SQL, &[]).await? {
            w.write_record([
                row.get::<_, i64>(0).to_string(),
                format!("{:.3}", row.get::<_, f64>(1)),
                format!("{:.3}", row.get::<_, f64>(2)),
                row.get::<_, i64>(3).to_string(),
                row.get::<_, i64>(4).to_string(),
                row.get::<_, i64>(5).to_string(),
                row.get::<_, i64>(6).to_string(),
                row.get::<_, String>(7),
            ])?;
        }
        w.flush()?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let interval = Duration::from_millis(config::env_or("INTERVAL_MS", 1000u64));
    let wait_samples: u32 = config::env_or("WAIT_SAMPLES", 10u32).max(1);
    let dir = PathBuf::from(config::env_string("RESULTS_DIR", "/results")).join("raw/collector");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let client = db::connect(&config::database_url(), "collector").await?;
    // Never queue behind a TRUNCATE for long: size functions take a brief
    // AccessShareLock on each relation.
    client.batch_execute("SET lock_timeout = '1s'").await?;

    let mut c = Collector {
        client,
        tables: writer(
            &dir,
            "tables.csv",
            &[
                "ts_ms", "table", "n_tup_ins", "n_tup_upd", "n_tup_hot_upd", "n_tup_del", "n_live_tup",
                "n_dead_tup", "autovacuum_count", "autoanalyze_count", "autovacuum_time_ms", "seq_scan",
                "idx_scan", "heap_bytes", "index_bytes", "total_bytes",
            ],
        )?,
        database: writer(
            &dir,
            "database.csv",
            &[
                "ts_ms", "xact_commit", "xact_rollback", "blks_read", "blks_hit", "tup_inserted",
                "tup_updated", "tup_deleted", "temp_bytes", "deadlocks", "wal_records", "wal_fpi",
                "wal_bytes", "checkpoints_timed", "checkpoints_requested", "checkpoint_buffers_written",
                "multixact_blks_zeroed",
            ],
        )?,
        waits: writer(&dir, "waits.csv", &["ts_ms", "wait_event_type", "wait_event", "avg_sessions"])?,
        progress: writer(
            &dir,
            "progress.csv",
            &["ts_ms", "ingested", "consumed", "applied", "backlog", "batches"],
        )?,
    };

    let mut shutdown = Shutdown::install();
    let wait_step = interval / wait_samples;
    println!("collector started: interval={interval:?}, output={}", dir.display());

    loop {
        let tick = Instant::now();
        let ts = epoch_ms();
        let mut waits = BTreeMap::new();
        let mut taken = 0;
        let mut stop = false;
        for _ in 0..wait_samples {
            if let Err(err) = c.sample_waits(&mut waits).await {
                eprintln!("wait sample failed: {err:#}");
            }
            taken += 1;
            if shutdown.sleep(wait_step).await {
                stop = true;
                break;
            }
        }
        for result in [
            c.sample_tables(ts).await,
            c.sample_database(ts).await,
            c.sample_progress(ts).await,
            c.write_waits(ts, taken, &waits),
        ] {
            if let Err(err) = result {
                eprintln!("sample failed: {err:#}");
            }
        }
        if stop || shutdown.is_requested() {
            break;
        }
        if let Some(rest) = interval.checked_sub(tick.elapsed()) {
            if shutdown.sleep(rest).await {
                break;
            }
        }
    }

    c.export(&dir).await?;
    println!("collector exported batch_log and statements to {}", dir.display());
    Ok(())
}
