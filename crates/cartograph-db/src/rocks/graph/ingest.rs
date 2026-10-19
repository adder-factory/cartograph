use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

pub(super) mod adjacency;
mod file_changes;
mod lexical_index;
pub(super) use file_changes::FileChanges;

use crate::{
    CanonicalSearchDocument, EdgeInput, FileInput, NumericalSiteInput, ReferenceInput, SymbolInput,
};

use super::super::{
    Collection, ReadBudget, RecordChange, RecordKey, RevisionSnapshot, RevisionWriter,
};
use super::{
    DOCUMENT, EDGE, FILE, GraphBase, GraphPublication, GraphPublicationError,
    GraphPublicationInput, GraphRetention, GraphWriteReport, INCOMING, MANIFEST, NUMERICAL,
    OUTGOING, ProjectStore, REFERENCE, Result, RocksError, SYMBOL, StorageRequest, check_workspace,
    codec, count,
};

#[derive(Serialize, Deserialize)]
pub(super) struct EdgeProperties {
    pub(super) confidence: f32,
    pub(super) provenance: String,
    pub(super) site_count: u32,
}

#[derive(Clone, Copy)]
pub(super) enum Row<'a> {
    File(&'a FileInput),
    Symbol(&'a SymbolInput),
    Edge(&'a EdgeInput),
    Reference(&'a ReferenceInput),
    Numerical(&'a NumericalSiteInput),
    Document(&'a CanonicalSearchDocument),
}

// The largest table suffix is source UUID + target UUID + kind + provenance
// digest (65 bytes). Keep one inline key and checked row index per candidate,
// with no per-row key allocation or borrowed-enum allocation retained by sort.
struct TableOrder {
    suffix: [u8; 65],
    index: u32,
    length: u8,
}

impl TableOrder {
    fn new(key: &RecordKey, prefix: &[u8], index: usize) -> Result<Self> {
        let bytes = key
            .as_bytes()
            .strip_prefix(prefix)
            .ok_or_else(codec::corrupt)?;
        if bytes.len() > 65 {
            return Err(codec::corrupt());
        }
        let mut suffix = [0; 65];
        suffix[..bytes.len()].copy_from_slice(bytes);
        Ok(Self {
            suffix,
            index: u32::try_from(index).map_err(|_| super::limit())?,
            length: u8::try_from(bytes.len()).map_err(|_| codec::corrupt())?,
        })
    }

    fn bytes(&self) -> &[u8] {
        &self.suffix[..usize::from(self.length)]
    }
}

fn row_at<'a>(
    facts: &'a crate::CanonicalGenerationFacts,
    prefix: &[u8],
    index: usize,
) -> Result<Row<'a>> {
    match prefix {
        FILE => facts.files().get(index).map(Row::File),
        SYMBOL => facts.symbols().get(index).map(Row::Symbol),
        EDGE => facts.edges().get(index).map(Row::Edge),
        REFERENCE => facts.references().get(index).map(Row::Reference),
        NUMERICAL => facts.numerical_sites().get(index).map(Row::Numerical),
        DOCUMENT => facts.documents().get(index).map(Row::Document),
        _ => None,
    }
    .ok_or_else(codec::corrupt)
}

impl Row<'_> {
    pub(super) fn canonical_encode(self) -> Result<Vec<u8>> {
        if let Self::Edge(row) = self {
            codec::encode(row)
        } else {
            self.encode()
        }
    }

    fn encode(self) -> Result<Vec<u8>> {
        match self {
            Self::File(row) => codec::encode(row),
            Self::Symbol(row) => codec::encode(row),
            Self::Edge(row) => codec::encode(&EdgeProperties {
                confidence: row.confidence,
                provenance: row.provenance.clone(),
                site_count: row.site_count,
            }),
            Self::Reference(row) => codec::encode(row),
            Self::Numerical(row) => codec::encode(row),
            Self::Document(row) => codec::encode(row),
        }
    }
}

pub(super) fn publish(
    store: &ProjectStore,
    mut input: GraphPublicationInput,
    request: &StorageRequest,
    mut reclaim: impl FnMut(usize) -> Result<()>,
) -> std::result::Result<GraphPublication, GraphPublicationError> {
    request.check()?;
    let previous = match store.current_snapshot(None) {
        Ok(snapshot) => Some(snapshot),
        Err(RocksError::GenerationUnavailable) => None,
        Err(error) => return Err(error.into()),
    };
    check_base(&input.identity.base, previous.as_ref())?;
    if let Some(previous) = &previous {
        previous.graph(&mut read_budget(request)?)?;
    }
    let writer = store.begin_revision(
        input.identity.generation_id.clone(),
        input.identity.source_revision.clone(),
        super::Duration::from_secs(300),
    )?;
    let result = prepare_and_publish(
        store,
        &writer,
        &mut input,
        previous.as_ref(),
        request,
        &mut reclaim,
    );
    drop(input);
    match result {
        Ok((revision, report)) => {
            // Release our old native snapshot before collecting retired rows.
            drop(previous);
            let retention = match store.prune(2, 4, 10_000, &mut read_budget(request)?) {
                Ok(report) => GraphRetention::Completed(report),
                Err(error) => GraphRetention::Deferred(error),
            };
            Ok(GraphPublication {
                revision,
                report,
                retention,
            })
        }
        Err(primary) => Err(failed_publication(store, &writer, primary)),
    }
}

fn failed_publication(
    store: &ProjectStore,
    writer: &RevisionWriter,
    primary: RocksError,
) -> GraphPublicationError {
    GraphPublicationError {
        primary,
        cleanup_failure: store.cleanup_graph_revision(writer).err(),
    }
}

fn check_base(base: &GraphBase, previous: Option<&RevisionSnapshot<'_>>) -> Result<()> {
    match (base, previous) {
        (GraphBase::Empty, None) => Ok(()),
        (GraphBase::Current(expected), Some(previous))
            if previous.revision().generation_id == *expected =>
        {
            Ok(())
        }
        _ => Err(RocksError::GenerationChanged),
    }
}

fn prepare_and_publish(
    store: &ProjectStore,
    writer: &RevisionWriter,
    input: &mut GraphPublicationInput,
    previous: Option<&RevisionSnapshot<'_>>,
    request: &StorageRequest,
    reclaim: &mut impl FnMut(usize) -> Result<()>,
) -> Result<(super::StoredRevision, GraphWriteReport)> {
    // A writer may have published between taking the old view and reserving.
    let current = match store.current_snapshot(None) {
        Ok(snapshot) => Some(snapshot),
        Err(RocksError::GenerationUnavailable) => None,
        Err(error) => return Err(error),
    };
    check_base(&input.identity.base, current.as_ref())?;
    drop(current);
    let lexical = lexical_index::statistics(previous, request)?;
    let changes = input.changes.take().filter(|_| lexical.is_some());
    let sparse = changes.is_some();
    let mut delta = Delta {
        store,
        writer,
        previous,
        request,
        policy: input.policy,
        pending: Vec::new(),
        pending_bytes: 0,
        report: GraphWriteReport::default(),
        lexical_bootstrap: lexical.is_none(),
        lexical_statistics: lexical.unwrap_or_default(),
        lexical_working_bytes: input.policy.maximum_working_bytes,
    };
    let info = input.info()?;
    let digest_version = match &input.payload {
        super::GraphPayload::Resident(facts) => facts.digest_version(),
        super::GraphPayload::Packed(graph) => graph.digest_version,
    };
    if let Some(changes) = changes {
        delta.lexical_working_bytes = delta
            .policy
            .maximum_working_bytes
            .checked_sub(usize::try_from(changes.retained_bytes()?).map_err(|_| super::limit())?)
            .ok_or_else(super::limit)?;
        changes.apply(&mut delta)?;
        drop(changes);
        delta.lexical_working_bytes = delta.policy.maximum_working_bytes;
    } else {
        match &mut input.payload {
            super::GraphPayload::Resident(facts) => {
                replace_tables(&mut delta, facts)?;
                replace_adjacency(&mut delta, facts.edges(), false)?;
                replace_adjacency(&mut delta, facts.edges(), true)?;
            }
            super::GraphPayload::Packed(graph) => graph.replay(
                |collection, prefix, rows| {
                    let mut old = PreviousRows::new(previous, collection, prefix, request);
                    rows.visit(request, |key, value| delta.row(&mut old, key, value))?;
                    delta.finish_rows(&mut old)
                },
                request,
                reclaim,
            )?,
        }
        if let super::GraphPayload::Packed(graph) = &input.payload {
            delta.report.adjacency_blocks = graph.adjacency_blocks;
            delta.report.maximum_degree = graph.maximum_degree;
        }
    }
    let name_index_version = info.name_index_version;
    let previous_name_version = previous
        .map(|snapshot| {
            snapshot
                .graph(&mut read_budget(request)?)
                .map(|graph| graph.info().name_index_version)
        })
        .transpose()?
        .unwrap_or(0);
    if (!sparse || previous_name_version != name_index_version)
        && let super::GraphPayload::Resident(facts) = &input.payload
    {
        replace_names(&mut delta, facts, name_index_version != 0)?;
    }
    delta.finish_lexical(info.documents)?;
    let mut old = PreviousRows::new(previous, Collection::Graph, MANIFEST, request);
    delta.row(&mut old, codec::key(MANIFEST, &[])?, codec::encode(&info)?)?;
    delta.finish_rows(&mut old)?;
    delta.flush()?;
    request.check()?;
    store.seal_with_admission(
        writer,
        info.content_digest,
        digest_version,
        input.run_excludes.clone(),
        input.maximum_file_bytes,
    )?;
    request.check()?;
    let revision = store.publish(writer)?;
    Ok((revision, delta.report))
}

fn replace_names(
    delta: &mut Delta<'_, '_>,
    facts: &crate::CanonicalGenerationFacts,
    enabled: bool,
) -> Result<()> {
    use super::names;
    let mut keys = Vec::new();
    if enabled {
        let count = facts
            .symbols()
            .len()
            .checked_mul(2)
            .ok_or_else(super::limit)?;
        check_workspace::<[u8; names::SUFFIX_BYTES]>(count, 0, delta.policy)?;
        keys.try_reserve_exact(count).map_err(|_| super::limit())?;
        check_workspace::<[u8; names::SUFFIX_BYTES]>(keys.capacity(), 0, delta.policy)?;
        for symbol in facts.symbols() {
            delta.request.check()?;
            keys.push(names::suffix(
                &symbol.qualified_name,
                symbol.symbol_id.as_str(),
            )?);
            keys.push(names::suffix(
                names::simple(&symbol.qualified_name),
                symbol.symbol_id.as_str(),
            )?);
        }
        keys.sort_unstable();
        keys.dedup();
    }
    let mut previous = PreviousRows::new(
        delta.previous,
        Collection::Graph,
        names::PREFIX,
        delta.request,
    );
    for key in keys {
        delta.request.check()?;
        delta.row(&mut previous, codec::key(names::PREFIX, &key)?, vec![1])?;
    }
    delta.finish_rows(&mut previous)
}

fn replace_tables(
    delta: &mut Delta<'_, '_>,
    facts: &crate::CanonicalGenerationFacts,
) -> Result<()> {
    let tables = [
        (Collection::Graph, FILE, facts.files().len()),
        (Collection::Graph, SYMBOL, facts.symbols().len()),
        (Collection::Graph, EDGE, facts.edges().len()),
        (Collection::Graph, REFERENCE, facts.references().len()),
        (Collection::Graph, NUMERICAL, facts.numerical_sites().len()),
        (Collection::Search, DOCUMENT, facts.documents().len()),
    ];
    for (collection, prefix, length) in tables {
        delta.request.check()?;
        let mut old = PreviousRows::new(delta.previous, collection, prefix, delta.request);
        visit_table_rows(
            facts,
            prefix,
            length,
            delta.policy,
            delta.request,
            |key, row, workspace| {
                delta.lexical_working_bytes = delta
                    .policy
                    .maximum_working_bytes
                    .checked_sub(workspace)
                    .ok_or_else(super::limit)?;
                delta.row(&mut old, key, row.encode()?)
            },
        )?;
        delta.lexical_working_bytes = delta.policy.maximum_working_bytes;
        delta.finish_rows(&mut old)?;
    }
    Ok(())
}

pub(super) fn validate_workspace(
    facts: &crate::CanonicalGenerationFacts,
    policy: super::GraphPolicy,
    request: &StorageRequest,
) -> Result<()> {
    for length in [
        facts.files().len(),
        facts.symbols().len(),
        facts.edges().len(),
        facts.references().len(),
        facts.numerical_sites().len(),
        facts.documents().len(),
    ] {
        u32::try_from(length).map_err(|_| super::limit())?;
    }
    for prefix in [EDGE, REFERENCE] {
        let length = if prefix == EDGE {
            facts.edges().len()
        } else {
            facts.references().len()
        };
        for group in table_groups(facts, prefix, length, || request.check()) {
            let group = group?;
            check_workspace::<TableOrder>(group.len(), 0, policy)?;
        }
    }
    check_workspace::<usize>(facts.edges().len(), 0, policy)
}

// UUID text order is identical to its canonical binary key order. Only hashes
// and the stored edge-kind discriminant need sorting, and their preceding UUID
// fields already partition canonical rows into contiguous groups.
fn table_groups<'a>(
    facts: &'a crate::CanonicalGenerationFacts,
    prefix: &'a [u8],
    length: usize,
    mut check: impl FnMut() -> Result<()> + 'a,
) -> impl Iterator<Item = Result<std::ops::Range<usize>>> + 'a {
    let mut offset = 0;
    std::iter::from_fn(move || {
        if offset == length {
            return None;
        }
        if let Err(error) = check() {
            offset = length;
            return Some(Err(error));
        }
        let start = offset;
        offset += 1;
        while offset < length
            && match prefix {
                REFERENCE => {
                    facts.references()[start].file_id == facts.references()[offset].file_id
                }
                EDGE => {
                    let left = &facts.edges()[start];
                    let right = &facts.edges()[offset];
                    left.source_symbol_id == right.source_symbol_id
                        && left.target_symbol_id == right.target_symbol_id
                }
                _ => false,
            }
        {
            if let Err(error) = check() {
                offset = length;
                return Some(Err(error));
            }
            offset += 1;
        }
        Some(Ok(start..offset))
    })
}

