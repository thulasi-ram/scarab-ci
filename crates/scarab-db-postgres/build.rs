fn main() {
    // `sqlx::migrate!` embeds ./migrations at compile time, but adding a .sql
    // file changes no Rust source, so an incremental build happily reuses the
    // stale rlib and the new table "does not exist" at runtime. Slice 2 of
    // ADR-0067 lost an hour to exactly that.
    println!("cargo:rerun-if-changed=migrations");
}
