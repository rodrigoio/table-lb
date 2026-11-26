//! Project B consumer: table load balancing over two buffer tables.
//!
//! The consumer is the only process that switches buffers, because it is the
//! only one that knows when the cold buffer has been consumed completely.
//! Cycle:
//!
//!   1. wait for data in the HOT buffer (poll every POLL_MS)
//!   2. switch: UPDATE buffer_control SET active = <cold> ...
//!      Producers hold FOR SHARE on their controller slot while inserting, so
//!      this UPDATE waits for every in-flight insert. Once it commits, nobody
//!      can write into the buffer that just turned cold.
//!   3. drain the new COLD buffer completely in one transaction:
//!      stats + MERGE INTO orders (shared with project A), then COMMIT
//!   4. TRUNCATE the cold buffer (kept outside the transaction so the design
//!      also holds on engines where TRUNCATE is not transactional; a crash
//!      before it only causes an idempotent re-apply on restart)
//!   5. log the cycle and start over
//!
//! Environment: DATABASE_URL, POLL_MS (100), AUDIT (0|1)

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::NaiveDateTime;
use tlb_core::apply::{audit_sql, merge_sql};
use tlb_core::batch_log::{self, BatchRecord};
use tlb_core::shutdown::Shutdown;
use tlb_core::{config, db, lag};
use tokio_postgres::{Client, Statement};

const BUFFERS: [&str; 2] = ["buffer_0", "buffer_1"];

const READ_CONTROL_SQL: &str = "SELECT active, generation FROM buffer_control WHERE slot = 0";

/// Updates every slot row: the statement has to wait for the FOR SHARE lock
/// of each producer currently inserting through that slot.
const SWITCH_SQL: &str = "UPDATE buffer_control
   SET active = $1, generation = generation + 1, switched_at = CURRENT_TIMESTAMP";

fn stats_sql(table: &str) -> String {
    format!(
        "SELECT COUNT(*), COUNT(DISTINCT order_id), MIN(received_at),
       PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY received_at),
       PERCENTILE_DISC(0.05) WITHIN GROUP (ORDER BY received_at)
  FROM {table}"
    )
}

fn has_data_sql(table: &str) -> String {
    format!("SELECT 1 FROM {table} FETCH FIRST 1 ROWS ONLY")
}

struct Consumer {
    client: Client,
    poll: Duration,
    stats: Vec<Statement>,
    merge: Vec<Statement>,
    audit: Vec<Option<Statement>>,
    has_data: Vec<Statement>,
    switch: Statement,
    active: usize,
    generation: i64,
    // progress counters for the periodic log line
    cycles: u64,
    rows: u64,
    switch_ms_total: f64,
    switch_ms_max: f64,
    last_report: Instant,
}

impl Consumer {
    async fn new(client: Client, poll: Duration, audit: bool) -> Result<Self> {
        let mut stats = Vec::new();
        let mut merge = Vec::new();
        let mut audits = Vec::new();
        let mut has_data = Vec::new();
        for table in BUFFERS {
            stats.push(client.prepare(&stats_sql(table)).await?);
            merge.push(client.prepare(&merge_sql(table, "")).await?);
            audits.push(if audit {
                Some(client.prepare(&audit_sql(table, "")).await?)
            } else {
                None
            });
            has_data.push(client.prepare(&has_data_sql(table)).await?);
        }
        let switch = client.prepare(SWITCH_SQL).await?;
        let row = client.query_one(READ_CONTROL_SQL, &[]).await?;
        let active = row.get::<_, i16>(0) as usize;
        let generation: i64 = row.get(1);
        Ok(Self {
            client,
            poll,
            stats,
            merge,
            audit: audits,
            has_data,
            switch,
            active,
            generation,
            cycles: 0,
            rows: 0,
            switch_ms_total: 0.0,
            switch_ms_max: 0.0,
            last_report: Instant::now(),
        })
    }

    fn cold(&self) -> usize {
        1 - self.active
    }

    /// Polls the hot buffer until it has at least one committed row.
    /// Returns None when shutdown was requested while the buffer was empty.
    async fn wait_for_data(&mut self, shutdown: &mut Shutdown) -> Result<Option<f64>> {
        let started = Instant::now();
        loop {
            let found = self.client.query_opt(&self.has_data[self.active], &[]).await?;
            if found.is_some() {
                return Ok(Some(started.elapsed().as_secs_f64() * 1000.0));
            }
            if shutdown.is_requested() || shutdown.sleep(self.poll).await {
                // one last look: producers may have stopped right before us
                if self.client.query_opt(&self.has_data[self.active], &[]).await?.is_none() {
                    return Ok(None);
                }
            }
        }
    }