pub(super) fn visit_table_rows<'a>(
    facts: &'a crate::CanonicalGenerationFacts,
    prefix: &[u8],
    length: usize,
    policy: super::GraphPolicy,
    request: &StorageRequest,
    mut visit: impl FnMut(RecordKey, Row<'a>, usize) -> Result<()>,
) -> Result<()> {
    if prefix != EDGE && prefix != REFERENCE {
        for index in 0..length {
            request.check()?;
            let (key, row) = table_row(facts, prefix, index)?;
            visit(key, row, 0)?;
        }
        return Ok(());
    }
    for group in table_groups(facts, prefix, length, || request.check()) {
        let group = group?;
        request.check()?;
        check_workspace::<TableOrder>(group.len(), 0, policy)?;
        let mut rows = Vec::new();
        rows.try_reserve_exact(group.len())
            .map_err(|_| super::limit())?;
        let workspace = rows
            .capacity()
            .checked_mul(size_of::<TableOrder>())
            .ok_or_else(super::limit)?;
        if workspace > policy.maximum_working_bytes {
            return Err(super::limit());
        }
        for index in group {
            request.check()?;
            let (key, _) = table_row(facts, prefix, index)?;
            rows.push(TableOrder::new(&key, prefix, index)?);
        }
        rows.sort_unstable_by(|left, right| left.bytes().cmp(right.bytes()));
        request.check()?;
        for ordered in rows {
            let index = usize::try_from(ordered.index).map_err(|_| super::limit())?;
            visit(
                codec::key(prefix, ordered.bytes())?,
                row_at(facts, prefix, index)?,
                workspace,
            )?;
        }
    }
    Ok(())
}

