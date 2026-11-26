//! Correctness checks for the tests in `tests/`.
//!
//! The pgbench lifecycle generator is deterministic: with CLIENTS clients and
//! TRANSACTIONS transactions per client, the set of produced (order_id, phase)
//! pairs is known in advance. This tool recomputes it and compares it with
//! the database.
//!
//! VERIFY_MODE=consumed  compare audit_consumed with the expected events
//!                       (missing, duplicated, unexpected)
//! VERIFY_MODE=state     compare orders with the expected final state
//!                       (highest phase per order) and print a state hash
//!
//! Other environment: DATABASE_URL, CLIENTS, TRANSACTIONS, WINDOW, BASE,
//! EXPECT_LOSS (consumed mode: succeed only if events were lost).
//! Exit code 0 = check passed, 1 = check failed.

use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, Result};
use tlb_core::{config, db};

const CLIENT_STRIDE: i64 = 100_000_000;

struct Generator {
    clients: i64,
    transactions: i64,
    window: i64,
    base: i64,
}

impl Generator {
    fn from_env() -> Self {
        Self {
            clients: config::env_or("CLIENTS", 1),
            transactions: config::env_or("TRANSACTIONS", 0),
            window: config::env_or("WINDOW", 50_000),
            base: config::env_or("BASE", 0),
        }
    }

    /// Same formula as the pgbench producer scripts.
    fn events(&self) -> impl Iterator<Item = (i64, i16)> + '_ {
        (0..self.clients).flat_map(move |client| {
            (0..self.transactions).map(move |e| {
                let phase = ((e / self.window) % 4) as i16;
                let order_id = self.base
                    + (client + 1) * CLIENT_STRIDE
                    + (e / (4 * self.window)) * self.window
                    + e % self.window;
                (order_id, phase)
            })
        })
    }

    fn id_range(&self) -> (i64, i64) {
        (self.base + CLIENT_STRIDE, self.base + (self.clients + 1) * CLIENT_STRIDE)
    }
}

/// FNV-1a, 64 bit: a dependency-free, stable hash of the final state.
fn fnv1a(state: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(state, |h, b| (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

async fn check_consumed(client: &tokio_postgres::Client, generator: &Generator) -> Result<bool> {
    let expected: HashMap<(i64, i16), ()> = generator.events().map(|k| (k, ())).collect();
    let mut seen: HashMap<(i64, i16), i64> = HashMap::new();
    for row in client
        .query(
            "SELECT order_id, status_code, COUNT(*) FROM audit_consumed GROUP BY order_id, status_code",
            &[],
        )
        .await?
    {
        seen.insert((row.get(0), row.get(1)), row.get(2));
    }

    let missing = expected.keys().filter(|k| !seen.contains_key(k)).count();
    let duplicated: i64 = seen.values().filter(|c| **c > 1).map(|c| c - 1).sum();
    let unexpected = seen.keys().filter(|k| !expected.contains_key(k)).count();
    let consumed: i64 = seen.values().sum();

    println!("expected_events={}", expected.len());
    println!("consumed_events={consumed}");
    println!("missing_events={missing}");
    println!("duplicated_events={duplicated}");
    println!("unexpected_events={unexpected}");

    let mut by_phase: BTreeMap<i16, usize> = BTreeMap::new();
    for (_, phase) in expected.keys().filter(|k| !seen.contains_key(k)) {
        *by_phase.entry(*phase).or_default() += 1;
    }
    if !by_phase.is_empty() {
        println!("missing_by_phase={by_phase:?}");
    }

    let exact = missing == 0 && duplicated == 0 && unexpected == 0;
    if config::env_flag("EXPECT_LOSS") {
        let passed = missing > 0;
        println!(
            "result={}",
            if passed { "PASS (events were lost, as expected without the lock)" } else { "FAIL (no loss observed)" }
        );
        Ok(passed)
    } else {
        println!("result={}", if exact { "PASS" } else { "FAIL" });
        Ok(exact)
    }
}

async fn check_state(client: &tokio_postgres::Client, generator: &Generator) -> Result<bool> {
    let mut expected: BTreeMap<i64, i16> = BTreeMap::new();
    for (order_id, phase) in generator.events() {
        let entry = expected.entry(order_id).or_insert(phase);
        *entry = (*entry).max(phase);
    }

    let (low, high) = generator.id_range();
    let rows = client
        .query(
            "SELECT id, status_code FROM orders WHERE id >= $1 AND id < $2 ORDER BY id",
            &[&low, &high],
        )
        .await?;

    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut actual: BTreeMap<i64, i16> = BTreeMap::new();
    for row in &rows {
        let id: i64 = row.get(0);
        let status: i16 = row.get(1);
        hash = fnv1a(hash, format!("{id}:{status}\n").as_bytes());
        actual.insert(id, status);
    }

    let missing = expected.keys().filter(|id| !actual.contains_key(id)).count();
    let unexpected = actual.keys().filter(|id| !expected.contains_key(id)).count();
    let wrong = expected
        .iter()
        .filter(|(id, status)| actual.get(id).is_some_and(|a| a != *status))
        .count();
    let mut by_status: BTreeMap<i16, usize> = BTreeMap::new();
    for status in actual.values() {
        *by_status.entry(*status).or_default() += 1;
    }

    println!("expected_orders={}", expected.len());
    println!("actual_orders={}", actual.len());
    println!("orders_by_status={by_status:?}");
    println!("missing_orders={missing}");
    println!("unexpected_orders={unexpected}");
    println!("wrong_state_orders={wrong}");
    println!("state_hash={hash:016x}");
    let passed = missing == 0 && unexpected == 0 && wrong == 0;
    println!("result={}", if passed { "PASS" } else { "FAIL" });
    Ok(passed)
}

#[tokio::main]
async fn main() -> Result<()> {
    let generator = Generator::from_env();
    if generator.transactions <= 0 {
        bail!("TRANSACTIONS must be set to the per-client transaction count used by pgbench");
    }
    let client = db::connect(&config::database_url(), "verify").await?;
    let passed = match config::env_string("VERIFY_MODE", "consumed").as_str() {
        "consumed" => check_consumed(&client, &generator).await?,
        "state" => check_state(&client, &generator).await?,
        other => bail!("unknown VERIFY_MODE {other:?}"),
    };
    std::process::exit(if passed { 0 } else { 1 });
}
