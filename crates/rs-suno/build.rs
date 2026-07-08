#![forbid(unsafe_code)]

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=SUNO_TARGET={target}");
}
