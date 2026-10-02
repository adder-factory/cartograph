use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::fs::OpenOptions;

use secrecy::{ExposeSecret, SecretString};
use tempfile::NamedTempFile;

use super::{ManagedDatabaseError, push_lower_hex_byte};

const DATABASE_USER: &str = "cartograph";
const DATABASE_NAME: &str = "cartograph";
const GENERATED_CREDENTIAL_BYTES: usize = 32;
const MAX_CREDENTIAL_BYTES: u64 = 128;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
#[cfg(unix)]
const OTHER_ACCESS_MODE_MASK: u32 = 0o077;
#[cfg(unix)]
const OTHER_WRITE_MODE_MASK: u32 = 0o022;
#[cfg(any(target_os = "linux", target_os = "macos"))]
const ACL_MODE_LENGTH: usize = 10;
#[cfg(any(target_os = "linux", target_os = "macos"))]
const ACL_MODE_WITH_MARKER_LENGTH: usize = 11;

pub(super) struct CredentialStore {
    path: PathBuf,
}

pub(super) struct LoadedCredentials {
    pub(super) credentials: DatabaseCredentials,
    pub(super) created: bool,
}

pub(super) struct LifecycleLock {
    _file: File,
}

pub(super) struct DatabaseCredentials {
    value: SecretString,
}

impl CredentialStore {
    pub(super) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn load(&self) -> Result<DatabaseCredentials, ManagedDatabaseError> {
        self.prepare_private_parent()?;
        reject_symlink(&self.path)?;
        let mut file = open_private_regular_file(&self.path, false)?;
        validate_private_permissions(&file, &self.path)?;
        let mut contents = String::new();
        Read::by_ref(&mut file)
            .take(MAX_CREDENTIAL_BYTES + 1)
            .read_to_string(&mut contents)
            .map_err(|_| ManagedDatabaseError::CredentialRead)?;
        let content_length =
            u64::try_from(contents.len()).map_err(|_| ManagedDatabaseError::CredentialTooLarge)?;
        if content_length > MAX_CREDENTIAL_BYTES {
            return Err(ManagedDatabaseError::CredentialTooLarge);
        }
        DatabaseCredentials::parse(&contents)
    }

    pub(super) fn load_or_create(&self) -> Result<LoadedCredentials, ManagedDatabaseError> {
        if self.path.exists() {
            return self.load().map(|credentials| LoadedCredentials {
                credentials,
                created: false,
            });
        }

        self.prepare_private_parent()?;
        let parent = self
            .path
            .parent()
            .ok_or(ManagedDatabaseError::CredentialPath)?;
        let credentials = DatabaseCredentials::generate()?;
        let mut temporary =
            NamedTempFile::new_in(parent).map_err(|_| ManagedDatabaseError::CredentialWrite)?;
        set_private_permissions(temporary.as_file(), temporary.path())?;
        temporary
            .write_all(credentials.render_value().as_bytes())
            .map_err(|_| ManagedDatabaseError::CredentialWrite)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|_| ManagedDatabaseError::CredentialWrite)?;

        match temporary.persist_noclobber(&self.path) {
            Ok(_) => Ok(LoadedCredentials {
                credentials,
                created: true,
            }),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.load().map(|credentials| LoadedCredentials {
                    credentials,
                    created: false,
                })
            }
            Err(_) => Err(ManagedDatabaseError::CredentialWrite),
        }
    }

    pub(super) fn acquire_lifecycle_lock(&self) -> Result<LifecycleLock, ManagedDatabaseError> {
        self.prepare_private_parent()?;
        let parent = self
            .path
            .parent()
            .ok_or(ManagedDatabaseError::CredentialPath)?;
        let lock_path = parent.join("lifecycle.lock");
        reject_symlink(&lock_path)?;
        let file = open_private_regular_file(&lock_path, true)?;
        set_private_permissions(&file, &lock_path)?;
        validate_private_permissions(&file, &lock_path)?;
        match file.try_lock() {
            Ok(()) => Ok(LifecycleLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(ManagedDatabaseError::LifecycleBusy),
            Err(_) => Err(ManagedDatabaseError::CredentialWrite),
        }
    }

    pub(super) fn validate_for_removal(&self) -> Result<bool, ManagedDatabaseError> {
        self.prepare_private_parent()?;
        reject_symlink(&self.path)?;
        if !self.path.exists() {
            return Ok(false);
        }
        let file = open_private_regular_file(&self.path, false)?;
        validate_private_permissions(&file, &self.path)?;
        Ok(true)
    }

    pub(super) fn remove(&self) -> Result<bool, ManagedDatabaseError> {
        if !self.validate_for_removal()? {
            return Ok(false);
        }
        fs::remove_file(&self.path).map_err(|_| ManagedDatabaseError::CredentialRemove)?;
        Ok(true)
    }

    fn prepare_private_parent(&self) -> Result<(), ManagedDatabaseError> {
        let parent = self
            .path
            .parent()
            .ok_or(ManagedDatabaseError::CredentialPath)?;
        let state_root = parent
            .parent()
            .ok_or(ManagedDatabaseError::CredentialPath)?;
        let project_root = state_root
            .parent()
            .ok_or(ManagedDatabaseError::CredentialPath)?;
        reject_symlink(parent)?;
        reject_symlink(state_root)?;
        validate_safe_ancestor(project_root)?;
        if state_root.exists() {
            validate_safe_ancestor(state_root)?;
        }
        fs::create_dir_all(parent).map_err(|_| ManagedDatabaseError::CredentialWrite)?;
        validate_safe_ancestor(state_root)?;
        set_private_directory_permissions(parent)?;
        validate_private_directory(parent)
    }
}

