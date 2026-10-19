//! Source-summary roll-ups share immutable read snapshots and atomic save fences.

use super::summaries::current_symbol_summary;
use super::{
    Kind, Record, Scope, State, WRITE_BYTES, WRITE_ROWS, corrupt, read_scope, workspace, write,
};
use crate::{
    FileInput, NativeDatabase, NewAgentArtifact, StorageError, SymbolInput,
    artifacts::{
        FileSummarySaveRequest, ModuleSummaryRollupDigestInput, ModuleSummaryRollupItem,
        ModuleSummarySaveRequest, PendingFileSummary, PendingModuleSummary,
        PendingSummaryRollupQuery, SummaryRollupDigestInput, SummaryRollupItem, SummarySaveFields,
        immediate_parent_directory, module_summary_rollup_digest, summary_rollup_digest,
        validate_limit, validate_model,
    },
    native_database::timestamp_order,
    retrieval::native::{retain, visit_files, visit_symbols},
    rocks::{GraphSnapshot, ReadBudget, RocksError, StateTransaction},
};
use cartograph_domain::{ContentDigest, GenerationId, ProjectId, Visibility};
use std::{cmp::Ordering, collections::BTreeMap};

#[derive(Default)]
struct Group {
    count: u64,
    tools: u64,
    file_hash: String,
    latest: String,
    items: Vec<Item>,
}
struct Item {
    symbol: SymbolInput,
    path: String,
    summary: String,
}
struct Request {
    modules: bool,
    model: String,
    anchor: ContentDigest,
    after: Option<String>,
    limit: usize,
    item_limit: usize,
}

impl Group {
    fn record(&mut self, summary: Option<&Record>, tool: bool, file: &FileInput, modules: bool) {
        if tool {
            self.tools += 1;
        }
        let Some(summary) = summary else {
            return;
        };
        self.count += 1;
        if self.latest.is_empty() || timestamp_order(&summary.updated_at, &self.latest).is_gt() {
            self.latest.clone_from(&summary.updated_at);
        }
        if !modules {
            self.file_hash = file.content_hash.to_string();
        }
    }

    fn retain_item(
        &mut self,
        symbol: SymbolInput,
        file: &FileInput,
        summary: &Record,
        limit: usize,
        bytes: &mut usize,
    ) -> Result<(), RocksError> {
        let old = self.items.iter().map(item_bytes).sum::<usize>();
        retain(
            &mut self.items,
            Item {
                symbol,
                path: file.normalized_path.clone(),
                summary: summary.body.chars().take(600).collect(),
            },
            limit,
            item_order,
        );
        *bytes = bytes
            .checked_sub(old)
            .and_then(|bytes| bytes.checked_add(self.items.iter().map(item_bytes).sum::<usize>()))
            .ok_or_else(workspace)?;
        if *bytes > 32 * 1_048_576 {
            return Err(workspace());
        }
        Ok(())
    }
}

fn rollup_evidence(
    generation: &GenerationId,
    scope: &str,
    group: Group,
    anchor: &ContentDigest,
    modules: bool,
) -> (String, serde_json::Value) {
    if modules {
        let pending = module_pending(generation, scope.to_owned(), group, anchor);
        (
            pending.content_hash.clone(),
            serde_json::json!({
            "summarizedSymbols":pending.summarized_symbols, "toolExportConstants":pending.tool_export_constants,
            "evidenceItems":pending.items.len(), "itemsTruncated":pending.items_truncated,
            "protocol":"symbol_to_module_v2"}),
        )
    } else {
        let pending = file_pending(generation, scope.to_owned(), group, anchor);
        (
            pending.content_hash.clone(),
            serde_json::json!({
            "fileContentHash":pending.file_content_hash, "summarizedSymbols":pending.summarized_symbols,
            "evidenceItems":pending.items.len(), "itemsTruncated":pending.items_truncated,
            "protocol":"symbol_to_file_v2"}),
        )
    }
}

impl NativeDatabase {
    /// Page files with changed source, summary evidence, model, or prompt anchor.
    /// # Errors
    /// Rejects invalid bounds, corrupt evidence, or exhausted native budgets.
    pub async fn pending_file_summaries(
        &self,
        request: PendingSummaryRollupQuery<'_>,
    ) -> Result<Vec<PendingFileSummary>, StorageError> {
        self.require_project(request.project_id)?;
        let request = Request::new(request, false)?;
        self.state(None, 0, 0, move |transaction, budget| {
            let Some(current) = transaction.current()? else {
                return Ok(Vec::new());
            };
            let snapshot = transaction.graph_snapshot()?;
            let graph = snapshot.graph(budget)?;
            let groups = pending(
                transaction,
                &graph,
                budget,
                &current.generation_id,
                &request,
            )?;
            Ok(groups
                .into_iter()
                .map(|(path, group)| {
                    file_pending(&current.generation_id, path, group, &request.anchor)
                })
                .collect())
        })
        .await
    }

