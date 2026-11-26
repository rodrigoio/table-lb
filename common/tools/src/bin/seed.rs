//! Seeds the `orders` table with historical orders using portable
//! multi-row INSERT statements (no COPY, no generate_series).
//!
//! Environment:
//!   DATABASE_URL   connection string
//!   SEED_ORDERS    number of historical orders (default 1_000_000)
//!   SEED_BATCH     rows per INSERT statement (default 1_000)

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

#[tokio::main]
async fn main() -> Result<()> {
    let total: i64 = config::env_or("SEED_ORDERS", 1_000_000);
    let batch: i64 = config::env_or("SEED_BATCH", 1_000);
    let client = db::connect(&config::database_url(), "seed").await?;

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
