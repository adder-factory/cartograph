use std::time::{Duration, Instant};

use super::{Result, RocksError};

const MIB: usize = 1_048_576;

/// Resource policy shared by every column family in one project database.
///
/// Cache and memtable budgets are soft engine limits. Iterators, request
/// buffers, native metadata, and the filesystem cache require additional RAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorePolicy {
    /// Shared block-cache capacity, including charged memtable allocations.
    pub cache_bytes: usize,
    /// Shared write-buffer budget across all column families.
    pub memtable_bytes: usize,
    /// Maximum encoded logical bytes in one foreground atomic write.
    pub maximum_batch_bytes: usize,
    /// Maximum number of changes in one foreground batch.
    pub maximum_batch_rows: usize,
}

impl Default for StorePolicy {
    fn default() -> Self {
        Self {
            cache_bytes: 64 * MIB,
            memtable_bytes: 32 * MIB,
            maximum_batch_bytes: 64 * MIB,
            maximum_batch_rows: 100_000,
        }
    }
}

impl StorePolicy {
    pub(super) fn validate(self) -> Result<Self> {
        if !(8 * MIB..=MIB * 4_096).contains(&self.cache_bytes)
            || !(4 * MIB..=self.cache_bytes).contains(&self.memtable_bytes)
            || !(MIB..=64 * MIB).contains(&self.maximum_batch_bytes)
            || !(1..=100_000).contains(&self.maximum_batch_rows)
        {
            return Err(RocksError::Limit { resource: "policy" });
        }
        Ok(self)
    }
}

/// A consumable read allowance covering physical records, bytes, and time.
///
/// Callers reuse the same allowance across every storage lookup in a request.
pub struct ReadBudget {
    remaining_rows: usize,
    remaining_bytes: usize,
    workspace_bytes: usize,
    deadline: Instant,
    control: Option<super::StorageRequest>,
}

impl ReadBudget {
    /// Admit a finite request. No zero/unbounded deadline is accepted.
    ///
    /// # Errors
    /// Returns a limit error for invalid rows, bytes, or deadlines.
    pub fn new(rows: usize, bytes: usize, timeout: Duration) -> Result<Self> {
        if rows == 0 || bytes == 0 || timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(RocksError::Limit {
                resource: "read_policy",
            });
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RocksError::Limit {
                resource: "deadline",
            })?;
        Ok(Self {
            remaining_rows: rows,
            remaining_bytes: bytes,
            workspace_bytes: bytes,
            deadline,
            control: None,
        })
    }

    /// Streaming operators retain a separately bounded workspace while charging
    /// every physical read, including repeated reads, against a finite total.
    /// Only audited native operators may use this admission contract.
    pub(crate) fn streaming(
        rows: usize,
        bytes: usize,
        workspace: usize,
        timeout: Duration,
    ) -> Result<Self> {
        if rows > 2_000_000 || bytes > 8 * 1024 * MIB || !(MIB..=128 * MIB).contains(&workspace) {
            return Err(RocksError::Limit {
                resource: "streaming_read_policy",
            });
        }
        let mut budget = Self::new(rows, bytes, timeout)?;
        budget.workspace_bytes = workspace;
        Ok(budget)
    }

    pub(crate) fn with_control(mut self, control: super::StorageRequest) -> Self {
        self.control = Some(control);
        self
    }

    pub(super) fn reservation_bytes(&self) -> Result<u32> {
        self.workspace_bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(super::engine::MAXIMUM_RECORD_BYTES))
            .and_then(|bytes| bytes.checked_add(4_096))
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or(RocksError::Limit {
                resource: "worker_bytes",
            })
    }

    /// Check cancellation and time during CPU work without consuming a row.
    /// # Errors
    /// Returns cancellation, closure, or deadline errors.
    pub fn checkpoint(&self) -> Result<()> {
        if let Some(control) = &self.control {
            control.check()?;
        }
        if Instant::now() >= self.deadline {
            return Err(RocksError::Limit {
                resource: "read_deadline",
            });
        }
        Ok(())
    }

    pub(super) fn charge(&mut self, bytes: usize) -> Result<()> {
        self.checkpoint()?;
        self.remaining_rows = self
            .remaining_rows
            .checked_sub(1)
            .ok_or(RocksError::Limit {
                resource: "read_rows",
            })?;
        self.remaining_bytes =
            self.remaining_bytes
                .checked_sub(bytes)
                .ok_or(RocksError::Limit {
                    resource: "read_bytes",
                })?;
        Ok(())
    }
}
