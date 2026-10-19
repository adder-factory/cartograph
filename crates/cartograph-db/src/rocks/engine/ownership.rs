use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

use super::{
    ContentDigest, DB, FORMAT_KEY, FORMAT_VERSION, IteratorMode, Options, PROJECT_KEY, Result,
    RocksError, family_names, merge_counter, operation,
};

const MARKER: &str = "CARTOGRAPH_OWNER";
const HEADER: &[u8] = b"Cartograph RocksDB owner\ncartograph-rocks-v1\n";

#[cfg(unix)]
pub(super) fn secure_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .map_err(|_| RocksError::Directory)?;
    secure_owned_directory(&directory, rustix::process::geteuid().as_raw())
}

#[cfg(unix)]
fn secure_owned_directory(directory: &File, uid: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let metadata = directory.metadata().map_err(|_| RocksError::Directory)?;
    if !metadata.is_dir() || metadata.uid() != uid {
        return Err(RocksError::Directory);
    }
    if metadata.mode() & 0o7777 != 0o700 {
        // Change only the verified owned descriptor, never a path that could
        // have been replaced by a symlink between inspection and chmod.
        directory
            .set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|_| RocksError::Directory)?;
    }
    let protected = directory.metadata().map_err(|_| RocksError::Directory)?;
    if protected.uid() != uid || protected.mode() & 0o7777 != 0o700 {
        return Err(RocksError::Directory);
    }
    Ok(())
}

pub(super) struct PrimaryLock(pub(super) File);

impl Drop for PrimaryLock {
    fn drop(&mut self) {
        // Closing one descriptor need not release flock while a concurrent
        // child is between fork and close-on-exec. The owner explicitly
        // releases its lock after the native database has been destroyed.
        let _released = self.0.unlock();
    }
}

/// Classify another native owner without parsing an engine error string or
/// attempting recovery while its database is open. Keep this guard until the
/// native handle has closed; never remove the lock file while owners exist.
pub(super) fn lock_primary(state: &Path) -> Result<PrimaryLock> {
    let path = state.join("native-primary.lock");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) | Err(_) => return Err(RocksError::Directory),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|_| RocksError::Directory)?;
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(RocksError::Directory);
    }
    match file.try_lock() {
        Ok(()) => Ok(PrimaryLock(file)),
        Err(std::fs::TryLockError::WouldBlock) => Err(RocksError::WriterBusy),
        Err(_) => Err(operation("primary_lock")),
    }
}

/// A pre-existing nonempty directory is never claimed by opening its database.
pub(super) fn admit(directory: &Path, identity: &ContentDigest) -> Result<()> {
    match fs::symlink_metadata(directory.join(MARKER)) {
        Ok(_) => validate(directory, identity),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if fs::read_dir(directory)
                .map_err(|_| RocksError::Directory)?
                .next()
                .is_some()
            {
                return Err(RocksError::Format);
            }
            stamp_checkpoint(directory, identity)
        }
        Err(_) => Err(RocksError::Directory),
    }
}

/// Validate an existing native catalog without recovering or modifying it in
/// writable mode. A valid outer marker cannot conceal incompatible inner data.
pub(super) fn validate_native(directory: &Path, identity: &ContentDigest) -> Result<()> {
    if !directory
        .join("CURRENT")
        .try_exists()
        .map_err(|_| RocksError::Directory)?
    {
        return Ok(());
    }
    let mut options = Options::default();
    options.set_merge_operator_associative("cartograph_u64_add_v1", merge_counter);
    let mut names = DB::list_cf(&options, directory).map_err(|_| operation("read_format"))?;
    names.sort();
    let mut expected = family_names().collect::<Vec<_>>();
    expected.sort_unstable();
    if names.iter().any(|name| !expected.contains(&name.as_str())) {
        return Err(RocksError::Format);
    }
    let native = DB::open_cf_for_read_only(&options, directory, &names, false)
        .map_err(|_| operation("read_format"))?;
    match native
        .get(FORMAT_KEY)
        .map_err(|_| operation("read_format"))?
    {
        Some(format) => {
            if format != FORMAT_VERSION || names.len() != expected.len() {
                return Err(RocksError::Format);
            }
            if native
                .get(PROJECT_KEY)
                .map_err(|_| operation("read_format"))?
                .as_deref()
                != Some(identity.as_str().as_bytes())
            {
                return Err(RocksError::ProjectMismatch);
            }
        }
        None => {
            // First native open creates CFs individually. It is safe to finish
            // an owned subset only while every existing CF is still empty.
            for name in names {
                let family = native.cf_handle(&name).ok_or(RocksError::Format)?;
                if native
                    .iterator_cf(&family, IteratorMode::Start)
                    .next()
                    .is_some()
                {
                    return Err(RocksError::Format);
                }
            }
        }
    }
    Ok(())
}