impl DatabaseCredentials {
    fn generate() -> Result<Self, ManagedDatabaseError> {
        let mut random = [0_u8; GENERATED_CREDENTIAL_BYTES];
        getrandom::fill(&mut random).map_err(|_| ManagedDatabaseError::CredentialRandom)?;
        let mut credential = String::with_capacity(GENERATED_CREDENTIAL_BYTES * 2);
        for byte in random {
            push_lower_hex_byte(&mut credential, byte);
        }
        Ok(Self {
            value: SecretString::from(credential),
        })
    }

    fn parse(contents: &str) -> Result<Self, ManagedDatabaseError> {
        let credential = contents.strip_suffix('\n').unwrap_or(contents);
        if credential.len() < 24
            || credential.contains(['\r', '\n'])
            || !credential
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            return Err(ManagedDatabaseError::CredentialFormat);
        }
        Ok(Self {
            value: SecretString::from(credential.to_owned()),
        })
    }

    pub(super) fn database_url(&self, port: u16) -> Result<SecretString, ManagedDatabaseError> {
        let mut url = url::Url::parse("postgresql://127.0.0.1")
            .map_err(|_| ManagedDatabaseError::CredentialFormat)?;
        url.set_username(DATABASE_USER)
            .map_err(|()| ManagedDatabaseError::CredentialFormat)?;
        set_url_credential(&mut url, self.value.expose_secret())?;
        url.set_port(Some(port))
            .map_err(|()| ManagedDatabaseError::CredentialFormat)?;
        url.set_path(DATABASE_NAME);
        Ok(SecretString::from(url.to_string()))
    }

    fn render_value(&self) -> String {
        format!("{}\n", self.value.expose_secret())
    }
}

fn set_url_credential(url: &mut url::Url, credential: &str) -> Result<(), ManagedDatabaseError> {
    url.set_password(Some(credential))
        .map_err(|()| ManagedDatabaseError::CredentialFormat)
}

fn reject_symlink(path: &Path) -> Result<(), ManagedDatabaseError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(ManagedDatabaseError::CredentialRead),
    };
    if metadata.file_type().is_symlink() {
        return Err(ManagedDatabaseError::CredentialSymlink);
    }
    Ok(())
}

#[cfg(unix)]
fn open_private_regular_file(path: &Path, create: bool) -> Result<File, ManagedDatabaseError> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(path).map_err(|_| {
        if create {
            ManagedDatabaseError::CredentialWrite
        } else {
            ManagedDatabaseError::CredentialRead
        }
    })?;
    let metadata = file
        .metadata()
        .map_err(|_| ManagedDatabaseError::CredentialRead)?;
    if !metadata.file_type().is_file() {
        return Err(ManagedDatabaseError::CredentialNotRegular);
    }
    if metadata.len() > MAX_CREDENTIAL_BYTES {
        return Err(ManagedDatabaseError::CredentialTooLarge);
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_private_regular_file(_path: &Path, _create: bool) -> Result<File, ManagedDatabaseError> {
    Err(ManagedDatabaseError::CredentialAclUnsupported)
}

#[cfg(unix)]
fn validate_safe_ancestor(path: &Path) -> Result<(), ManagedDatabaseError> {
    validate_path_mode(
        path,
        OTHER_WRITE_MODE_MASK,
        ManagedDatabaseError::CredentialStatePermissions,
    )
}

#[cfg(not(unix))]
fn validate_safe_ancestor(_path: &Path) -> Result<(), ManagedDatabaseError> {
    Err(ManagedDatabaseError::CredentialAclUnsupported)
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), ManagedDatabaseError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
        .map_err(|_| ManagedDatabaseError::CredentialWrite)
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), ManagedDatabaseError> {
    Err(ManagedDatabaseError::CredentialAclUnsupported)
}

