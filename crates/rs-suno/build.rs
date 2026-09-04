#![forbid(unsafe_code)]

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=SUNO_TARGET={target}");
    println!("cargo:rerun-if-env-changed=SUNO_VERSION_SUFFIX");
    let suffix = std::env::var("SUNO_VERSION_SUFFIX").unwrap_or_default();
    println!("cargo:rustc-env=SUNO_VERSION_SUFFIX={suffix}");
}
