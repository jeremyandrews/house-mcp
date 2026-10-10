//! The connection pool and the write transaction every change goes through.

use std::{str::FromStr, time::Duration};

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