#[cfg(unix)]
fn validate_private_directory(path: &Path) -> Result<(), ManagedDatabaseError> {
    validate_path_mode(
        path,
        OTHER_ACCESS_MODE_MASK,
        ManagedDatabaseError::CredentialStatePermissions,
    )
}

#[cfg(unix)]
fn validate_path_mode(
    path: &Path,
    forbidden_mode: u32,
    permission_error: ManagedDatabaseError,
) -> Result<(), ManagedDatabaseError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(path)
        .map_err(|_| ManagedDatabaseError::CredentialRead)?
        .permissions()
        .mode();
    if mode & forbidden_mode != 0 {
        return Err(permission_error);
    }
    validate_no_extended_acl(path)
}

#[cfg(not(unix))]
fn validate_private_directory(_path: &Path) -> Result<(), ManagedDatabaseError> {
    Err(ManagedDatabaseError::CredentialAclUnsupported)
}

#[cfg(target_os = "macos")]
fn validate_no_extended_acl(path: &Path) -> Result<(), ManagedDatabaseError> {
    let output = std::process::Command::new("/bin/ls")
        .arg("-lde")
        .arg(path)
        .output()
        .map_err(|_| ManagedDatabaseError::CredentialAclUnsupported)?;
    if !output.status.success() {
        return Err(ManagedDatabaseError::CredentialAclUnsupported);
    }
    validate_acl_listing(&output.stdout)
}

#[cfg(target_os = "linux")]
fn validate_no_extended_acl(path: &Path) -> Result<(), ManagedDatabaseError> {
    let output = std::process::Command::new("/bin/ls")
        .arg("-ld")
        .arg("--")
        .arg(path)
        .output()
        .map_err(|_| ManagedDatabaseError::CredentialAclUnsupported)?;
    if !output.status.success() {
        return Err(ManagedDatabaseError::CredentialAclUnsupported);
    }
    validate_acl_listing(&output.stdout)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn validate_acl_listing(listing: &[u8]) -> Result<(), ManagedDatabaseError> {
    let listing =
        std::str::from_utf8(listing).map_err(|_| ManagedDatabaseError::CredentialAclUnsupported)?;
    let mode = listing
        .split_ascii_whitespace()
        .next()
        .ok_or(ManagedDatabaseError::CredentialAclUnsupported)?;
    let mode = mode.as_bytes();
    if !valid_acl_mode_prefix(mode) {
        return Err(ManagedDatabaseError::CredentialAclUnsupported);
    }
    match mode.len() {
        ACL_MODE_LENGTH => Ok(()),
        ACL_MODE_WITH_MARKER_LENGTH => match mode.last().copied() {
            Some(b'+') => Err(ManagedDatabaseError::CredentialExtendedAcl),
            #[cfg(target_os = "linux")]
            Some(b'.') => Ok(()),
            #[cfg(target_os = "macos")]
            Some(b'@') => Ok(()),
            Some(_) | None => Err(ManagedDatabaseError::CredentialAclUnsupported),
        },
        _ => Err(ManagedDatabaseError::CredentialAclUnsupported),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn valid_acl_mode_prefix(mode: &[u8]) -> bool {
    let ([
        file_type,
        owner_read,
        owner_write,
        owner_execute,
        group_read,
        group_write,
        group_execute,
        other_read,
        other_write,
        other_execute,
    ]
    | [
        file_type,
        owner_read,
        owner_write,
        owner_execute,
        group_read,
        group_write,
        group_execute,
        other_read,
        other_write,
        other_execute,
        _,
    ]) = mode
    else {
        return false;
    };
    matches!(*file_type, b'-' | b'd')
        && matches!(*owner_read, b'r' | b'-')
        && matches!(*owner_write, b'w' | b'-')
        && matches!(*owner_execute, b'x' | b's' | b'S' | b'-')
        && matches!(*group_read, b'r' | b'-')
        && matches!(*group_write, b'w' | b'-')
        && matches!(*group_execute, b'x' | b's' | b'S' | b'-')
        && matches!(*other_read, b'r' | b'-')
        && matches!(*other_write, b'w' | b'-')
        && matches!(*other_execute, b'x' | b't' | b'T' | b'-')
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn validate_no_extended_acl(_path: &Path) -> Result<(), ManagedDatabaseError> {
    Err(ManagedDatabaseError::CredentialAclUnsupported)
}

#[cfg(unix)]
fn set_private_permissions(file: &File, path: &Path) -> Result<(), ManagedDatabaseError> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))
        .map_err(|_| ManagedDatabaseError::CredentialWrite)?;
    validate_no_extended_acl(path)
}

#[cfg(not(unix))]
fn set_private_permissions(_file: &File, _path: &Path) -> Result<(), ManagedDatabaseError> {
    Err(ManagedDatabaseError::CredentialAclUnsupported)
}

#[cfg(unix)]
fn validate_private_permissions(file: &File, path: &Path) -> Result<(), ManagedDatabaseError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = file
        .metadata()
        .map_err(|_| ManagedDatabaseError::CredentialRead)?
        .permissions()
        .mode();
    if mode & OTHER_ACCESS_MODE_MASK != 0 {
        return Err(ManagedDatabaseError::CredentialPermissions);
    }
    validate_no_extended_acl(path)
}