    /// Page immediate source directories whose roll-up evidence is no longer current.
    /// # Errors
    /// Rejects invalid bounds, corrupt evidence, or exhausted native budgets.
    pub async fn pending_module_summaries(
        &self,
        request: PendingSummaryRollupQuery<'_>,
    ) -> Result<Vec<PendingModuleSummary>, StorageError> {
        self.require_project(request.project_id)?;
        let request = Request::new(request, true)?;
        self.state(None, 0, 0, move |transaction, budget| {
            let Some(current) = transaction.current()? else {
                return Ok(Vec::new());
            };
            let snapshot = transaction.graph_snapshot()?;
            let graph = snapshot.graph(budget)?;
            let groups = pending(
                transaction,
                &graph,
                budget,
                &current.generation_id,
                &request,
            )?;
            Ok(groups
                .into_iter()
                .map(|(directory, group)| {
                    module_pending(&current.generation_id, directory, group, &request.anchor)
                })
                .collect())
        })
        .await
    }

    /// Recompute a file's exact evidence and publish its summary atomically.
    /// # Errors
    /// Rejects changed evidence and failed bounded native writes.
    pub async fn save_file_summary(
        &self,
        project: &ProjectId,
        request: FileSummarySaveRequest<'_>,
    ) -> Result<Option<Record>, StorageError> {
        self.save_rollup(project, request.path.as_str(), request.fields, false)
            .await
    }

    /// Recompute one immediate directory's evidence and atomically publish its paragraph.
    /// # Errors
    /// Rejects changed evidence and failed bounded native writes.
    pub async fn save_module_summary(
        &self,
        project: &ProjectId,
        request: ModuleSummarySaveRequest<'_>,
    ) -> Result<Option<Record>, StorageError> {
        self.save_rollup(project, request.directory.as_str(), request.fields, true)
            .await
    }

    /// Recompute file evidence once and atomically publish at most sixteen summaries.
    /// Results retain request order; `None` preserves an existing higher-quality summary.
    /// # Errors
    /// Rejects empty, duplicate, oversized or stale batches without publishing any row.
    pub async fn save_file_summaries(
        &self,
        project: &ProjectId,
        requests: &[FileSummarySaveRequest<'_>],
    ) -> Result<Vec<Option<Record>>, StorageError> {
        self.save_rollup_batch(
            project,
            requests
                .iter()
                .map(|request| (request.path.as_str(), request.fields)),
            false,
        )
        .await
    }

    /// Recompute immediate-directory evidence once and atomically publish at most sixteen summaries.
    /// Results retain request order; `None` preserves an existing higher-quality summary.
    /// # Errors
    /// Rejects empty, duplicate, oversized or stale batches without publishing any row.
    pub async fn save_module_summaries(
        &self,
        project: &ProjectId,
        requests: &[ModuleSummarySaveRequest<'_>],
    ) -> Result<Vec<Option<Record>>, StorageError> {
        self.save_rollup_batch(
            project,
            requests
                .iter()
                .map(|request| (request.directory.as_str(), request.fields)),
            true,
        )
        .await
    }

    async fn save_rollup(
        &self,
        project: &ProjectId,
        scope: &str,
        fields: SummarySaveFields<'_>,
        modules: bool,
    ) -> Result<Option<Record>, StorageError> {
        self.save_rollup_batch(project, [(scope, fields)], modules)
            .await?
            .pop()
            .ok_or(StorageError::InvalidInput {
                field: "rollup_batch",
            })
    }

    async fn save_rollup_batch<'a>(
        &self,
        project: &ProjectId,
        requests: impl IntoIterator<Item = (&'a str, SummarySaveFields<'a>)>,
        modules: bool,
    ) -> Result<Vec<Option<Record>>, StorageError> {
        self.require_project(project)?;
        let requests = owned_saves(requests)?;
        let rows = u32::try_from(requests.len()).map_err(|_| StorageError::InvalidInput {
            field: "rollup_batch",
        })? * WRITE_ROWS;
        self.state(None, WRITE_BYTES, rows, move |transaction, budget| {
            save_batch(transaction, budget, requests, modules)
        })
        .await
    }
}

#[derive(Clone)]
struct OwnedSave {
    scope: String,
    body: String,
    expected: ContentDigest,
    anchor: ContentDigest,
    model: String,
    mode: &'static str,
    item_limit: usize,
}

fn owned_saves<'a>(
    requests: impl IntoIterator<Item = (&'a str, SummarySaveFields<'a>)>,
) -> Result<Vec<OwnedSave>, StorageError> {
    let mut owned = Vec::<OwnedSave>::new();
    let mut bytes = 0_usize;
    for (scope, fields) in requests {
        bytes = bytes
            .checked_add(scope.len() + fields.summary.len() + fields.model.len() + 1024)
            .ok_or(StorageError::InvalidInput {
                field: "rollup_batch",
            })?;
        if owned.len() == 16
            || bytes > WRITE_BYTES as usize
            || owned.iter().any(|item| item.scope == scope)
        {
            return Err(StorageError::InvalidInput {
                field: "rollup_batch",
            });
        }
        owned.push(OwnedSave {
            scope: scope.to_owned(),
            body: fields.summary.to_owned(),
            expected: fields.expected_digest.clone(),
            anchor: fields.anchor_digest.clone(),
            model: fields.model.to_owned(),
            mode: fields.generation_mode,
            item_limit: usize::from(fields.item_limit),
        });
    }
    if owned.is_empty() {
        return Err(StorageError::InvalidInput {
            field: "rollup_batch",
        });
    }
    Ok(owned)
}

