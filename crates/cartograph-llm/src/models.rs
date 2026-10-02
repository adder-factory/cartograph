use std::{
    fs::File,
    io::Read as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures_util::{StreamExt as _, stream};
use reqwest::{StatusCode, redirect};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::io::AsyncWriteExt as _;

const MAXIMUM_INSTALL_CONCURRENCY: u16 = 4;
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_mins(1);
const MAXIMUM_REDIRECTS: usize = 5;
const HASH_BUFFER_BYTES: usize = 1024 * 1024;
const LOWER_HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
const HIGH_NIBBLE_SHIFT: u8 = 4;
const LOW_NIBBLE_MASK: u8 = 0x0f;

#[derive(Clone, Copy)]
struct RecommendedModel {
    filename: &'static str,
    url: &'static str,
    size_bytes: u64,
    sha256: &'static str,
    minimal: bool,
}

const RECOMMENDED_MODELS: &[RecommendedModel] = &[
    RecommendedModel {
        filename: "jina-embeddings-v2-base-code.Q4_K_M.gguf",
        url: "https://huggingface.co/second-state/jina-embeddings-v2-base-code-GGUF/resolve/main/jina-embeddings-v2-base-code-Q4_K_M.gguf",
        size_bytes: 109_451_680,
        sha256: "cc1b9c936e806c5b3a4fa437903cd538ff9839e948870cb44be49825fe00fee1",
        minimal: true,
    },
    RecommendedModel {
        filename: "qwen2.5-coder-3b-instruct-q4_k_m.gguf",
        url: "https://huggingface.co/Qwen/Qwen2.5-Coder-3B-Instruct-GGUF/resolve/main/qwen2.5-coder-3b-instruct-q4_k_m.gguf",
        size_bytes: 2_104_932_800,
        sha256: "724fb256bec1ff062b2f65e4569e871ad2e95ab2a3989723d1769c54294730b7",
        minimal: true,
    },
    RecommendedModel {
        filename: "bge-reranker-v2-m3-Q4_K_M.gguf",
        url: "https://huggingface.co/gpustack/bge-reranker-v2-m3-GGUF/resolve/main/bge-reranker-v2-m3-Q4_K_M.gguf",
        size_bytes: 438_376_864,
        sha256: "e186a244ed455b4ab66ec64339ce7427a6ae13f5c0b5e544de96e50f0f8b3673",
        minimal: false,
    },
    RecommendedModel {
        filename: "qwen2.5-coder-7b-instruct-q4_k_m.gguf",
        url: "https://huggingface.co/Qwen/Qwen2.5-Coder-7B-Instruct-GGUF/resolve/main/qwen2.5-coder-7b-instruct-q4_k_m.gguf",
        size_bytes: 4_683_073_536,
        sha256: "509287f78cb4d4cf6b3843734733b914b2c158e43e22a7f4bf5e963800894d3c",
        minimal: false,
    },
];

/// Validated model-install policy.
#[derive(Clone, Debug)]
pub struct InstallModelsOptions {
    directory: PathBuf,
    minimal: bool,
    concurrency: u16,
}

impl InstallModelsOptions {
    /// Creates a validated model-install policy.
    ///
    /// # Errors
    ///
    /// Returns an error if the destination path is empty or `concurrency` is
    /// zero or above the bounded downloader maximum.
    pub fn new(
        directory: impl Into<PathBuf>,
        minimal: bool,
        concurrency: u16,
    ) -> Result<Self, InstallModelsError> {
        let directory = directory.into();
        if directory.as_os_str().is_empty()
            || concurrency == 0
            || concurrency > MAXIMUM_INSTALL_CONCURRENCY
        {
            return Err(InstallModelsError::InvalidOptions);
        }
        Ok(Self {
            directory,
            minimal,
            concurrency,
        })
    }
}

/// One checksum-verified model file, without exposing its absolute path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledModel {
    /// Filename for this record.
    pub filename: String,
    /// Verified on-disk model size in bytes.
    pub size_bytes: u64,
}