fn table_row<'a>(
    facts: &'a crate::CanonicalGenerationFacts,
    prefix: &[u8],
    index: usize,
) -> Result<(RecordKey, Row<'a>)> {
    match prefix {
        FILE => {
            let row = &facts.files()[index];
            Ok((
                codec::key(FILE, &codec::uuid(row.file_id.as_str())?)?,
                Row::File(row),
            ))
        }
        SYMBOL => {
            let row = &facts.symbols()[index];
            Ok((
                codec::key(SYMBOL, &codec::uuid(row.symbol_id.as_str())?)?,
                Row::Symbol(row),
            ))
        }
        EDGE => {
            let row = &facts.edges()[index];
            let entry = codec::entry(&row.target_symbol_id, row.kind, &row.provenance)?;
            Ok((
                codec::edge_key(&row.source_symbol_id, &row.target_symbol_id, &entry)?,
                Row::Edge(row),
            ))
        }
        REFERENCE => {
            let row = &facts.references()[index];
            let mut suffix = codec::uuid(row.file_id.as_str())?.to_vec();
            suffix.extend_from_slice(blake3::hash(&codec::encode(row)?).as_bytes());
            Ok((codec::key(REFERENCE, &suffix)?, Row::Reference(row)))
        }
        NUMERICAL => {
            let row = &facts.numerical_sites()[index];
            Ok((
                codec::key(NUMERICAL, &codec::uuid(row.site_id.as_str())?)?,
                Row::Numerical(row),
            ))
        }
        DOCUMENT => {
            let row = &facts.documents()[index];
            Ok((
                codec::key(DOCUMENT, &codec::uuid(row.document_id.as_str())?)?,
                Row::Document(row),
            ))
        }
        _ => Err(codec::corrupt()),
    }
}