/// Only the caller that just created a native checkpoint may stamp a nonempty
/// directory. The identity remains that of its source; rebinding is explicit.
pub(super) fn stamp_checkpoint(directory: &Path, identity: &ContentDigest) -> Result<()> {
    // A crash while preparing the marker can leave only a temporary sibling
    // outside the database. It cannot turn the unowned, empty DB directory
    // into a nonempty foreign directory or expose a partial authoritative file.
    let parent = directory.parent().ok_or(RocksError::Directory)?;
    let mut pending = tempfile::Builder::new()
        .prefix(".cartograph-rocks-owner-")
        .tempfile_in(parent)
        .map_err(|_| RocksError::Directory)?;
    pending
        .write_all(&encoded(identity))
        .and_then(|()| pending.as_file().sync_all())
        .map_err(|_| RocksError::Directory)?;
    match pending.persist_noclobber(directory.join(MARKER)) {
        Ok(_) => {
            #[cfg(unix)]
            for path in [directory, parent] {
                File::open(path)
                    .and_then(|file| file.sync_all())
                    .map_err(|_| RocksError::Directory)?;
            }
            Ok(())
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate(directory, identity)
        }
        Err(_) => Err(RocksError::Directory),
    }
}

fn validate(directory: &Path, identity: &ContentDigest) -> Result<()> {
    let path = directory.join(MARKER);
    let metadata = fs::symlink_metadata(&path).map_err(|_| RocksError::Directory)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 1_024 {
        return Err(RocksError::Directory);
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(1_025).read_to_end(&mut bytes))
        .map_err(|_| RocksError::Directory)?;
    if !bytes.starts_with(HEADER) || bytes.len() != encoded(identity).len() {
        return Err(RocksError::Format);
    }
    if bytes != encoded(identity) {
        return Err(RocksError::ProjectMismatch);
    }
    Ok(())
}

fn encoded(identity: &ContentDigest) -> Vec<u8> {
    let mut bytes = HEADER.to_vec();
    bytes.extend_from_slice(identity.as_str().as_bytes());
    bytes.push(b'\n');
    bytes
}

#[cfg(test)]
mod primary_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn directory_with_a_different_owner_is_refused_without_changing_permissions()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let root = tempfile::tempdir()?;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755))?;
        let directory = File::open(root.path())?;
        let original = directory.metadata()?;
        let other_uid = original.uid().checked_add(1).ok_or("uid overflow")?;
        assert_eq!(
            secure_owned_directory(&directory, other_uid),
            Err(RocksError::Directory)
        );
        assert_eq!(directory.metadata()?.mode(), original.mode());
        Ok(())
    }

    #[test]
    fn inherited_descriptor_does_not_retain_or_release_a_completed_owners_lock()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let primary = lock_primary(root.path())?;
        // A duplicate deterministically retains the same open description as
        // a child between fork and close-on-exec. It gains no unlock-on-drop.
        let inherited = primary.0.try_clone()?;
        assert!(matches!(
            lock_primary(root.path()),
            Err(RocksError::WriterBusy)
        ));
        drop(primary);
        let reopened = lock_primary(root.path())?;
        drop(inherited);
        assert!(matches!(
            lock_primary(root.path()),
            Err(RocksError::WriterBusy)
        ));
        drop(reopened);
        assert!(lock_primary(root.path()).is_ok());
        Ok(())
    }
}
