//! The connection pool and the write transaction every change goes through.

use std::{
    str::FromStr,
    time::{Duration, Instant},
};

use sqlx::{
    PgPool, Postgres, Transaction,
    postgres::{PgConnectOptions, PgPoolOptions},
};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// The name the change triggers record as the client for the server's own
/// writes.
const APPLICATION_NAME: &str = "house-mcp";

pub async fn connect(url: &str) -> Result<PgPool, sqlx::Error> {
    let options = PgConnectOptions::from_str(url)?
        .application_name(APPLICATION_NAME)
        .options([("statement_timeout", "30s")]);
    PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(options)
        .await
}

/// How long to keep trying to reach the database, and how long to wait
/// between tries. Each wait doubles, up to `longest_wait`.
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    pub give_up_after: Duration,
    pub first_wait: Duration,
    pub longest_wait: Duration,
}

impl Retry {
    /// At start, the database may still be starting too, or restarting.
    pub const STARTUP: Self = Self {
        give_up_after: Duration::from_secs(300),
        first_wait: Duration::from_secs(1),
        longest_wait: Duration::from_secs(30),
    };

    /// One try.
    pub const ONCE: Self = Self {
        give_up_after: Duration::ZERO,
        first_wait: Duration::ZERO,
        longest_wait: Duration::ZERO,
    };
}

/// Connects, trying again with a growing wait while the database cannot be
/// reached, and gives up with the last error once the next wait would pass
/// `retry.give_up_after`. Each failed try is logged at info: a database that
/// is still starting is expected, not an error.
pub async fn connect_with_retry(url: &str, retry: Retry) -> Result<PgPool, sqlx::Error> {
    let started = Instant::now();
    let mut wait = retry.first_wait;
    let mut attempt = 1;
    loop {
        tracing::info!("connecting to the database, attempt {attempt}");
        match connect(url).await {
            Ok(pool) => {
                tracing::info!("connected to the database on attempt {attempt}");
                return Ok(pool);
            }
            Err(e) => {
                if started.elapsed() + wait > retry.give_up_after {
                    tracing::info!(
                        "database connection attempt {attempt} failed: {e}; giving up after {:.0?}",
                        started.elapsed()
                    );
                    return Err(e);
                }
                tracing::info!("database connection attempt {attempt} failed: {e}; trying again in {wait:.0?}");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(retry.longest_wait);
                attempt += 1;
            }
        }
    }
}

/// Opens a transaction whose changes the triggers record under this author
/// and reason. Both settings are local, so they end with the transaction and
/// never leak to the next user of the pooled connection.
pub async fn begin_write(
    pool: &PgPool,
    author: &str,
    reason: &str,
) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('house.author', $1, true), set_config('house.reason', $2, true)")
        .bind(author)
        .bind(reason)
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