fn replace_adjacency(delta: &mut Delta<'_, '_>, edges: &[EdgeInput], incoming: bool) -> Result<()> {
    let prefix = if incoming { INCOMING } else { OUTGOING };
    let mut old = PreviousRows::new(delta.previous, Collection::Graph, prefix, delta.request);
    let report = adjacency::visit(
        edges,
        incoming,
        delta.policy,
        delta.request,
        |key, value| delta.row(&mut old, key, value),
    )?;
    delta.report.adjacency_blocks = delta
        .report
        .adjacency_blocks
        .checked_add(report.blocks)
        .ok_or_else(super::limit)?;
    delta.report.maximum_degree = delta.report.maximum_degree.max(report.maximum_degree);
    delta.finish_rows(&mut old)
}

struct Delta<'a, 'store> {
    store: &'a ProjectStore,
    writer: &'a RevisionWriter,
    previous: Option<&'a RevisionSnapshot<'store>>,
    request: &'a StorageRequest,
    policy: super::GraphPolicy,
    pending: Vec<RecordChange>,
    pending_bytes: usize,
    report: GraphWriteReport,
    lexical_bootstrap: bool,
    lexical_statistics: super::lexical::LexicalStatistics,
    lexical_working_bytes: usize,
}

impl Delta<'_, '_> {
    fn row(
        &mut self,
        old: &mut PreviousRows<'_, '_>,
        key: RecordKey,
        value: Vec<u8>,
    ) -> Result<()> {
        self.request.check()?;
        self.report.candidate_records += 1;
        while old.peek()?.is_some_and(|row| row.0 < key) {
            let (old_key, _) = old.pop()?.ok_or_else(codec::corrupt)?;
            self.remove(old.collection, old_key)?;
        }
        if old.peek()?.is_some_and(|row| row.0 == key) {
            let (_, old_value) = old.pop()?.ok_or_else(codec::corrupt)?;
            if value == old_value {
                if self.lexical_bootstrap
                    && old.collection == Collection::Search
                    && key.as_bytes().starts_with(DOCUMENT)
                {
                    self.update_lexical(&key, Some(&value))?;
                }
                self.report.reused_records += 1;
                return Ok(());
            }
        }
        self.push(old.collection, key, Some(value))?;
        self.report.written_records += 1;
        Ok(())
    }

