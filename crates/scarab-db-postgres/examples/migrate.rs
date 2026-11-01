//! One-shot migration runner: connect, migrate, exit.
//!
//! For harnesses that have a Postgres but no control-plane process to migrate
//! it: a test process that stands in for the control plane while a workspace
//! service needs the ADR-0067 fence/pack tables to exist. (The kind CI tier
//! used it until 2026-11-01; `just e2e` now migrates through the release's
//! own server.) It runs the SAME migrator
//! the server runs on boot (`PostgresDb::migrate`), so a database prepared
//! here is indistinguishable from one a converged boot prepared.
//!
//! An example rather than a bin on purpose: the workspace role must never
//! migrate (ADR-0067 part 2), and a shipped `scarab-migrate` binary in the
//! image would be an invitation to run it from N replicas. Examples do not
//! ship.
//!
//! ```text
//! cargo run -p scarab-db-postgres --example migrate -- <postgres-url>
//! # or: SCARAB_DATABASE_URL=... cargo run -p scarab-db-postgres --example migrate
//! ```

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let url = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("SCARAB_DATABASE_URL").ok())
        .filter(|u| !u.is_empty())
        .expect("usage: migrate <postgres-url>  (or set SCARAB_DATABASE_URL)");
    let db = scarab_db_postgres::PostgresDb::connect(&url)
        .await
        .expect("connect");
    db.migrate().await.expect("migrate");
    println!("migrations applied");
}
