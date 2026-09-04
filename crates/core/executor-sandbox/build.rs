//! Records the target triple so plugin checksum pins can be looked up per
//! target at runtime.

use std::env;

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=PETRI_TARGET_TRIPLE={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