    fn finish_rows(&mut self, old: &mut PreviousRows<'_, '_>) -> Result<()> {
        while let Some((key, _)) = old.pop()? {
            self.remove(old.collection, key)?;
        }
        Ok(())
    }

    fn remove(&mut self, collection: Collection, key: RecordKey) -> Result<()> {
        self.push(collection, key, None)?;
        self.report.deleted_records += 1;
        Ok(())
    }

    fn push(
        &mut self,
        collection: Collection,
        key: RecordKey,
        value: Option<Vec<u8>>,
    ) -> Result<()> {
        if collection == Collection::Search && key.as_bytes().starts_with(DOCUMENT) {
            self.update_lexical(&key, value.as_deref())?;
        }
        let bytes = key
            .as_bytes()
            .len()
            .saturating_mul(3)
            .saturating_add(value.as_ref().map_or(0, Vec::len))
            .saturating_add(128);
        let store_policy = self.store.batch_policy();
        // Native validation reserves this fixed metadata allowance. Retain it
        // when the store's byte ceiling is tighter than the graph's ceiling.
        let maximum_bytes = self
            .policy
            .maximum_batch_bytes
            .min(store_policy.maximum_batch_bytes.saturating_sub(4_096));
        if bytes > maximum_bytes {
            return Err(RocksError::Limit {
                resource: "graph_batch_bytes",
            });
        }
        if self.pending.len() >= store_policy.maximum_batch_rows
            || self.pending_bytes.saturating_add(bytes) > maximum_bytes
        {
            self.flush()?;
        }
        self.pending_bytes += bytes;
        // The 128-byte per-row allowance also bounds retained Vec capacity:
        // geometric spare capacity for RecordChange is below one batch allowance.
        // Key/value payloads and native staging copies retain the existing four
        // batch allowances charged by storage-job admission.
        self.pending.push(match value {
            Some(value) => RecordChange::put(collection, key, value),
            None => RecordChange::delete(collection, key),
        });
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.request.check()?;
        if !self.pending.is_empty() {
            self.store
                .stage_controlled(self.writer, &self.pending, Some(self.request))?;
            self.pending.clear();
            self.pending_bytes = 0;
        }
        Ok(())
    }
}

