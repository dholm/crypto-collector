// Force a rebuild whenever the migrations directory changes.
//
// `src/db/pool.rs` embeds the SQL files via `sqlx::migrate!("./migrations")` at compile time.
// Cargo does not otherwise treat those `.sql` files as inputs, so adding or editing a migration
// without touching Rust source would leave the compiled binary embedding a stale migration set —
// the new migration silently never runs at startup. Tracking the directory here makes any
// migration change invalidate the build cache so the embedded set is always current.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