#[cfg(not(unix))]
fn validate_private_permissions(_file: &File, _path: &Path) -> Result<(), ManagedDatabaseError> {
    Err(ManagedDatabaseError::CredentialAclUnsupported)
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    #[cfg(unix)]
    const TEST_DATABASE_PORT: u16 = 55_432;
    #[cfg(unix)]
    const GROUP_READABLE_FILE_MODE: u32 = 0o640;
    #[cfg(unix)]
    const GROUP_WRITABLE_DIRECTORY_MODE: u32 = 0o770;

    #[cfg(unix)]
    fn credential_store(project_root: &Path) -> CredentialStore {
        CredentialStore::new(project_root.join(".cartograph/v2/postgres.password"))
    }

    #[test]
    #[cfg(unix)]
    fn generated_credentials_are_private_reused_and_never_rendered_by_debug() {
        let directory = test_directory();
        let store = credential_store(directory.path());
        let first = load_test_credentials(&store);
        let first_url = test_database_url(&first.credentials);
        let second = load_test_credentials(&store);
        let second_url = test_database_url(&second.credentials);

        assert!(first.created);
        assert!(!second.created);
        assert_eq!(first_url.expose_secret(), second_url.expose_secret());
        assert!(!format!("{first_url:?}").contains("cartograph@"));
        assert!(!format!("{first_url:?}").contains("55432"));
        assert_private_store_modes(&store);
    }

    #[test]
    #[cfg(unix)]
    fn credential_removal_is_private_validated_and_idempotent() {
        let directory = test_directory();
        let store = credential_store(directory.path());
        let _credentials = load_test_credentials(&store);

        assert_matches!(store.validate_for_removal(), Ok(true));
        assert_matches!(store.remove(), Ok(true));
        assert!(!store.path().exists());
        assert_matches!(store.remove(), Ok(false));
    }

    #[cfg(unix)]
    fn test_directory() -> tempfile::TempDir {
        match tempfile::tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create test directory: {error}"),
        }
    }

    #[cfg(unix)]
    fn load_test_credentials(store: &CredentialStore) -> LoadedCredentials {
        match store.load_or_create() {
            Ok(credentials) => credentials,
            Err(error) => panic!("could not load test credentials: {error}"),
        }
    }

    #[cfg(unix)]
    fn test_database_url(credentials: &DatabaseCredentials) -> SecretString {
        match credentials.database_url(TEST_DATABASE_PORT) {
            Ok(url) => url,
            Err(error) => panic!("could not build test database URL: {error}"),
        }
    }

    #[cfg(unix)]
    fn assert_private_store_modes(store: &CredentialStore) {
        let Some(parent) = store.path().parent() else {
            panic!("credential path has no parent");
        };

        assert_eq!(test_mode(store.path()) & OTHER_ACCESS_MODE_MASK, 0);
        assert_eq!(test_mode(parent) & OTHER_ACCESS_MODE_MASK, 0);
    }

    #[cfg(unix)]
    fn test_mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;

        match fs::metadata(path) {
            Ok(metadata) => metadata.permissions().mode(),
            Err(error) => panic!("could not stat private test path: {error}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn rejects_credentials_readable_by_group_or_other_users() {
        use std::os::unix::fs::PermissionsExt;

        let directory = match tempfile::tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create test directory: {error}"),
        };
        let path = directory.path().join(".cartograph/v2/postgres.password");
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            panic!("could not create credential directory: {error}");
        }
        let contents = "01234567890123456789012345678901\n";
        if let Err(error) = fs::write(&path, contents) {
            panic!("could not write credentials: {error}");
        }
        if let Err(error) =
            fs::set_permissions(&path, fs::Permissions::from_mode(GROUP_READABLE_FILE_MODE))
        {
            panic!("could not set test permissions: {error}");
        }

        let error = CredentialStore::new(path).load().err();

        assert_matches!(error, Some(ManagedDatabaseError::CredentialPermissions));
    }

    #[test]
    #[cfg(unix)]
    fn rejects_symlinked_credential_file() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = match tempfile::tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create test directory: {error}"),
        };
        let parent = directory.path().join(".cartograph/v2");
        if let Err(error) = fs::create_dir_all(&parent) {
            panic!("could not create credential directory: {error}");
        }
        let target = parent.join("target.password");
        let link = parent.join("postgres.password");
        let contents = "01234567890123456789012345678901\n";
        if let Err(error) = fs::write(&target, contents) {
            panic!("could not write target credentials: {error}");
        }
        if let Err(error) =
            fs::set_permissions(&target, fs::Permissions::from_mode(PRIVATE_FILE_MODE))
        {
            panic!("could not set target permissions: {error}");
        }
        if let Err(error) = symlink(&target, &link) {
            panic!("could not create credential symlink: {error}");
        }

        assert_matches!(
            CredentialStore::new(link).load().err(),
            Some(ManagedDatabaseError::CredentialSymlink)
        );
    }

    #[test]
    #[cfg(unix)]
    fn rejects_symlinked_cartograph_state_directory() {
        use std::os::unix::fs::symlink;

        let directory = match tempfile::tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create test directory: {error}"),
        };
        let external_state = directory.path().join("external-state");
        if let Err(error) = fs::create_dir(&external_state) {
            panic!("could not create external state directory: {error}");
        }
        if let Err(error) = symlink(&external_state, directory.path().join(".cartograph")) {
            panic!("could not create state-directory symlink: {error}");
        }
        let store = CredentialStore::new(directory.path().join(".cartograph/v2/postgres.password"));

        assert_matches!(
            store.load_or_create().err(),
            Some(ManagedDatabaseError::CredentialSymlink)
        );
        assert!(!external_state.join("v2/postgres.password").exists());
    }

    #[test]
    #[cfg(unix)]
    fn lifecycle_lock_serializes_same_project_mutations() {
        let directory = match tempfile::tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create test directory: {error}"),
        };
        let store = credential_store(directory.path());
        let first = match store.acquire_lifecycle_lock() {
            Ok(lock) => lock,
            Err(error) => panic!("could not acquire first lifecycle lock: {error}"),
        };
        assert_matches!(
            store.acquire_lifecycle_lock().err(),
            Some(ManagedDatabaseError::LifecycleBusy)
        );
        drop(first);
        assert!(store.acquire_lifecycle_lock().is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn rejects_group_writable_project_root_before_creating_secret_state() {
        use std::os::unix::fs::PermissionsExt;

        let directory = match tempfile::tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create test directory: {error}"),
        };
        if let Err(error) = fs::set_permissions(
            directory.path(),
            fs::Permissions::from_mode(GROUP_WRITABLE_DIRECTORY_MODE),
        ) {
            panic!("could not set project permissions: {error}");
        }
        let store = credential_store(directory.path());

        assert_matches!(
            store.load_or_create().err(),
            Some(ManagedDatabaseError::CredentialStatePermissions)
        );
        assert!(!store.path().exists());
    }

    #[test]
    #[cfg(unix)]
    fn rejects_fifo_credential_without_blocking() {
        let directory = match tempfile::tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create test directory: {error}"),
        };
        let store = credential_store(directory.path());
        if let Err(error) = store.load_or_create() {
            panic!("could not prepare credential state: {error}");
        }
        if let Err(error) = fs::remove_file(store.path()) {
            panic!("could not remove credential fixture: {error}");
        }
        create_fifo(store.path());

        assert_matches!(
            store.load().err(),
            Some(ManagedDatabaseError::CredentialNotRegular)
        );
    }

    #[test]
    #[cfg(unix)]
    fn rejects_fifo_lifecycle_lock_without_blocking() {
        let directory = match tempfile::tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create test directory: {error}"),
        };
        let store = credential_store(directory.path());
        if let Err(error) = store.load_or_create() {
            panic!("could not prepare credential state: {error}");
        }
        let lock_path = store
            .path()
            .parent()
            .unwrap_or_else(|| panic!("credential path has no parent"))
            .join("lifecycle.lock");
        create_fifo(&lock_path);

        assert_matches!(
            store.acquire_lifecycle_lock().err(),
            Some(ManagedDatabaseError::CredentialNotRegular)
        );
    }

    #[test]
    #[cfg(unix)]
    fn rejects_oversized_credential_before_reading_it() {
        use std::os::unix::fs::PermissionsExt;

        let directory = match tempfile::tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create test directory: {error}"),
        };
        let store = credential_store(directory.path());
        if let Some(parent) = store.path().parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            panic!("could not create credential directory: {error}");
        }
        let oversized_length = match usize::try_from(MAX_CREDENTIAL_BYTES + 1) {
            Ok(length) => length,
            Err(error) => panic!("credential test bound does not fit usize: {error}"),
        };
        let oversized = "a".repeat(oversized_length);
        if let Err(error) = fs::write(store.path(), oversized) {
            panic!("could not write oversized fixture: {error}");
        }
        if let Err(error) =
            fs::set_permissions(store.path(), fs::Permissions::from_mode(PRIVATE_FILE_MODE))
        {
            panic!("could not set oversized fixture permissions: {error}");
        }

        assert_matches!(
            store.load().err(),
            Some(ManagedDatabaseError::CredentialTooLarge)
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn rejects_macos_extended_acl_even_when_mode_bits_are_private() {
        let directory = match tempfile::tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create test directory: {error}"),
        };
        let store = credential_store(directory.path());
        if let Err(error) = store.load_or_create() {
            panic!("could not create credentials: {error}");
        }
        let acl = std::process::Command::new("/bin/chmod")
            .arg("+a")
            .arg("everyone allow read")
            .arg(store.path())
            .output();
        let acl = match acl {
            Ok(output) => output,
            Err(error) => panic!("could not run ACL fixture command: {error}"),
        };
        assert!(acl.status.success());

        assert_matches!(
            store.load().err(),
            Some(ManagedDatabaseError::CredentialExtendedAcl)
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn acl_listing_parser_rejects_extended_access_and_malformed_output() {
        assert_eq!(
            validate_acl_listing(b"-rw------- 1 owner group 64 Jul 30 12:00 credential"),
            Ok(())
        );
        assert_eq!(
            validate_acl_listing(b"-rw-------+ 1 owner group 64 Jul 30 12:00 credential"),
            Err(ManagedDatabaseError::CredentialExtendedAcl)
        );
        for malformed in [
            &b""[..],
            &b"garbage 1 owner group 64 Jul 30 12:00 credential"[..],
            &b"-rw------ 1 owner group 64 Jul 30 12:00 credential"[..],
            &b"-zw------- 1 owner group 64 Jul 30 12:00 credential"[..],
            &b"-rw-------? 1 owner group 64 Jul 30 12:00 credential"[..],
            &b"-rw-------@@ 1 owner group 64 Jul 30 12:00 credential"[..],
            &[0xff, b' ', b'x'][..],
        ] {
            assert_eq!(
                validate_acl_listing(malformed),
                Err(ManagedDatabaseError::CredentialAclUnsupported)
            );
        }
        #[cfg(target_os = "linux")]
        assert_eq!(
            validate_acl_listing(b"-rw-------. 1 owner group 64 Jul 30 12:00 credential"),
            Ok(())
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            validate_acl_listing(b"-rw-------@ 1 owner group 64 Jul 30 12:00 credential"),
            Ok(())
        );
    }

    #[test]
    fn rejects_multiline_short_and_non_hex_password_values() {
        for contents in [
            "012345678901234567890123\nsecond-line\n",
            "short\n",
            "0123456789012345678901234567890!\n",
        ] {
            assert_matches!(
                DatabaseCredentials::parse(contents).err(),
                Some(ManagedDatabaseError::CredentialFormat)
            );
        }
    }

    #[cfg(unix)]
    fn create_fifo(path: &Path) {
        let fifo = std::process::Command::new("mkfifo").arg(path).output();
        let fifo = match fifo {
            Ok(output) => output,
            Err(error) => panic!("could not run FIFO fixture command: {error}"),
        };
        assert!(fifo.status.success());
    }
}
