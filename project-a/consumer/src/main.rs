//! Project A consumer: N competing workers over a single inbox table.
//!
//! Each worker loops over one transaction:
//!   1. claim a batch:  SELECT ... WHERE state = 'P' ORDER BY id
//!                      FETCH FIRST n ROWS ONLY FOR UPDATE SKIP LOCKED
//!   2. apply it:       MERGE INTO orders (shared with project B)
//!   3. clean it up:    UPDATE inbox SET state = 'F' ... (or DELETE)
//!   4. log it:         INSERT INTO batch_log
//!   5. COMMIT
//!
//! Environment:
//!   DATABASE_URL, WORKERS (4), BATCH_SIZE (500), CLEANUP_MODE (update|delete),
//!   EMPTY_SLEEP_MS (50), AUDIT (0|1)

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::NaiveDateTime;
use tlb_core::apply::{audit_sql, merge_sql, placeholders};
use tlb_core::batch_log::{self, BatchRecord};
use tlb_core::shutdown::Shutdown;
use tlb_core::{config, db, lag};
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Statement};
use uuid::Uuid;

const CLAIM_SQL: &str = "SELECT id, order_id, received_at
  FROM inbox
 WHERE state = 'P'
 ORDER BY id
 FETCH FIRST $1 ROWS ONLY
 FOR UPDATE SKIP LOCKED";

#[derive(Clone, Copy, Debug, PartialEq)]
enum CleanupMode {
    Update,
    Delete,
}

#[derive(Clone)]
struct Config {
    url: String,
    workers: usize,
    batch_size: usize,
    cleanup: CleanupMode,
    empty_sleep: Duration,
    audit: bool,
}

impl Config {
    fn from_env() -> Result<Self> {
        let cleanup = match config::env_string("CLEANUP_MODE", "update").as_str() {
            "update" => CleanupMode::Update,
            "delete" => CleanupMode::Delete,
            other => bail!("CLEANUP_MODE must be 'update' or 'delete', got {other:?}"),
        };
        Ok(Self {
            url: config::database_url(),
            workers: config::env_or("WORKERS", 4usize).max(1),
            batch_size: config::env_or("BATCH_SIZE", 500usize).max(1),
            cleanup,
            empty_sleep: Duration::from_millis(config::env_or("EMPTY_SLEEP_MS", 50u64)),
            audit: config::env_flag("AUDIT"),
        })
    }
}

/// Builds (merge, cleanup, audit) statements for a batch of `n` claimed ids.
fn batch_sql(cfg: &Config, n: usize) -> (String, String, Option<String>) {
    let ids = placeholders(1, n);
    let filter = format!("b.id IN ({ids})");
    let cleanup = match cfg.cleanup {
        CleanupMode::Update => format!(
            "UPDATE inbox SET state = 'F', processed_at = CURRENT_TIMESTAMP WHERE id IN ({ids})"
        ),
        CleanupMode::Delete => format!("DELETE FROM inbox WHERE id IN ({ids})"),
    };
    let audit = cfg.audit.then(|| audit_sql("inbox", &filter));
    (merge_sql("inbox", &filter), cleanup, audit)
}

/// Statements that depend on the number of claimed ids (IN list size).
struct BatchStatements {
    merge: Statement,
    cleanup: Statement,
    audit: Option<Statement>,
}

struct Worker {
    id: i16,
    cfg: Config,
    client: Client,
    claim: Statement,
    /// Prepared statements for full batches; partial batches use one-off statements.
    full: Option<BatchStatements>,
}

enum Outcome {
    Empty,
    Processed,
}

impl Worker {
    async fn prepare_batch(&self, n: usize) -> Result<BatchStatements> {
        let (merge, cleanup, audit) = batch_sql(&self.cfg, n);
        Ok(BatchStatements {
            merge: self.client.prepare(&merge).await?,
            cleanup: self.client.prepare(&cleanup).await?,
            audit: match audit {
                Some(sql) => Some(self.client.prepare(&sql).await?),
                None => None,
            },
        })
    }

