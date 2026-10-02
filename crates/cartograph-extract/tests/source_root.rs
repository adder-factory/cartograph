//! Integration coverage for Cartograph native extraction contracts.

mod dependency_ownership;

use std::assert_matches;
use std::fs;

use cartograph_domain::NormalizedPath;
use cartograph_extract::{SourceLimits, SourceReadError, SourceReadOptions, SourceRoot};

#[test]
fn bounded_reader_hashes_supported_files_and_rejects_oversized_inputs() {
    let root_dir = tempdir();
    let source_dir = root_dir.path().join("src");
    if let Err(error) = fs::create_dir(&source_dir) {
        panic!("test source directory creation failed: {error}");
    }
    if let Err(error) = fs::write(source_dir.join("service.ts"), b"export const value = 1;\n") {
        panic!("test source write failed: {error}");
    }
    if let Err(error) = fs::write(source_dir.join("large.ts"), [0_u8; 65]) {
        panic!("test large source write failed: {error}");
    }

    let root = source_root(root_dir.path());
    let snapshot = read(&root, "src/service.ts", limits(64));
    assert_eq!(snapshot.path().as_str(), "src/service.ts");
    assert_eq!(snapshot.source(), "export const value = 1;\n");
    assert_matches!(
        root.read(&path("src/large.ts"), limits(64)),
        Err(SourceReadError::SourceTooLarge)
    );
    assert_matches!(
        root.read_with_cancellation(
            &path("src/service.ts"),
            SourceReadOptions::new(limits(64), || true),
        ),
        Err(SourceReadError::Cancelled)
    );
}

#[test]
fn reader_streams_utf8_across_chunk_boundaries_and_rejects_incomplete_tail() {
    let root_dir = tempdir();
    let mut split = vec![b'a'; (64 * 1024) - 1];
    split.extend_from_slice("é".as_bytes());
    if let Err(error) = fs::write(root_dir.path().join("split.ts"), &split) {
        panic!("test split UTF-8 source write failed: {error}");
    }
    if let Err(error) = fs::write(root_dir.path().join("tail.ts"), [b'a', 0xc3]) {
        panic!("test invalid UTF-8 source write failed: {error}");
    }

    let root = source_root(root_dir.path());
    let snapshot = read(&root, "split.ts", limits(split.len()));
    assert_eq!(snapshot.source().as_bytes(), split);
    assert_matches!(
        root.read(&path("tail.ts"), limits(16)),
        Err(SourceReadError::InvalidSnapshot)
    );
}

#[test]
fn source_debug_output_redacts_source_path_and_project_root() {
    let root_dir = tempdir();
    let secret_fragment = "do-not-render-this-secret";
    let secret_path = "private/credential.ts";
    let source_dir = root_dir.path().join("private");
    if let Err(error) = fs::create_dir(&source_dir) {
        panic!("test private source directory creation failed: {error}");
    }
    if let Err(error) = fs::write(source_dir.join("credential.ts"), secret_fragment) {
        panic!("test private source write failed: {error}");
    }

    let root = source_root(root_dir.path());
    let snapshot = read(&root, secret_path, limits(1024));
    let snapshot_debug = format!("{snapshot:?}");
    let root_debug = format!("{root:?}");
    assert!(!snapshot_debug.contains(secret_fragment));
    assert!(!snapshot_debug.contains(secret_path));
    assert!(!root_debug.contains(&root_dir.path().display().to_string()));
    assert!(root_debug.contains("<redacted>"));
}

#[cfg(unix)]
#[test]
fn reader_rejects_symlink_targets_outside_the_canonical_project_root() {
    use std::os::unix::fs::symlink;

    let root_dir = tempdir();
    let outside = tempdir();
    let outside_file = outside.path().join("outside.ts");
    if let Err(error) = fs::write(&outside_file, b"export const secret = true;\n") {
        panic!("test outside source write failed: {error}");
    }
    if let Err(error) = symlink(&outside_file, root_dir.path().join("linked.ts")) {
        panic!("test source symlink failed: {error}");
    }

    let root = source_root(root_dir.path());
    assert_matches!(
        root.read(&path("linked.ts"), limits(1024)),
        Err(SourceReadError::OutsideRoot)
    );
}

#[cfg(unix)]
#[test]
fn reader_rejects_fifo_before_opening_the_blocking_target() {
    let root_dir = tempdir();
    let fifo = root_dir.path().join("events.ts");
    let status = match std::process::Command::new("mkfifo").arg(&fifo).status() {
        Ok(status) => status,
        Err(error) => panic!("test FIFO creation failed: {error}"),
    };
    assert!(status.success());

    let root = source_root(root_dir.path());
    assert_matches!(
        root.read(&path("events.ts"), limits(1024)),
        Err(SourceReadError::NotRegularFile)
    );
}

fn tempdir() -> tempfile::TempDir {
    match tempfile::tempdir() {
        Ok(directory) => directory,
        Err(error) => panic!("test tempdir creation failed: {error}"),
    }
}

fn source_root(path: &std::path::Path) -> SourceRoot {
    match SourceRoot::open(path) {
        Ok(root) => root,
        Err(error) => panic!("test source root failed: {error}"),
    }
}

fn path(raw: &str) -> NormalizedPath {
    match NormalizedPath::parse(raw) {
        Ok(path) => path,
        Err(error) => panic!("test normalized path failed: {error}"),
    }
}

fn limits(bytes: usize) -> SourceLimits {
    match SourceLimits::new(bytes) {
        Ok(limits) => limits,
        Err(error) => panic!("test source limits failed: {error}"),
    }
}

fn read(
    root: &SourceRoot,
    raw_path: &str,
    limits: SourceLimits,
) -> cartograph_extract::SourceSnapshot {
    match root.read(&path(raw_path), limits) {
        Ok(snapshot) => snapshot,
        Err(error) => panic!("test bounded source read failed: {error}"),
    }
}