    /// Makes the cold buffer hot and returns how long the UPDATE waited.
    async fn switch(&mut self) -> Result<f64> {
        let started = Instant::now();
        let new_active = self.cold() as i16;
        self.client.execute(&self.switch, &[&new_active]).await?;
        self.active = new_active as usize;
        self.generation += 1;
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        self.switch_ms_total += ms;
        self.switch_ms_max = self.switch_ms_max.max(ms);
        Ok(ms)
    }

    /// Consumes the cold buffer completely. Returns the number of rows read.
    async fn drain(&mut self, wait_for_data_ms: Option<f64>, switch_wait_ms: Option<f64>) -> Result<u64> {
        let cold = self.cold();
        let table = BUFFERS[cold];
        let started_at = lag::now_utc();

        let tx = self.client.transaction().await?;
        let row = tx.query_one(&self.stats[cold], &[]).await?;
        let rows_read: i64 = row.get(0);
        if rows_read == 0 {
            tx.commit().await?;
            return Ok(0);
        }
        let rows_distinct: i64 = row.get(1);
        let oldest: NaiveDateTime = row.get(2);
        let p50: NaiveDateTime = row.get(3);
        let p05: NaiveDateTime = row.get(4);

        if let Some(audit) = &self.audit[cold] {
            tx.execute(audit, &[]).await?;
        }
        let t_apply = Instant::now();
        let applied = tx.execute(&self.merge[cold], &[]).await?;
        let apply_ms = t_apply.elapsed().as_secs_f64() * 1000.0;
        let finished_at = lag::now_utc();
        tx.commit().await?;

        let t_cleanup = Instant::now();
        self.client
            .batch_execute(&format!("TRUNCATE TABLE {table}"))
            .await
            .with_context(|| format!("truncating {table}"))?;
        let cleanup_ms = t_cleanup.elapsed().as_secs_f64() * 1000.0;

        let record = BatchRecord {
            project: "B",
            worker: 0,
            source_table: table.to_string(),
            generation: Some(self.generation),
            started_at,
            finished_at,
            rows_read: rows_read as i32,
            rows_distinct: rows_distinct as i32,
            rows_applied: applied as i32,
            lag: lag::from_percentiles(finished_at, oldest, p50, p05),
            apply_ms,
            cleanup_ms: Some(cleanup_ms),
            switch_wait_ms,
            wait_for_data_ms,
            retries: 0,
        };
        batch_log::insert(&self.client, &record).await?;

        self.cycles += 1;
        self.rows += rows_read as u64;
        self.report(false);
        Ok(rows_read as u64)
    }

    fn report(&mut self, force: bool) {
        if force || self.last_report.elapsed() >= Duration::from_secs(10) {
            let avg = if self.cycles > 0 { self.switch_ms_total / self.cycles as f64 } else { 0.0 };
            println!(
                "generation={} cycles={} rows={} switch_wait_ms(avg={avg:.1}, max={:.1})",
                self.generation, self.cycles, self.rows, self.switch_ms_max
            );
            self.last_report = Instant::now();
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let poll = Duration::from_millis(config::env_or("POLL_MS", 100u64));
    let audit = config::env_flag("AUDIT");
    let client = db::connect(&config::database_url(), "consumer-b").await?;
    let mut consumer = Consumer::new(client, poll, audit).await?;
    let mut shutdown = Shutdown::install();
    println!(
        "consumer-b: poll={poll:?} audit={audit} active=buffer_{} generation={}",
        consumer.active, consumer.generation
    );

    // Leftovers from a previous run (e.g. a crash before TRUNCATE) are
    // drained first; re-applying them is idempotent.
    let leftovers = consumer.drain(None, None).await?;
    if leftovers > 0 {
        println!("drained {leftovers} leftover rows from buffer_{}", consumer.cold());
    }

    // After a shutdown request, at most one more cycle flushes the hot buffer.
    let mut flush_cycles = 1;
    loop {
        let Some(wait_ms) = consumer.wait_for_data(&mut shutdown).await? else {
            break;
        };
        let switch_ms = consumer.switch().await?;
        consumer.drain(Some(wait_ms), Some(switch_ms)).await?;
        if shutdown.is_requested() {
            if flush_cycles == 0 {
                break;
            }
            flush_cycles -= 1;
        }
    }
    consumer.report(true);
    println!("consumer-b stopped");
    Ok(())
}
