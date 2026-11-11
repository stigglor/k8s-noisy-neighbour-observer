//! Builds `observer-ebpf` for the BPF target and leaves the object in `OUT_DIR`, where
//! `src/bpf.rs` embeds it.

use anyhow::{anyhow, Context};
use aya_build::Toolchain;

/// The install script holds the nightly pin, so the build and every install share one value.
const TOOLCHAIN_SCRIPT: &str = "../scripts/install-ebpf-toolchain.sh";

fn main() -> anyhow::Result<()> {
    println!("cargo:rerun-if-changed={TOOLCHAIN_SCRIPT}");
    let script = std::fs::read_to_string(TOOLCHAIN_SCRIPT)
        .with_context(|| format!("reading {TOOLCHAIN_SCRIPT}"))?;
    let nightly = script
        .lines()
        .find_map(|line| line.strip_prefix("NIGHTLY="))
        .ok_or_else(|| anyhow!("{TOOLCHAIN_SCRIPT} sets no NIGHTLY"))?;

    let metadata = cargo_metadata::MetadataCommand::new()
        .no_deps()
        .exec()
        .context("reading cargo metadata")?;
    let package = metadata
        .packages
        .iter()
        .find(|package| package.name.as_str() == "observer-ebpf")
        .ok_or_else(|| anyhow!("observer-ebpf is not in the workspace"))?;
    let root_dir = package
        .manifest_path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", package.manifest_path))?;
    let ebpf = aya_build::Package {
        name: package.name.as_str(),
        root_dir: root_dir.as_str(),
        ..Default::default()
    };
    aya_build::build_ebpf([ebpf], Toolchain::Custom(nightly))
}
