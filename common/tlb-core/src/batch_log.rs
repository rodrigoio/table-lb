//! Writer for the `batch_log` table (one row per batch / drain cycle).

use anyhow::Result;
use chrono::NaiveDateTime;
use tokio_postgres::GenericClient;

use crate::lag::LagStats;

#[derive(Debug, Clone)]
pub struct BatchRecord {
    pub project: &'static str,
    pub worker: i16,
    pub source_table: String,
    pub generation: Option<i64>,
    pub started_at: NaiveDateTime,
    pub finished_at: NaiveDateTime,
    pub rows_read: i32,
    pub rows_distinct: i32,
    pub rows_applied: i32,
    pub lag: LagStats,
    pub apply_ms: f64,
    pub cleanup_ms: Option<f64>,
    pub switch_wait_ms: Option<f64>,
    pub wait_for_data_ms: Option<f64>,
    pub retries: i32,
}

pub const INSERT_SQL: &str = "INSERT INTO batch_log (
    project, worker, source_table, generation, started_at, finished_at,
    rows_read, rows_distinct, rows_applied, lag_p50_ms, lag_p95_ms, lag_max_ms,
    apply_ms, cleanup_ms, switch_wait_ms, wait_for_data_ms, retries)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)";

pub async fn insert<C: GenericClient>(client: &C, r: &BatchRecord) -> Result<()> {
    client
        .execute(
            INSERT_SQL,
            &[
                &r.project,
                &r.worker,
                &r.source_table,
                &r.generation,
                &r.started_at,
                &r.finished_at,
                &r.rows_read,
                &r.rows_distinct,
                &r.rows_applied,
                &r.lag.p50_ms,
                &r.lag.p95_ms,
                &r.lag.max_ms,
                &r.apply_ms,
                &r.cleanup_ms,
                &r.switch_wait_ms,
                &r.wait_for_data_ms,
                &r.retries,
            ],
        )
        .await?;
    Ok(())
}
