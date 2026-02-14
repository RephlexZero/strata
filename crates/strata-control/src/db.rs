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

/// Insert development seed data (admin user, test sender, test destination).
/// Activated by setting `DEV_SEED=1` environment variable.
pub async fn seed_dev_data(pool: &PgPool) -> anyhow::Result<()> {
    // Admin user: admin@strata.local / admin
    let admin_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id = $1)")
        .bind("usr_00000000-0000-0000-0000-000000000001")
        .fetch_one(pool)
        .await?;

    if admin_exists {
        tracing::info!("dev seed data already exists, skipping");
        return Ok(());
    }

    // Hash "admin" with argon2id
    let password_hash = strata_common::auth::hash_password("admin")?;

    sqlx::query("INSERT INTO users (id, email, password_hash, role) VALUES ($1, $2, $3, $4)")
        .bind("usr_00000000-0000-0000-0000-000000000001")
        .bind("admin@strata.local")
        .bind(&password_hash)
        .bind("admin")
        .execute(pool)
        .await?;

    sqlx::query(
        "INSERT INTO senders (id, owner_id, name, hostname, enrollment_token, enrolled) VALUES ($1, $2, $3, $4, $5, $6)"
    )
    .bind("snd_00000000-0000-0000-0000-000000000001")
    .bind("usr_00000000-0000-0000-0000-000000000001")
    .bind("Dev Simulator")
    .bind("sim-sender-01")
    .bind("dev-enrollment-token")
    .bind(false)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO destinations (id, owner_id, platform, name, url) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("dst_00000000-0000-0000-0000-000000000001")
    .bind("usr_00000000-0000-0000-0000-000000000001")
    .bind("custom_rtmp")
    .bind("Local Test Output")
    .bind("rtmp://localhost/live/test")
    .execute(pool)
    .await?;

    tracing::info!("dev seed data inserted (admin@strata.local / admin)");
    Ok(())
}