fn save_batch(
    transaction: &mut StateTransaction<'_>,
    budget: &mut ReadBudget,
    requests: Vec<OwnedSave>,
    modules: bool,
) -> Result<Vec<Option<Record>>, RocksError> {
    let generation = transaction
        .current()?
        .ok_or(RocksError::GenerationChanged)?
        .generation_id;
    let snapshot = transaction.graph_snapshot()?;
    let graph = snapshot.graph(budget)?;
    let scopes = requests
        .iter()
        .map(|request| request.scope.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let item_limit = requests
        .iter()
        .map(|request| request.item_limit)
        .max()
        .ok_or(corrupt())?;
    let mut groups = inventory(
        transaction,
        &graph,
        budget,
        modules,
        |scope| scopes.contains(scope),
        Some(item_limit),
    )?;
    drop(graph);
    drop(snapshot);
    let artifacts = requests
        .into_iter()
        .map(|request| {
            let mut group = groups
                .remove(&request.scope)
                .filter(|group| group.count >= if modules { 3 } else { 1 })
                .ok_or(RocksError::GenerationChanged)?;
            group.items.truncate(request.item_limit);
            prepare_save(transaction, budget, &generation, request, group, modules)
        })
        .collect::<Result<Vec<_>, _>>()?;
    artifacts
        .into_iter()
        .map(|artifact| {
            artifact
                .map(|artifact| write(transaction, budget, artifact, true))
                .transpose()
        })
        .collect()
}

fn prepare_save(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    generation: &GenerationId,
    request: OwnedSave,
    group: Group,
    modules: bool,
) -> Result<Option<NewAgentArtifact>, RocksError> {
    let (digest, metadata) =
        rollup_evidence(generation, &request.scope, group, &request.anchor, modules);
    if digest != request.expected.as_str() {
        return Err(RocksError::GenerationChanged);
    }
    let artifact_scope = if modules { Scope::Module } else { Scope::File };
    if request.mode == "structural_rule"
        && read_scope(
            transaction,
            Kind::Summary,
            artifact_scope,
            &request.scope,
            budget,
        )?
        .is_some_and(|record| {
            record.source_digest.as_deref() == Some(request.expected.as_str())
                && !record
                    .metadata
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .starts_with("structural:")
        })
    {
        return Ok(None);
    }
    let mut metadata = metadata.as_object().cloned().ok_or(corrupt())?;
    metadata.extend(serde_json::json!({"model":request.model, "anchorDigest":request.anchor.as_str(),
        "rollupDigest":request.expected.as_str(), "generationMode":request.mode, "evidenceLimit":request.item_limit})
        .as_object().cloned().ok_or(corrupt())?);
    Ok(Some(NewAgentArtifact {
        kind: Kind::Summary,
        scope: artifact_scope,
        scope_key: request.scope,
        body: request.body,
        metadata: metadata.into(),
        generation_id: Some(generation.clone()),
        source_digest: Some(request.expected),
        state: State::Complete,
    }))
}

impl Request {
    fn new(input: PendingSummaryRollupQuery<'_>, modules: bool) -> Result<Self, StorageError> {
        validate_model(input.model)?;
        validate_limit(input.limit, super::super::MAX_FILE_SUMMARY_BATCH)?;
        validate_limit(
            input.item_limit,
            if modules {
                super::super::MAX_MODULE_SUMMARY_ROLLUP_ITEMS
            } else {
                super::super::MAX_SUMMARY_ROLLUP_ITEMS
            },
        )?;
        if input
            .after
            .is_some_and(|path| cartograph_domain::NormalizedPath::parse(path).is_err())
        {
            return Err(StorageError::InvalidInput {
                field: "rollup_cursor",
            });
        }
        Ok(Self {
            modules,
            model: input.model.to_owned(),
            anchor: input.anchor_digest.clone(),
            after: input.after.map(str::to_owned),
            limit: usize::from(input.limit),
            item_limit: usize::from(input.item_limit),
        })
    }
}
fn files(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<BTreeMap<cartograph_domain::FileId, FileInput>, RocksError> {
    let mut files = BTreeMap::new();
    let mut bytes = 0_usize;
    visit_files(graph, budget, |file, _| {
        bytes = bytes
            .checked_add(file.normalized_path.len() + file.language.len() + 512)
            .ok_or_else(workspace)?;
        if bytes > 16 * 1_048_576 {
            return Err(workspace());
        }
        files.insert(file.file_id.clone(), file);
        Ok(())
    })?;
    Ok(files)
}
fn group_key(path: &str, modules: bool) -> Option<String> {
    if modules {
        immediate_parent_directory(path)
    } else {
        Some(path.to_owned())
    }
}
fn inventory(
    transaction: &StateTransaction<'_>,
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    modules: bool,
    selected: impl Fn(&str) -> bool,
    item_limit: Option<usize>,
) -> Result<BTreeMap<String, Group>, RocksError> {
    let files = files(graph, budget)?;
    let mut groups = BTreeMap::<String, Group>::new();
    let mut bytes = 0_usize;
    let mut item_bytes = 0_usize;
    visit_symbols(graph, budget, |symbol, budget| {
        let file = files.get(&symbol.file_id).ok_or(corrupt())?;
        let Some(key) = group_key(&file.normalized_path, modules) else {
            return Ok(());
        };
        if !selected(&key) {
            return Ok(());
        }
        let summary = current_symbol_summary(transaction, &symbol, budget)?;
        let tool =
            modules && symbol.symbol_kind == "constant" && symbol.qualified_name.ends_with("_TOOL");
        if summary.is_none() && !tool {
            return Ok(());
        }
        if !groups.contains_key(&key) {
            bytes = bytes.checked_add(key.len() + 512).ok_or_else(workspace)?;
            if bytes > 16 * 1_048_576 {
                return Err(workspace());
            }
        }
        let group = groups.entry(key).or_default();
        group.record(summary.as_ref(), tool, file, modules);
        if let (Some(limit), Some(summary)) = (item_limit, summary) {
            group.retain_item(symbol, file, &summary, limit, &mut item_bytes)?;
        }
        Ok(())
    })?;
    Ok(groups)
}
fn pending(
    transaction: &StateTransaction<'_>,
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    generation: &GenerationId,
    request: &Request,
) -> Result<BTreeMap<String, Group>, RocksError> {
    let mut selected = BTreeMap::new();
    let minimum_count = if request.modules { 3 } else { 1 };
    let scope = if request.modules {
        Scope::Module
    } else {
        Scope::File
    };
    for (key, group) in inventory(transaction, graph, budget, request.modules, |_| true, None)? {
        if group.count < minimum_count || request.after.as_ref().is_some_and(|after| &key <= after)
        {
            continue;
        }
        let cached = read_scope(transaction, Kind::Summary, scope, &key, budget)?;
        if cached
            .as_ref()
            .is_none_or(|cached| stale(cached, &group, generation, request))
        {
            selected.insert(key, group);
            if selected.len() == request.limit {
                break;
            }
        }
    }
    hydrate(
        transaction,
        graph,
        budget,
        &mut selected,
        request.modules,
        request.item_limit,
    )?;
    Ok(selected)
}
fn stale(record: &Record, group: &Group, generation: &GenerationId, request: &Request) -> bool {
    let text = |key| record.metadata.get(key).and_then(serde_json::Value::as_str);
    record.state != State::Complete
        || record.generation_id.as_deref() != Some(generation.as_str())
        || (request.model != "structural:v2" && text("model") != Some(request.model.as_str()))
        || text("anchorDigest") != Some(request.anchor.as_str())
        || text("rollupDigest") != record.source_digest.as_deref()
        || record
            .metadata
            .get("summarizedSymbols")
            .and_then(serde_json::Value::as_u64)
            != Some(group.count)
        || record
            .metadata
            .get("evidenceLimit")
            .and_then(serde_json::Value::as_u64)
            != Some(request.item_limit as u64)
        || (!request.modules && text("fileContentHash") != Some(group.file_hash.as_str()))
        || (request.modules
            && record
                .metadata
                .get("toolExportConstants")
                .and_then(serde_json::Value::as_u64)
                != Some(group.tools))
        || timestamp_order(&record.updated_at, &group.latest).is_lt()
}
fn hydrate(
    transaction: &StateTransaction<'_>,
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    groups: &mut BTreeMap<String, Group>,
    modules: bool,
    limit: usize,
) -> Result<(), RocksError> {
    if groups.is_empty() {
        return Ok(());
    }
    let files = files(graph, budget)?;
    let mut bytes = 0_usize;
    visit_symbols(graph, budget, |symbol, budget| {
        let file = files.get(&symbol.file_id).ok_or(corrupt())?;
        let Some(key) = group_key(&file.normalized_path, modules) else {
            return Ok(());
        };
        let Some(group) = groups.get_mut(&key) else {
            return Ok(());
        };
        let Some(summary) = current_symbol_summary(transaction, &symbol, budget)? else {
            return Ok(());
        };
        group.retain_item(symbol, file, &summary, limit, &mut bytes)
    })
}
fn item_bytes(item: &Item) -> usize {
    item.symbol.signature.len()
        + item.symbol.qualified_name.len()
        + item.symbol.symbol_kind.len()
        + item.path.len()
        + item.summary.len()
        + 1024
}
fn item_order(a: &Item, b: &Item) -> Ordering {
    b.symbol
        .export
        .exported
        .cmp(&a.symbol.export.exported)
        .then(
            (b.symbol.visibility == Some(Visibility::Public))
                .cmp(&(a.symbol.visibility == Some(Visibility::Public))),
        )
        .then(b.symbol.pagerank_ppb.cmp(&a.symbol.pagerank_ppb))
        .then(a.path.cmp(&b.path))
        .then(a.symbol.start_line.cmp(&b.symbol.start_line))
        .then(a.symbol.symbol_id.cmp(&b.symbol.symbol_id))
}
fn file_pending(
    generation: &GenerationId,
    path: String,
    group: Group,
    anchor: &ContentDigest,
) -> PendingFileSummary {
    let items = group
        .items
        .into_iter()
        .map(|item| SummaryRollupItem {
            symbol_id: item.symbol.symbol_id.to_string(),
            qualified_name: item.symbol.qualified_name,
            symbol_kind: item.symbol.symbol_kind,
            summary: item.summary,
        })
        .collect::<Vec<_>>();
    let digest = summary_rollup_digest(SummaryRollupDigestInput {
        domain: "cartograph:file-summary:v2",
        scope_key: &path,
        source_hash: &group.file_hash,
        total_items: group.count,
        items: &items,
        anchor_digest: anchor,
    });
    PendingFileSummary {
        generation_id: generation.to_string(),
        path,
        file_content_hash: group.file_hash,
        content_hash: digest.to_string(),
        summarized_symbols: group.count,
        items_truncated: group.count > items.len() as u64,
        items,
    }
}
fn module_pending(
    generation: &GenerationId,
    directory: String,
    group: Group,
    anchor: &ContentDigest,
) -> PendingModuleSummary {
    let items = group
        .items
        .into_iter()
        .map(|item| ModuleSummaryRollupItem {
            symbol_id: item.symbol.symbol_id.to_string(),
            path: item.path,
            qualified_name: item.symbol.qualified_name,
            symbol_kind: item.symbol.symbol_kind,
            summary: item.summary,
        })
        .collect::<Vec<_>>();
    let digest = module_summary_rollup_digest(ModuleSummaryRollupDigestInput {
        directory: &directory,
        total_items: group.count,
        tool_export_constants: group.tools,
        items: &items,
        anchor_digest: anchor,
    });
    PendingModuleSummary {
        generation_id: generation.to_string(),
        directory,
        content_hash: digest.to_string(),
        summarized_symbols: group.count,
        tool_export_constants: group.tools,
        items_truncated: group.count > items.len() as u64,
        items,
    }
}

#[cfg(test)]
mod tests;
