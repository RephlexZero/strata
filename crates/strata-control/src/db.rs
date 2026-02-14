//! Database connection pool and migrations.

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// Connect to PostgreSQL and return a connection pool.
pub async fn connect(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(database_url)
        .await?;

    tracing::info!("connected to PostgreSQL");
    Ok(pool)
}

/// Run embedded SQL migrations.
pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    tracing::info!("database migrations complete");
    Ok(())
}
