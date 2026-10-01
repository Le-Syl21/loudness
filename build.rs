//! Records which vpin this build reads tables with.
//!
//! An extraction manifest names the vpin that produced it, since vpin is what
//! decides how a sound comes out of a table. Cargo only tells a crate its own
//! version, so the one of a dependency is read from the lock file.

use std::env;
use std::fs;
use std::path::Path;

fn main() {
    println!("cargo::rerun-if-changed=Cargo.lock");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let version = fs::read_to_string(Path::new(&manifest_dir).join("Cargo.lock"))
        .ok()
        .and_then(|lock| locked_version(&lock, "vpin"))
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo::rustc-env=VPIN_VERSION={version}");
}

/// Version of `name` in a Cargo.lock, if it is listed.
fn locked_version(lock: &str, name: &str) -> Option<String> {
    let wanted = format!("name = \"{name}\"");
    let mut lines = lock.lines().map(str::trim);
    lines.find(|line| *line == wanted)?;
    let line = lines.next()?;
    let version = line.strip_prefix("version = \"")?.strip_suffix('"')?;
    Some(version.to_string())
}