struct PreviousRows<'a, 'store> {
    snapshot: Option<&'a RevisionSnapshot<'store>>,
    collection: Collection,
    prefix: &'static [u8],
    rows: VecDeque<(RecordKey, Vec<u8>)>,
    after: Option<RecordKey>,
    done: bool,
    request: &'a StorageRequest,
}

impl<'a, 'store> PreviousRows<'a, 'store> {
    fn new(
        snapshot: Option<&'a RevisionSnapshot<'store>>,
        collection: Collection,
        prefix: &'static [u8],
        request: &'a StorageRequest,
    ) -> Self {
        Self {
            snapshot,
            collection,
            prefix,
            rows: VecDeque::new(),
            after: None,
            done: snapshot.is_none(),
            request,
        }
    }

    fn fill(&mut self) -> Result<()> {
        self.request.check()?;
        if self.rows.is_empty()
            && !self.done
            && let Some(snapshot) = self.snapshot
        {
            let page = snapshot.scan_prefix(
                self.collection,
                self.prefix,
                self.after.as_ref(),
                256,
                &mut read_budget(self.request)?,
            )?;
            self.after = page.next_after;
            self.done = self.after.is_none();
            self.rows = page.rows.into();
        }
        Ok(())
    }

    fn peek(&mut self) -> Result<Option<&(RecordKey, Vec<u8>)>> {
        self.fill()?;
        Ok(self.rows.front())
    }
    fn pop(&mut self) -> Result<Option<(RecordKey, Vec<u8>)>> {
        self.fill()?;
        Ok(self.rows.pop_front())
    }
}

fn read_budget(request: &StorageRequest) -> Result<ReadBudget> {
    Ok(
        ReadBudget::new(100_000, 64 * 1_048_576, super::Duration::from_secs(300))?
            .with_control(request.clone()),
    )
}

#[cfg(test)]
mod tests;
