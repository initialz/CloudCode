// Emit the build's Rust target triple into a compile-time env var so the
// agent can report it in its hello frame.
fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=CLOUDCODE_TARGET={target}");
}
