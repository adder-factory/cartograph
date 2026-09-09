//! Build-time fingerprinting for the native extractor contract.

use std::{
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
};

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("CARGO_MANIFEST_DIR is unavailable"))?;
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("workspace root is unavailable"))?;
    let mut inputs = Vec::new();
    collect_files(&workspace_root.join("vendor"), &mut inputs)?;
    collect_files(&manifest_dir.join("src"), &mut inputs)?;
    collect_files(
        &workspace_root.join("crates/cartograph-domain/src"),
        &mut inputs,
    )?;
    inputs.extend([
        manifest_dir.join("Cargo.toml"),
        manifest_dir.join("build.rs"),
        workspace_root.join("crates/cartograph-domain/Cargo.toml"),
        workspace_root.join("rust-toolchain.toml"),
    ]);
    inputs.push(workspace_root.join("Cargo.lock"));
    inputs.push(workspace_root.join("Cargo.toml"));
    for entry in fs::read_dir(workspace_root.join("crates"))? {
        let entry = entry?;
        let manifest = entry.path().join("Cargo.toml");
        if entry.file_type()?.is_dir() && manifest.is_file() {
            inputs.push(manifest);
        }
    }
    let cargo_config = workspace_root.join(".cargo/config.toml");
    if cargo_config.is_file() {
        inputs.push(cargo_config);
    }
    inputs.sort();
    inputs.dedup();

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cartograph-native-extractor-contract-v2\0");
    for input in inputs {
        let relative = input.strip_prefix(workspace_root)?;
        let label = relative.to_string_lossy();
        let bytes = fs::read(&input)?;
        hasher.update(label.as_bytes());
        hasher.update(&[0]);
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
        println!("cargo:rerun-if-changed={}", input.display());
    }
    let mut compilation_inputs = env::vars()
        .filter(|(key, _)| {
            key.starts_with("CARGO_FEATURE_")
                || key.starts_with("CARGO_CFG_TARGET_")
                || key == "CARGO_ENCODED_RUSTFLAGS"
        })
        .collect::<Vec<_>>();
    compilation_inputs.sort();
    for (key, value) in compilation_inputs {
        hasher.update(key.as_bytes());
        hasher.update(&[0]);
        hasher.update(value.as_bytes());
        hasher.update(&[0]);
    }
    println!(
        "cargo:rustc-env=CARTOGRAPH_NATIVE_EXTRACTOR_CONTRACT={}",
        hasher.finalize().to_hex()
    );
    Ok(())
}

fn collect_files(root: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(&entry.path(), files)?;
        } else if file_type.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}
