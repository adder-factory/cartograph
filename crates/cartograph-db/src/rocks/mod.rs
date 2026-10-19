//! Project-owned `RocksDB` persistence and immutable graph revisions.
//!
//! One writable handle owns a project directory. CLI and MCP adapters must
//! share that owner rather than independently opening writable handles.
//! Blocking operations belong on the runtime's supervised storage worker.

/// Exact native storage engine version reviewed with the Rust adapter.
pub const ENGINE_VERSION: &str = "11.8.1";

mod engine;
mod graph;
mod key;
mod policy;
mod runtime;
#[cfg(test)]
pub(crate) use runtime::StateTestGate;

pub use engine::{
    NativeStorageUsage, ProjectStore, PruneReport, RecordChange, RecordPage, RevisionSnapshot,
    RevisionState, RevisionWriter, StateNamespace, StateTransaction, StoredRevision,
};
pub use graph::{
    GraphBase, GraphCursor, GraphInfo, GraphLink, GraphNeighborPage, GraphPolicy, GraphPublication,
    GraphPublicationError, GraphPublicationInput, GraphRetention, GraphRevision, GraphSnapshot,
    GraphWriteReport, LexicalPosting, LexicalStatistics,
};
pub use key::{Collection, RecordKey};
pub use policy::{ReadBudget, StorePolicy};
pub use runtime::{
    AsyncProjectStore, AsyncRevisionWriter, StorageRequest, StorageRuntime, WorkerPolicy,
};

/// Persistence failures deliberately omit native error text and local paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RocksError {
    /// A validated operation references missing or inconsistent caller input.
    #[error("invalid native storage {field}")]
    InvalidInput {
        /// Stable input label, never its caller-supplied value.
        field: &'static str,
    },
    /// A resource or input exceeded its admitted bound.
    #[error("RocksDB request exceeded its {resource} bound")]
    Limit {
        /// Stable resource identifier.
        resource: &'static str,
    },
    /// A native storage operation failed.
    #[error("RocksDB operation failed during {operation}")]
    Operation {
        /// Stable operation identifier.
        operation: &'static str,
    },
    /// A path did not satisfy the project-owned directory contract.
    #[error("project storage directory is unavailable or unsafe")]
    Directory,
    /// A copied directory belongs to a different project identity.
    #[error("RocksDB directory belongs to a different project")]
    ProjectMismatch,
    /// A newer binary owns an incompatible durable format.
    #[error("RocksDB storage format is incompatible with this executable")]
    Format,
    /// Stored bytes violated their typed contract.
    #[error("RocksDB stored value violates its {field} contract")]
    Corrupt {
        /// Stable field identifier.
        field: &'static str,
    },
    /// Another native primary or mutation still owns this project's writer.
    #[error("another project storage owner or writer is active")]
    WriterBusy,
    /// A token expired or a newer acquisition replaced it.
    #[error("RocksDB mutation lost its exact writer fence")]
    FenceLost,
    /// An expected immutable generation is no longer current.
    #[error("project current generation changed")]
    GenerationChanged,
    /// A requested generation does not exist or is not readable.
    #[error("requested RocksDB generation is unavailable")]
    GenerationUnavailable,
    /// A lifecycle operation was attempted in the wrong state.
    #[error("RocksDB generation cannot perform the requested transition")]
    Transition,
    /// Replaying an admitted staging row changed its contents.
    #[error("RocksDB staging batch conflicts with durable contents")]
    ReplayConflict,
    /// The caller cancelled or abandoned its queued/cooperative operation.
    #[error("RocksDB request was cancelled")]
    Cancelled,
    /// The owner is draining its storage workers and rejects new requests.
    #[error("RocksDB storage owner is closing")]
    Closed,
}

type Result<T> = std::result::Result<T, RocksError>;

fn operation(operation: &'static str) -> RocksError {
    RocksError::Operation { operation }
}
