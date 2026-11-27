//! Seeds the `orders` table with historical orders using portable
//! multi-row INSERT statements (no COPY, no generate_series).
//!
//! Environment:
//!   DATABASE_URL   connection string
//!   SEED_ORDERS    number of historical orders (default 1_000_000)
//!   SEED_BATCH     rows per INSERT statement (default 1_000)
//!   LOCK_SLOTS     project B only: number of rows of buffer_control (one per
//!                  lock stripe). The schema creates 64; producers lock
//!                  slot = (client + offset) % LOCK_SLOTS, so the table must
//!                  hold exactly that many rows before any producer starts.

use std::time::Instant;

use anyhow::Result;
use chrono::{Duration, NaiveDate, NaiveDateTime};
use tlb_core::{config, db};
use tokio_postgres::types::ToSql;

const COLUMNS: usize = 6;

fn insert_sql(rows: usize) -> String {
    let values = (0..rows)
        .map(|r| {
            let base = r * COLUMNS;
            format!(
                "(${}, ${}, ${}, ${}, ${}, ${})",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO orders (id, status_code, source_ref, data, created_at, updated_at) VALUES {values}"
    )
}

/// Historical distribution: mostly delivered orders, a tail of open ones.
fn status_for(id: i64) -> i16 {
    match id % 100 {
        0..=3 => 0,
        4..=7 => 1,
        8..=14 => 2,
        _ => 3,
    }
}

/// Project B: makes buffer_control hold exactly `slots` rows (0..slots-1),
/// all with the current active buffer and generation. Portable SQL only.
async fn resize_controller(client: &tokio_postgres::Client, slots: i32) -> Result<()> {
    anyhow::ensure!((1..=32_767).contains(&slots), "LOCK_SLOTS must be between 1 and 32767");
    let exists: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'buffer_control'",
            &[],
        )
        .await?
        .get(0);
    if exists == 0 {
        return Ok(());
    }
    client
        .execute("DELETE FROM buffer_control WHERE slot >= CAST($1 AS INTEGER)", &[&slots])
        .await?;
    client
        .execute(
            "INSERT INTO buffer_control (slot, active, generation, switched_at)
             WITH RECURSIVE slots (n) AS (
                 SELECT 0
                 UNION ALL
                 SELECT n + 1 FROM slots WHERE n + 1 < CAST($1 AS INTEGER)
             )
             SELECT n,
                    (SELECT MAX(active) FROM buffer_control),
                    (SELECT MAX(generation) FROM buffer_control),
                    CURRENT_TIMESTAMP
               FROM slots
              WHERE NOT EXISTS (SELECT 1 FROM buffer_control c WHERE c.slot = slots.n)",
            &[&slots],
        )
        .await?;
    let rows: i64 = client.query_one("SELECT COUNT(*) FROM buffer_control", &[]).await?.get(0);
    println!("buffer_control: {rows} lock slots");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let total: i64 = config::env_or("SEED_ORDERS", 1_000_000);
    let batch: i64 = config::env_or("SEED_BATCH", 1_000);
    let client = db::connect(&config::database_url(), "seed").await?;

    if let Some(slots) = std::env::var("LOCK_SLOTS").ok().and_then(|v| v.parse::<i32>().ok()) {
        resize_controller(&client, slots).await?;
    }

    let already: i64 = client
        .query_one("SELECT COALESCE(MAX(id), 0) FROM orders WHERE id <= $1", &[&total])
        .await?
        .get(0);
    if already >= total {
        println!("orders already seeded ({total} rows), nothing to do");
        return Ok(());
    }

    let epoch: NaiveDateTime = NaiveDate::from_ymd_opt(2026, 1, 1)
        .expect("valid date")
        .and_hms_opt(0, 0, 0)
        .expect("valid time");
    let data = "x".repeat(128);
    let source_ref = "seed".to_string();
    let full_stmt = client.prepare(&insert_sql(batch as usize)).await?;

    let started = Instant::now();
    let mut next = already + 1;
    while next <= total {
        let last = (next + batch - 1).min(total);
        let rows = (last - next + 1) as usize;

        let mut ids = Vec::with_capacity(rows);
        let mut statuses = Vec::with_capacity(rows);
        let mut created = Vec::with_capacity(rows);
        let mut updated = Vec::with_capacity(rows);
        for id in next..=last {
            let created_at = epoch + Duration::seconds(id);
            ids.push(id);
            statuses.push(status_for(id));
            created.push(created_at);
            updated.push(created_at + Duration::seconds(id % 86_400));
        }

        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(rows * COLUMNS);
        for i in 0..rows {
            params.push(&ids[i]);
            params.push(&statuses[i]);
            params.push(&source_ref);
            params.push(&data);
            params.push(&created[i]);
            params.push(&updated[i]);
        }

        if rows == batch as usize {
            client.execute(&full_stmt, &params).await?;
        } else {
            client.execute(insert_sql(rows).as_str(), &params).await?;
        }

        if last % 100_000 == 0 || last == total {
            println!("seeded {last}/{total} orders ({:.1}s)", started.elapsed().as_secs_f64());
        }
        next = last + 1;
    }

    // Maintenance statement, not part of the strategies: fresh statistics
    // give both projects the same starting plans.
    client.batch_execute("ANALYZE orders").await?;
    println!("seed finished in {:.1}s", started.elapsed().as_secs_f64());
    Ok(())
}
