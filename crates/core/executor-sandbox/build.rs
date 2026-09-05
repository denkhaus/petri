//! Pins the exact plugin binaries supplied by the release bundler.

use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::PathBuf;

use sha2::{Digest, Sha256};

fn main() -> Result<(), Box<dyn Error>> {
    let target = env::var("TARGET")?;
    println!("cargo:rustc-env=PETRI_TARGET_TRIPLE={target}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PETRI_SANDBOX_PLUGIN_DIR");

    let mut pins = String::from("const PINNED_PLUGINS: &[(&str, &str, &str)] = &[\n");
    if let Some(directory) = env::var_os("PETRI_SANDBOX_PLUGIN_DIR") {
        let directory = PathBuf::from(directory);
        for kind in ["host", "docker", "daytona"] {
            let path = directory.join(format!("sandbox-driver-{kind}"));
            println!("cargo:rerun-if-changed={}", path.display());
            let digest = checksum(File::open(&path).map_err(|error| {
                io::Error::new(error.kind(), format!("reading {}: {error}", path.display()))
            })?)?;
            writeln!(pins, "    ({kind:?}, {target:?}, {digest:?}),")?;
        }
    }
    pins.push_str("];\n");
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").ok_or("missing OUT_DIR")?).join("plugin_pins.rs"),
        pins,
    )?;
    Ok(())
}

fn checksum(mut file: File) -> io::Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(format!("{:x}", hash.finalize()));
        }
        hash.update(&buffer[..count]);
    }
}
