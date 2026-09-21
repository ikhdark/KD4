fn main() {
    // Stable sqlx::migrate! embeds the directory at compile time. Track additions
    // as well as edits so the runtime never reuses an outdated migration set.
    println!("cargo:rerun-if-changed=migrations");
}
