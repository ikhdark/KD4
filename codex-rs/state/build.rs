fn main() {
    // Stable sqlx::migrate! tracks existing files, but not new directory members.
    for directory in [
        "migrations",
        "logs_migrations",
        "goals_migrations",
        "bugs_migrations",
    ] {
        println!("cargo:rerun-if-changed={directory}");
    }
}
