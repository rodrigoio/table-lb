//! Connection handling and error classification.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, NoTls};

/// Connects to the database, retrying while the server is starting up.
pub async fn connect(url: &str, application_name: &str) -> Result<Client> {
    let mut attempt = 0u32;
    loop {
        match tokio_postgres::connect(url, NoTls).await {
            Ok((client, connection)) => {
                tokio::spawn(async move {
                    if let Err(err) = connection.await {
                        eprintln!("connection closed with error: {err}");
                    }
                });
                client
                    .batch_execute(&format!("SET application_name = '{application_name}'"))
                    .await
                    .context("setting application_name")?;
                return Ok(client);
            }
            Err(err) if attempt < 120 => {
                attempt += 1;
                if attempt % 10 == 1 {
                    eprintln!("waiting for database ({err})");
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(err) => return Err(err).context("connecting to database"),
        }
    }
}

/// Errors that are expected under concurrency and safe to retry:
/// deadlocks, serialization failures and the MERGE insert race
/// (two workers inserting the same new order at the same time).
pub fn is_retryable(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<tokio_postgres::Error>())
        .filter_map(|pg| pg.code())
        .any(|code| {
            *code == SqlState::T_R_DEADLOCK_DETECTED
                || *code == SqlState::T_R_SERIALIZATION_FAILURE
                || *code == SqlState::UNIQUE_VIOLATION
        })
}
