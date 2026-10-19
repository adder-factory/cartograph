use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use cartograph_domain::ContentDigest;

use super::{ProjectStore, Result, RocksError, StorageRequest, StorePolicy};

#[cfg(test)]
mod tests;

#[derive(Default)]
pub(super) struct Projects {
    state: Mutex<State>,
    opening: Mutex<()>,
}

#[derive(Default)]
struct State {
    closed: bool,
    entries: BTreeMap<String, Entry>,
}

struct Entry {
    root: PathBuf,
    store: ProjectStore,
    policy: StorePolicy,
}

#[derive(Clone)]
pub(super) struct ProjectAccess {
    identity: ContentDigest,
    projects: Arc<Projects>,
}

impl Projects {
    pub(super) fn open(
        self: &Arc<Self>,
        root: &Path,
        identity: ContentDigest,
        policy: StorePolicy,
        control: &StorageRequest,
    ) -> Result<ProjectAccess> {
        self.open_with(
            root,
            identity,
            policy,
            control,
            |root, identity, policy, control| {
                ProjectStore::open_controlled(root, identity, policy, Some(control.clone()))
            },
        )
    }

    pub(super) fn open_with(
        self: &Arc<Self>,
        root: &Path,
        identity: ContentDigest,
        policy: StorePolicy,
        control: &StorageRequest,
        open_store: impl FnOnce(
            &Path,
            &ContentDigest,
            StorePolicy,
            &StorageRequest,
        ) -> Result<ProjectStore>,
    ) -> Result<ProjectAccess> {
        let policy = policy.validate()?;
        let root = root.canonicalize().map_err(|_| RocksError::Directory)?;
        control.check()?;
        if !self.already_open(&root, &identity, policy)? {
            // Serialize primary creation without holding the registry lock:
            // existing projects remain usable throughout slow native I/O.
            let _opening = self
                .opening
                .lock()
                .map_err(|_| super::super::operation("project_open"))?;
            control.check()?;
            if !self.already_open(&root, &identity, policy)? {
                let store = open_store(&root, &identity, policy, control)?;
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| super::super::operation("project_registry"))?;
                if state.closed {
                    // Shutdown may finish the registry while native open is
                    // running. This unregistered primary drops on this worker.
                    return Err(RocksError::Closed);
                }
                state.entries.insert(
                    identity.as_str().to_owned(),
                    Entry {
                        root,
                        store,
                        policy,
                    },
                );
            }
        }
        Ok(ProjectAccess {
            identity,
            projects: Arc::clone(self),
        })
    }

    fn already_open(
        &self,
        root: &Path,
        identity: &ContentDigest,
        policy: StorePolicy,
    ) -> Result<bool> {
        let state = self
            .state
            .lock()
            .map_err(|_| super::super::operation("project_registry"))?;
        if state.closed {
            return Err(RocksError::Closed);
        }
        if let Some(entry) = state.entries.get(identity.as_str()) {
            if entry.root != root {
                return Err(RocksError::ProjectMismatch);
            }
            if entry.policy != policy {
                return Err(RocksError::Limit {
                    resource: "project_policy_conflict",
                });
            }
            return Ok(true);
        }
        if state.entries.len() >= 64 {
            return Err(RocksError::Limit {
                resource: "owned_projects",
            });
        }
        Ok(false)
    }

    pub(super) fn close(&self) {
        let entries = self.state.lock().ok().map(|mut state| {
            state.closed = true;
            std::mem::take(&mut state.entries)
        });
        // This runs on a storage thread. Clones held by active jobs are also
        // dropped on storage threads, so native DB destruction never runs on
        // the async caller that drops a project handle.
        drop(entries);
    }
}

impl ProjectAccess {
    pub(super) fn run<T>(
        &self,
        control: &StorageRequest,
        operation: impl FnOnce(&ProjectStore) -> Result<T>,
    ) -> Result<T> {
        let store = {
            let state = self
                .projects
                .state
                .lock()
                .map_err(|_| super::super::operation("project_registry"))?;
            if state.closed {
                return Err(RocksError::Closed);
            }
            state
                .entries
                .get(self.identity.as_str())
                .ok_or(RocksError::Closed)?
                .store
                .clone()
        };
        operation(&store.with_control(control.clone()))
    }
}