/// Idempotent bounded installer result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallModelsReport {
    /// Models downloaded and checksum-verified by this invocation.
    pub downloaded: Vec<InstalledModel>,
    /// Existing models whose size and checksum already matched.
    pub skipped_verified: Vec<InstalledModel>,
    /// Total response-body bytes persisted by this invocation.
    pub bytes_downloaded: u64,
    /// Whether only the minimal recommended model set is selected.
    pub minimal: bool,
    /// Maximum number of downloads executed concurrently.
    pub concurrency: u16,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
/// Errors produced while processing install models.
pub enum InstallModelsError {
    #[error("Cartograph model install options are invalid")]
    /// Supplied options violate a documented bound or invariant.
    InvalidOptions,
    #[error("Cartograph model directory is unavailable")]
    /// The target directory could not be created or opened safely.
    DirectoryUnavailable,
    #[error("Cartograph model target is not a safe regular file")]
    /// The target is not a safe regular file owned by this operation.
    UnsafeTarget,
    #[error("Cartograph model download endpoint is unavailable")]
    /// The configured endpoint could not complete the bounded request.
    EndpointUnavailable,
    #[error("Cartograph model download endpoint rejected the request")]
    /// The configured backend rejected the bounded request.
    BackendRejected,
    #[error("Cartograph model download was idle for too long")]
    /// The response body made no progress before the idle deadline.
    IdleTimeout,
    #[error("Cartograph model download size does not match the signed manifest")]
    /// Downloaded bytes do not match the pinned model size.
    SizeMismatch,
    #[error("Cartograph model checksum does not match the signed manifest")]
    /// Downloaded bytes do not match the pinned SHA-256 digest.
    ChecksumMismatch,
    #[error("Cartograph model file could not be written atomically")]
    /// The bounded output could not be written atomically.
    WriteFailed,
}

enum InstallDisposition {
    Downloaded(InstalledModel),
    Skipped(InstalledModel),
}

/// Download the curated v1.1.33-compatible GGUF set with stronger checksum,
/// redirect, idle, and atomic-publication guarantees.
/// # Errors
///
/// Returns an error if destination/TLS/client setup fails or a bounded download
/// violates HTTPS redirects, size, idle timeout, checksum, or atomic-file safety.
pub async fn install_recommended_models(
    options: InstallModelsOptions,
) -> Result<InstallModelsReport, InstallModelsError> {
    prepare_directory(&options.directory).await?;
    crate::ensure_tls_crypto_provider().map_err(|_| InstallModelsError::EndpointUnavailable)?;
    let policy = redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAXIMUM_REDIRECTS || attempt.url().scheme() != "https" {
            attempt.stop()
        } else {
            attempt.follow()
        }
    });
    let client = reqwest::Client::builder()
        .connect_timeout(DOWNLOAD_CONNECT_TIMEOUT)
        .redirect(policy)
        .build()
        .map_err(|_| InstallModelsError::EndpointUnavailable)?;
    let models = RECOMMENDED_MODELS
        .iter()
        .copied()
        .filter(|model| !options.minimal || model.minimal)
        .collect::<Vec<_>>();
    let directory = Arc::new(options.directory.clone());
    let results = stream::iter(models)
        .map(|model| {
            let client = client.clone();
            let directory = directory.clone();
            async move { install_one(&client, directory.as_ref(), model).await }
        })
        .buffer_unordered(usize::from(options.concurrency))
        .collect::<Vec<_>>()
        .await;
    let mut downloaded = Vec::new();
    let mut skipped_verified = Vec::new();
    let mut bytes_downloaded = 0_u64;
    for result in results {
        match result? {
            InstallDisposition::Downloaded(model) => {
                bytes_downloaded = bytes_downloaded
                    .checked_add(model.size_bytes)
                    .ok_or(InstallModelsError::SizeMismatch)?;
                downloaded.push(model);
            }
            InstallDisposition::Skipped(model) => skipped_verified.push(model),
        }
    }
    downloaded.sort_by(|left, right| left.filename.cmp(&right.filename));
    skipped_verified.sort_by(|left, right| left.filename.cmp(&right.filename));
    Ok(InstallModelsReport {
        downloaded,
        skipped_verified,
        bytes_downloaded,
        minimal: options.minimal,
        concurrency: options.concurrency,
    })
}

