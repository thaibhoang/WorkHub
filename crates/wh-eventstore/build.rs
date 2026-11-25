// Rebuild when a migration is added: `sqlx::migrate!` embeds the directory at
// compile time but does not track new files on its own.
fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
}
