// @MX:ANCHOR: [AUTO] connect() — pool initializer and migration runner
// @MX:REASON: Every DB consumer (main, workers, integration tests) calls this exactly once per
//             process start. Changing pool config or the migration path here affects all call sites.
//             fan_in >= 3 (main, integration tests, future SPEC-SCHED-001 workers).

use anyhow::Result;
use sqlx::{postgres::PgPoolOptions, PgPool};
use tracing::info;

/// Initialize a PostgreSQL connection pool and apply sqlx migrations.
///
/// Uses `sqlx::migrate!("./migrations")` — runtime migration application from embedded SQL files.
/// No DATABASE_URL is needed at compile time; no `.sqlx/` offline cache is required for this SPEC.
/// Query macros (`query!`, `query_as!`) are deliberately not used in SPEC-DB-001 to keep the
/// build offline-friendly. Future SPECs may add compile-time-checked queries with an offline cache.
pub async fn connect(database_url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    info!("migrations applied successfully");
    Ok(pool)
}