async fn prepare_directory(directory: &Path) -> Result<(), InstallModelsError> {
    match tokio::fs::symlink_metadata(directory).await {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(directory)
                .await
                .map_err(|_| InstallModelsError::DirectoryUnavailable)?;
            let metadata = tokio::fs::symlink_metadata(directory)
                .await
                .map_err(|_| InstallModelsError::DirectoryUnavailable)?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                Ok(())
            } else {
                Err(InstallModelsError::DirectoryUnavailable)
            }
        }
        Ok(_) | Err(_) => Err(InstallModelsError::DirectoryUnavailable),
    }
}

async fn install_one(
    client: &reqwest::Client,
    directory: &Path,
    model: RecommendedModel,
) -> Result<InstallDisposition, InstallModelsError> {
    let target = directory.join(model.filename);
    let partial = directory.join(format!("{}.partial", model.filename));
    reject_symlink(&target).await?;
    reject_symlink(&partial).await?;
    if tokio::fs::metadata(&target).await.is_ok() {
        if verify_file(target.clone(), model).await? {
            return Ok(InstallDisposition::Skipped(installed(model)));
        }
        tokio::fs::remove_file(&target)
            .await
            .map_err(|_| InstallModelsError::WriteFailed)?;
    }
    if tokio::fs::metadata(&partial).await.is_ok() {
        tokio::fs::remove_file(&partial)
            .await
            .map_err(|_| InstallModelsError::WriteFailed)?;
    }
    let result = download_to_partial(client, &partial, model).await;
    if let Err(error) = result {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(error);
    }
    tokio::fs::rename(&partial, &target)
        .await
        .map_err(|_| InstallModelsError::WriteFailed)?;
    Ok(InstallDisposition::Downloaded(installed(model)))
}

async fn reject_symlink(path: &Path) -> Result<(), InstallModelsError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(InstallModelsError::UnsafeTarget)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(InstallModelsError::UnsafeTarget),
    }
}

async fn verify_file(path: PathBuf, model: RecommendedModel) -> Result<bool, InstallModelsError> {
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|_| InstallModelsError::UnsafeTarget)?;
    if !metadata.is_file() || metadata.len() != model.size_bytes {
        return Ok(false);
    }
    tokio::task::spawn_blocking(move || hash_file(&path))
        .await
        .map_err(|_| InstallModelsError::WriteFailed)?
        .map(|digest| digest == model.sha256)
}

fn hash_file(path: &Path) -> Result<String, InstallModelsError> {
    let mut file = File::open(path).map_err(|_| InstallModelsError::WriteFailed)?;
    let mut buffer = vec![0_u8; HASH_BUFFER_BYTES];
    let mut digest = Sha256::new();
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| InstallModelsError::WriteFailed)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex_digest(&digest.finalize()))
}

async fn download_to_partial(
    client: &reqwest::Client,
    partial: &Path,
    model: RecommendedModel,
) -> Result<(), InstallModelsError> {
    let response = client
        .get(model.url)
        .send()
        .await
        .map_err(|_| InstallModelsError::EndpointUnavailable)?;
    if response.status() != StatusCode::OK {
        return Err(InstallModelsError::BackendRejected);
    }
    if response
        .content_length()
        .is_some_and(|length| length != model.size_bytes)
    {
        return Err(InstallModelsError::SizeMismatch);
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    let mut output = options
        .open(partial)
        .await
        .map_err(|_| InstallModelsError::WriteFailed)?;
    #[cfg(unix)]
    set_private_permissions(&output).await?;
    let mut stream = response.bytes_stream();
    let mut downloaded = 0_u64;
    let mut digest = Sha256::new();
    loop {
        let next = tokio::time::timeout(DOWNLOAD_IDLE_TIMEOUT, stream.next())
            .await
            .map_err(|_| InstallModelsError::IdleTimeout)?;
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|_| InstallModelsError::EndpointUnavailable)?;
        downloaded = downloaded
            .checked_add(u64::try_from(chunk.len()).map_err(|_| InstallModelsError::SizeMismatch)?)
            .ok_or(InstallModelsError::SizeMismatch)?;
        if downloaded > model.size_bytes {
            return Err(InstallModelsError::SizeMismatch);
        }
        digest.update(&chunk);
        output
            .write_all(&chunk)
            .await
            .map_err(|_| InstallModelsError::WriteFailed)?;
    }
    if downloaded != model.size_bytes {
        return Err(InstallModelsError::SizeMismatch);
    }
    if hex_digest(&digest.finalize()) != model.sha256 {
        return Err(InstallModelsError::ChecksumMismatch);
    }
    output
        .sync_all()
        .await
        .map_err(|_| InstallModelsError::WriteFailed)
}