    async fn run_batch(&mut self, retries: i32) -> Result<Outcome> {
        let started_at = lag::now_utc();
        let partial_sql;
        let tx = self.client.transaction().await?;

        let rows = tx.query(&self.claim, &[&(self.cfg.batch_size as i64)]).await?;
        if rows.is_empty() {
            tx.commit().await?;
            return Ok(Outcome::Empty);
        }

        let n = rows.len();
        let mut ids: Vec<Uuid> = Vec::with_capacity(n);
        let mut orders: HashSet<i64> = HashSet::with_capacity(n);
        let mut received: Vec<NaiveDateTime> = Vec::with_capacity(n);
        for row in &rows {
            ids.push(row.get(0));
            orders.insert(row.get(1));
            received.push(row.get(2));
        }
        let params: Vec<&(dyn ToSql + Sync)> = ids.iter().map(|id| id as &(dyn ToSql + Sync)).collect();

        let t_apply = Instant::now();
        let applied;
        let t_cleanup;
        if n == self.cfg.batch_size {
            let stmts = self.full.as_ref().expect("full batch statements are prepared");
            if let Some(audit) = &stmts.audit {
                tx.execute(audit, &params).await?;
            }
            applied = tx.execute(&stmts.merge, &params).await?;
            t_cleanup = Instant::now();
            tx.execute(&stmts.cleanup, &params).await?;
        } else {
            partial_sql = batch_sql(&self.cfg, n);
            let (merge, cleanup, audit) = &partial_sql;
            if let Some(audit) = audit {
                tx.execute(audit.as_str(), &params).await?;
            }
            applied = tx.execute(merge.as_str(), &params).await?;
            t_cleanup = Instant::now();
            tx.execute(cleanup.as_str(), &params).await?;
        }
        let apply_ms = (t_cleanup - t_apply).as_secs_f64() * 1000.0;
        let cleanup_ms = t_cleanup.elapsed().as_secs_f64() * 1000.0;

        let finished_at = lag::now_utc();
        let record = BatchRecord {
            project: "A",
            worker: self.id,
            source_table: "inbox".to_string(),
            generation: None,
            started_at,
            finished_at,
            rows_read: n as i32,
            rows_distinct: orders.len() as i32,
            rows_applied: applied as i32,
            lag: lag::from_samples(finished_at, received),
            apply_ms,
            cleanup_ms: Some(cleanup_ms),
            switch_wait_ms: None,
            wait_for_data_ms: None,
            retries,
        };
        batch_log::insert(&tx, &record).await?;
        tx.commit().await?;
        Ok(Outcome::Processed)
    }

    async fn run(mut self, mut shutdown: Shutdown) -> Result<()> {
        let mut processed_batches = 0u64;
        while !shutdown.is_requested() {
            let mut retries = 0;
            let outcome = loop {
                match self.run_batch(retries).await {
                    Ok(outcome) => break outcome,
                    Err(err) if db::is_retryable(&err) && retries < 50 => {
                        retries += 1;
                        tokio::time::sleep(Duration::from_millis(5 * retries as u64)).await;
                    }
                    Err(err) => return Err(err).context(format!("worker {}", self.id)),
                }
            };
            match outcome {
                Outcome::Processed => processed_batches += 1,
                Outcome::Empty => {
                    shutdown.sleep(self.cfg.empty_sleep).await;
                }
            }
        }
        println!("worker {} stopped after {processed_batches} batches", self.id);
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::from_env()?;
    println!(
        "consumer-a: workers={} batch_size={} cleanup={:?} audit={}",
        cfg.workers, cfg.batch_size, cfg.cleanup, cfg.audit
    );
    let shutdown = Shutdown::install();

    let mut handles = Vec::with_capacity(cfg.workers);
    for id in 0..cfg.workers {
        let client = db::connect(&cfg.url, &format!("consumer-a-{id}")).await?;
        let claim = client.prepare(CLAIM_SQL).await?;
        let mut worker = Worker {
            id: id as i16,
            cfg: cfg.clone(),
            client,
            claim,
            full: None,
        };
        worker.full = Some(worker.prepare_batch(cfg.batch_size).await?);
        handles.push(tokio::spawn(worker.run(shutdown.clone())));
    }

    let mut failures = HashMap::new();
    for (id, handle) in handles.into_iter().enumerate() {
        if let Err(err) = handle.await? {
            eprintln!("{err:#}");
            failures.insert(id, err);
        }
    }
    if !failures.is_empty() {
        bail!("{} worker(s) failed", failures.len());
    }
    Ok(())
}
