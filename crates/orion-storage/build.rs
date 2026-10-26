fn main() {
    // `sqlx::migrate!` embeds the migration directories at macro expansion, so
    // a NEW migration file is invisible until the crate rebuilds.
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-changed=build.rs");
}