#[cfg(unix)]
async fn set_private_permissions(file: &tokio::fs::File) -> Result<(), InstallModelsError> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(|_| InstallModelsError::WriteFailed)
}

fn installed(model: RecommendedModel) -> InstalledModel {
    InstalledModel {
        filename: model.filename.to_owned(),
        size_bytes: model.size_bytes,
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(
            LOWER_HEX_DIGITS[usize::from(byte >> HIGH_NIBBLE_SHIFT)],
        ));
        output.push(char::from(
            LOWER_HEX_DIGITS[usize::from(byte & LOW_NIBBLE_MASK)],
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::{io::Write as _, net::TcpListener, thread, time::Duration};

    use super::*;

    const FIXTURE_REQUEST_BYTES: usize = 64 * 1_024;
    const FIXTURE_REQUEST_CHUNK_BYTES: usize = 4 * 1_024;
    const HTTP_HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";

    fn test_client() -> reqwest::Client {
        assert_eq!(crate::ensure_tls_crypto_provider(), Ok(()));
        reqwest::Client::new()
    }

    #[test]
    fn manifest_is_https_unique_and_has_frozen_sha256_values() {
        let mut filenames = RECOMMENDED_MODELS
            .iter()
            .map(|model| model.filename)
            .collect::<Vec<_>>();
        filenames.sort_unstable();
        filenames.dedup();
        assert_eq!(filenames.len(), RECOMMENDED_MODELS.len());
        assert!(RECOMMENDED_MODELS.iter().all(|model| {
            model.url.starts_with("https://")
                && model.size_bytes > 0
                && model.sha256.len() == 64
                && model.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        }));
        assert_eq!(
            RECOMMENDED_MODELS
                .iter()
                .filter(|model| model.minimal)
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn unsafe_model_targets_are_rejected_before_network_work() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("model tempdir failed: {error}"));
        let target = directory.path().join(RECOMMENDED_MODELS[0].filename);
        tokio::fs::create_dir(&target)
            .await
            .unwrap_or_else(|error| panic!("unsafe target fixture failed: {error}"));
        let client = test_client();
        assert_matches!(
            install_one(&client, directory.path(), RECOMMENDED_MODELS[0])
                .await
                .err(),
            Some(InstallModelsError::UnsafeTarget)
        );
    }

    #[tokio::test]
    async fn installer_downloads_atomically_then_reuses_only_the_verified_file() {
        let body = b"fixture model bytes";
        let checksum = hex_digest(&Sha256::digest(body));
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        );
        let (endpoint, request) = spawn_http_fixture(response);
        let endpoint = Box::leak(endpoint.into_boxed_str());
        let checksum = Box::leak(checksum.into_boxed_str());
        let model = RecommendedModel {
            filename: "fixture.gguf",
            url: endpoint,
            size_bytes: u64::try_from(body.len()).unwrap_or_default(),
            sha256: checksum,
            minimal: true,
        };
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("model directory fixture failed: {error}"));
        let client = test_client();

        let downloaded = install_one(&client, directory.path(), model)
            .await
            .unwrap_or_else(|error| panic!("model download failed: {error}"));
        let InstallDisposition::Downloaded(downloaded) = downloaded else {
            panic!("first model installation did not download")
        };
        assert_eq!(downloaded.filename, model.filename);
        assert_eq!(downloaded.size_bytes, model.size_bytes);
        assert_eq!(
            tokio::fs::read(directory.path().join(model.filename))
                .await
                .unwrap_or_else(|error| panic!("model read failed: {error}")),
            body
        );
        assert!(!directory.path().join("fixture.gguf.partial").exists());
        let request = request
            .join()
            .unwrap_or_else(|_| panic!("model HTTP fixture panicked"));
        assert!(request.starts_with(b"GET /model.gguf HTTP/1.1\r\n"));

        let skipped = install_one(&client, directory.path(), model)
            .await
            .unwrap_or_else(|error| panic!("verified model reuse failed: {error}"));
        let InstallDisposition::Skipped(skipped) = skipped else {
            panic!("verified model was downloaded again")
        };
        assert_eq!(skipped, downloaded);
    }

    #[tokio::test]
    async fn directory_options_and_download_failures_are_bounded_before_publication() {
        assert_matches!(
            InstallModelsOptions::new("", true, 1),
            Err(InstallModelsError::InvalidOptions)
        );
        assert_matches!(
            InstallModelsOptions::new("models", true, 0),
            Err(InstallModelsError::InvalidOptions)
        );
        assert_matches!(
            InstallModelsOptions::new("models", false, MAXIMUM_INSTALL_CONCURRENCY + 1),
            Err(InstallModelsError::InvalidOptions)
        );
        let root =
            tempfile::tempdir().unwrap_or_else(|error| panic!("directory fixture failed: {error}"));
        let created = root.path().join("created/models");
        prepare_directory(&created)
            .await
            .unwrap_or_else(|error| panic!("model directory creation failed: {error}"));
        prepare_directory(&created)
            .await
            .unwrap_or_else(|error| panic!("existing model directory failed: {error}"));
        let regular_file = root.path().join("not-a-directory");
        tokio::fs::write(&regular_file, b"fixture")
            .await
            .unwrap_or_else(|error| panic!("directory rejection fixture failed: {error}"));
        assert_eq!(
            prepare_directory(&regular_file).await,
            Err(InstallModelsError::DirectoryUnavailable)
        );

        for (status, declared_length, expected) in [
            (503_u16, 0_usize, InstallModelsError::BackendRejected),
            (200_u16, 99_usize, InstallModelsError::SizeMismatch),
        ] {
            let response = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n"
            );
            let (endpoint, request) = spawn_http_fixture(response);
            let endpoint = Box::leak(endpoint.into_boxed_str());
            let model = RecommendedModel {
                filename: "rejected.gguf",
                url: endpoint,
                size_bytes: 3,
                sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                minimal: true,
            };
            assert_eq!(
                download_to_partial(
                    &test_client(),
                    &root.path().join("rejected.gguf.partial"),
                    model,
                )
                .await,
                Err(expected)
            );
            request
                .join()
                .unwrap_or_else(|_| panic!("rejected model HTTP fixture panicked"));
        }

        let body = "bad";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (endpoint, request) = spawn_http_fixture(response);
        let endpoint = Box::leak(endpoint.into_boxed_str());
        let model = RecommendedModel {
            filename: "checksum.gguf",
            url: endpoint,
            size_bytes: u64::try_from(body.len()).unwrap_or_default(),
            sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            minimal: true,
        };
        let partial = root.path().join("checksum.gguf.partial");
        assert_eq!(
            download_to_partial(&test_client(), &partial, model).await,
            Err(InstallModelsError::ChecksumMismatch)
        );
        assert!(partial.exists());
        request
            .join()
            .unwrap_or_else(|_| panic!("checksum HTTP fixture panicked"));
    }

    fn spawn_http_fixture(response: String) -> (String, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .unwrap_or_else(|error| panic!("model fixture bind failed: {error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("model fixture address failed: {error}"));
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .unwrap_or_else(|error| panic!("model fixture accept failed: {error}"));
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap_or_else(|error| panic!("model fixture timeout failed: {error}"));
            let request = read_http_request(&mut stream);
            stream
                .write_all(response.as_bytes())
                .and_then(|()| stream.flush())
                .unwrap_or_else(|error| panic!("model fixture response failed: {error}"));
            request
        });
        (format!("http://{address}/model.gguf"), handle)
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut chunk = [0_u8; FIXTURE_REQUEST_CHUNK_BYTES];
        loop {
            let read = stream
                .read(&mut chunk)
                .unwrap_or_else(|error| panic!("model fixture read failed: {error}"));
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            assert!(request.len() <= FIXTURE_REQUEST_BYTES);
            if request
                .windows(HTTP_HEADER_TERMINATOR.len())
                .any(|window| window == HTTP_HEADER_TERMINATOR)
            {
                break;
            }
        }
        request
    }
}
