use std::mem::size_of;

use cartograph_domain::{
    ContentDigest, DocumentId, EdgeKind, FileId, GenerationId, NumericalSiteId, SymbolId,
};
use serde::de::DeserializeOwned;

use crate::{
    CanonicalSearchDocument, EdgeInput, FileInput, GraphDirection, NumericalSiteInput,
    ReferenceInput, SymbolInput,
};

use super::super::engine::PointReaders;
use super::super::{Collection, ReadBudget, RecordKey, Result, RevisionSnapshot, RocksError};
use super::{
    DOCUMENT, FILE, GraphInfo, INCOMING, MANIFEST, NUMERICAL, OUTGOING, REFERENCE, SYMBOL, codec,
    ingest::EdgeProperties,
};

/// Opaque cursor tied to one revision, vertex, direction, and packed block.
#[derive(Clone)]
pub struct GraphCursor {
    project: ContentDigest,
    generation: GenerationId,
    node: SymbolId,
    incoming: bool,
    block: RecordKey,
    offset: usize,
}

/// Topology-only relationship; properties are fetched separately on demand.
#[derive(Clone, Debug)]
pub struct GraphLink {
    project: ContentDigest,
    generation: GenerationId,
    source: SymbolId,
    target: SymbolId,
    kind: EdgeKind,
    entry: [u8; codec::ENTRY_BYTES],
}

impl GraphLink {
    /// Source symbol of this relationship.
    #[must_use]
    pub const fn source_symbol_id(&self) -> &SymbolId {
        &self.source
    }

    /// Target symbol of this relationship.
    #[must_use]
    pub const fn target_symbol_id(&self) -> &SymbolId {
        &self.target
    }

    /// Structural relationship category.
    #[must_use]
    pub const fn kind(&self) -> EdgeKind {
        self.kind
    }
}

/// Finite topology page in stable neighbor/kind/provenance-digest order.
pub struct GraphNeighborPage {
    /// Relationships; no source text or property documents are read.
    pub links: Vec<GraphLink>,
    /// Continue under the same immutable revision, including short pages.
    pub next: Option<GraphCursor>,
}

/// Typed graph view borrowing exactly one complete native revision snapshot.
pub struct GraphSnapshot<'view, 'store> {
    pub(super) snapshot: &'view RevisionSnapshot<'store>,
    point_readers: PointReaders<'view, 'store>,
    pub(super) info: GraphInfo,
}

impl<'store> RevisionSnapshot<'store> {
    /// Validate the typed graph format and digest for this pinned revision.
    ///
    /// # Errors
    /// Rejects missing graph capability, format/digest mismatch, or read bounds.
    pub fn graph<'view>(
        &'view self,
        budget: &mut ReadBudget,
    ) -> Result<GraphSnapshot<'view, 'store>> {
        let bytes = self
            .get(Collection::Graph, &codec::key(MANIFEST, &[])?, budget)?
            .ok_or(RocksError::Format)?;
        let info: GraphInfo = codec::decode(&bytes)?;
        if info.format_version != 1 || info.name_index_version > 1 {
            return Err(RocksError::Format);
        }
        if self.revision().content_digest.as_ref() != Some(&info.content_digest) {
            return Err(codec::corrupt());
        }
        Ok(GraphSnapshot {
            snapshot: self,
            point_readers: self.point_readers(),
            info,
        })
    }
}

impl GraphSnapshot<'_, '_> {
    /// Exact generation pinned by this immutable view.
    #[must_use]
    pub fn generation_id(&self) -> &GenerationId {
        &self.snapshot.revision().generation_id
    }

    /// Read a finite canonical file page from this revision.
    ///
    /// # Errors
    /// Returns cursor, decoding, budget, or storage failures.
    pub fn files(
        &self,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<FileInput>, Option<RecordKey>)> {
        self.table_records(Collection::Graph, FILE, after, limit, budget)
    }

    /// Read a finite canonical symbol page from this revision.
    ///
    /// # Errors
    /// Returns cursor, decoding, budget, or storage failures.
    pub fn symbols(
        &self,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<SymbolInput>, Option<RecordKey>)> {
        self.table_records(Collection::Graph, SYMBOL, after, limit, budget)
    }

    /// Page exact simple or qualified names, matching Unicode lowercase keys.
    /// Callers retain their original case-sensitive or qualified predicates and
    /// result ordering. Inspect `info().name_index_version` before using this
    /// optional index; zero requires the existing symbol scan.
    /// # Errors
    /// Rejects unavailable indexes, invalid names, cursors, corrupt postings,
    /// changed/missing symbols, and exhausted read budgets.
    pub fn symbols_by_name(
        &self,
        name: &str,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<SymbolInput>, Option<RecordKey>)> {
        use super::names;
        if self.info.name_index_version != 1 || name.is_empty() || name.len() > 4096 {
            return Err(RocksError::InvalidInput {
                field: "name_index",
            });
        }
        let mut prefix = names::PREFIX.to_vec();
        prefix.extend_from_slice(&names::hash(name));
        let page = self
            .snapshot
            .scan_prefix(Collection::Graph, &prefix, after, limit, budget)?;
        let expected = name.to_lowercase();
        let mut rows = Vec::new();
        for (key, marker) in page.rows {
            if marker != [1] {
                return Err(codec::corrupt());
            }
            let raw = key
                .as_bytes()
                .strip_prefix(prefix.as_slice())
                .ok_or_else(codec::corrupt)?;
            let id = codec::symbol(raw)?;
            let symbol = self.symbol(&id, budget)?.ok_or_else(codec::corrupt)?;
            // Hash collisions cannot admit an unrelated declaration.
            if symbol.qualified_name.to_lowercase() == expected
                || names::simple(&symbol.qualified_name).to_lowercase() == expected
            {
                rows.push(symbol);
            }
        }
        Ok((rows, page.next_after))
    }

    /// Read a finite canonical document page for bounded native retrieval.
    ///
    /// # Errors
    /// Returns cursor, decoding, budget, or storage failures.
    pub fn documents(
        &self,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<CanonicalSearchDocument>, Option<RecordKey>)> {
        self.table_records(Collection::Search, DOCUMENT, after, limit, budget)
    }

    /// Read a finite canonical reference page, including unresolved references.
    ///
    /// # Errors
    /// Returns cursor, decoding, budget, or storage failures.
    pub fn reference_records(
        &self,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<ReferenceInput>, Option<RecordKey>)> {
        self.table_records(Collection::Graph, REFERENCE, after, limit, budget)
    }

    /// Read a finite canonical numerical-site page from this revision.
    /// # Errors
    /// Returns cursor, decoding, budget, or storage failures.
    pub fn numerical_records(
        &self,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<NumericalSiteInput>, Option<RecordKey>)> {
        self.table_records(Collection::Graph, NUMERICAL, after, limit, budget)
    }

    fn table_records<T: DeserializeOwned>(
        &self,
        collection: Collection,
        prefix: &[u8],
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<T>, Option<RecordKey>)> {
        let page = self
            .snapshot
            .scan_prefix(collection, prefix, after, limit, budget)?;
        let rows = page
            .rows
            .into_iter()
            .map(|(_, bytes)| codec::decode(&bytes))
            .collect::<Result<Vec<_>>>()?;
        Ok((rows, page.next_after))
    }

    /// Exact row counts and physical graph format for the pinned revision.
    #[must_use]
    pub const fn info(&self) -> &GraphInfo {
        &self.info
    }

    /// Read one canonical source-file record.
    ///
    /// # Errors
    /// Returns read-budget, decoding, or storage failures.
    pub fn file(&self, id: &FileId, budget: &mut ReadBudget) -> Result<Option<FileInput>> {
        self.record(Collection::Graph, FILE, &codec::uuid(id.as_str())?, budget)
    }

    /// Read one canonical declaration without traversing its relationships.
    ///
    /// # Errors
    /// Returns read-budget, decoding, or storage failures.
    pub fn symbol(&self, id: &SymbolId, budget: &mut ReadBudget) -> Result<Option<SymbolInput>> {
        self.record(
            Collection::Graph,
            SYMBOL,
            &codec::uuid(id.as_str())?,
            budget,
        )
    }

    /// Read one static numerical evidence record.
    ///
    /// # Errors
    /// Returns read-budget, decoding, or storage failures.
    pub fn numerical_site(
        &self,
        id: &NumericalSiteId,
        budget: &mut ReadBudget,
    ) -> Result<Option<NumericalSiteInput>> {
        self.record(
            Collection::Graph,
            NUMERICAL,
            &codec::uuid(id.as_str())?,
            budget,
        )
    }

    /// Read a canonical durable search document; this does not imply BM25 readiness.
    ///
    /// # Errors
    /// Returns read-budget, decoding, or storage failures.
    pub fn document(
        &self,
        id: &DocumentId,
        budget: &mut ReadBudget,
    ) -> Result<Option<CanonicalSearchDocument>> {
        self.record(
            Collection::Search,
            DOCUMENT,
            &codec::uuid(id.as_str())?,
            budget,
        )
    }

    /// Read a bounded file-reference page with the native logical-key cursor.
    ///
    /// # Errors
    /// Returns cursor, read-budget, decoding, or storage failures.
    pub fn references(
        &self,
        id: &FileId,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<ReferenceInput>, Option<RecordKey>)> {
        let prefix = codec::key(REFERENCE, &codec::uuid(id.as_str())?)?;
        let page = self.snapshot.scan_prefix(
            Collection::Graph,
            prefix.as_bytes(),
            after,
            limit,
            budget,
        )?;
        let mut rows = Vec::with_capacity(page.rows.len());
        for (_, bytes) in page.rows {
            rows.push(codec::decode(&bytes)?);
        }
        Ok((rows, page.next_after))
    }

    /// Traverse compact adjacency without loading confidence/provenance properties.
    ///
    /// Each block contains at most 128 binary relationships. Large vertices
    /// use radix-partitioned blocks, so a local edit rewrites a bounded leaf
    /// instead of shifting every subsequent adjacency page.
    ///
    /// # Errors
    /// Rejects foreign/stale cursors, malformed blocks, or exhausted read bounds.
    pub fn neighbors(
        &self,
        node: &SymbolId,
        direction: GraphDirection,
        after: Option<&GraphCursor>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<GraphNeighborPage> {
        if limit == 0 || limit > 4_096 {
            return Err(RocksError::Limit {
                resource: "graph_neighbors",
            });
        }
        let incoming = matches!(direction, GraphDirection::Incoming);
        if after.is_some_and(|cursor| {
            cursor.project != *self.snapshot.project_identity()
                || cursor.generation != self.snapshot.revision().generation_id
                || cursor.node != *node
                || cursor.incoming != incoming
        }) {
            return Err(RocksError::GenerationChanged);
        }
        let prefix = codec::key(
            if incoming { INCOMING } else { OUTGOING },
            &codec::uuid(node.as_str())?,
        )?;
        Neighborhood {
            snapshot: self.snapshot,
            node,
            incoming,
            prefix,
            cursor: after.cloned(),
            resume_block: after.is_some(),
            links: Vec::new(),
            limit: usize::from(limit),
        }
        .read(budget)
    }

    /// Hydrate confidence, provenance, and site multiplicity for one topology link.
    ///
    /// # Errors
    /// Rejects links from another revision or mismatching/missing properties.
    pub fn edge(&self, link: &GraphLink, budget: &mut ReadBudget) -> Result<EdgeInput> {
        if link.project != *self.snapshot.project_identity()
            || link.generation != self.snapshot.revision().generation_id
        {
            return Err(RocksError::GenerationChanged);
        }
        let key = codec::edge_key(&link.source, &link.target, &link.entry)?;
        let bytes = self
            .point_readers
            .get(Collection::Graph, &key, budget)?
            .ok_or_else(codec::corrupt)?;
        let properties: EdgeProperties = codec::decode(&bytes)?;
        if !properties.confidence.is_finite()
            || !(0.0..=1.0).contains(&properties.confidence)
            || properties.site_count == 0
            || properties.site_count > 100_000_000
            || properties.provenance.len() > 256
            || blake3::hash(properties.provenance.as_bytes())
                .as_bytes()
                .as_slice()
                != &link.entry[codec::ID_BYTES + 1..]
        {
            return Err(codec::corrupt());
        }
        Ok(EdgeInput {
            source_symbol_id: link.source.clone(),
            target_symbol_id: link.target.clone(),
            kind: link.kind,
            confidence: properties.confidence,
            provenance: properties.provenance,
            site_count: properties.site_count,
        })
    }

    fn record<T: DeserializeOwned>(
        &self,
        collection: Collection,
        prefix: &[u8],
        suffix: &[u8],
        budget: &mut ReadBudget,
    ) -> Result<Option<T>> {
        self.point_readers
            .get(collection, &codec::key(prefix, suffix)?, budget)?
            .map(|bytes| codec::decode(&bytes))
            .transpose()
    }
}

struct Neighborhood<'view, 'store> {
    snapshot: &'view RevisionSnapshot<'store>,
    node: &'view SymbolId,
    incoming: bool,
    prefix: RecordKey,
    cursor: Option<GraphCursor>,
    resume_block: bool,
    links: Vec<GraphLink>,
    limit: usize,
}

impl Neighborhood<'_, '_> {
    fn read(mut self, budget: &mut ReadBudget) -> Result<GraphNeighborPage> {
        while let Some((key, bytes)) = self.next_block(budget)? {
            let entries = validate_block(&key, self.prefix.as_bytes(), &bytes)?;
            let offset = self
                .cursor
                .as_ref()
                .filter(|cursor| cursor.block == key)
                .map_or(0, |cursor| cursor.offset);
            if offset > entries.len() / codec::ENTRY_BYTES {
                return Err(codec::corrupt());
            }
            self.append(&key, entries, offset, budget)?;
            if self.links.len() == self.limit {
                return Ok(GraphNeighborPage {
                    links: self.links,
                    next: self.cursor,
                });
            }
            self.cursor = Some(self.cursor_at(key, entries.len() / codec::ENTRY_BYTES));
        }
        Ok(GraphNeighborPage {
            links: self.links,
            next: None,
        })
    }

    fn next_block(&mut self, budget: &mut ReadBudget) -> Result<Option<(RecordKey, Vec<u8>)>> {
        if self.resume_block {
            self.resume_block = false;
            let key = &self.cursor.as_ref().ok_or_else(codec::corrupt)?.block;
            let bytes = self
                .snapshot
                .get(Collection::Graph, key, budget)?
                .ok_or_else(codec::corrupt)?;
            return Ok(Some((key.clone(), bytes)));
        }
        let page = self.snapshot.scan_prefix(
            Collection::Graph,
            self.prefix.as_bytes(),
            self.cursor.as_ref().map(|cursor| &cursor.block),
            1,
            budget,
        )?;
        Ok(page.rows.into_iter().next())
    }

    fn append(
        &mut self,
        key: &RecordKey,
        entries: &[u8],
        offset: usize,
        budget: &mut ReadBudget,
    ) -> Result<()> {
        let initial_length = self.links.len();
        for entry in entries
            .as_chunks::<{ codec::ENTRY_BYTES }>()
            .0
            .iter()
            .skip(offset)
        {
            budget.charge(size_of::<GraphLink>() + 64 + 3 * 36)?;
            self.links.push(self.link(*entry)?);
            if self.links.len() == self.limit {
                break;
            }
        }
        if self.links.len() > initial_length {
            self.cursor =
                Some(self.cursor_at(key.clone(), offset + self.links.len() - initial_length));
        }
        Ok(())
    }

    fn link(&self, entry: [u8; codec::ENTRY_BYTES]) -> Result<GraphLink> {
        let neighbor = codec::symbol(&entry[..codec::ID_BYTES])?;
        let kind = codec::kind(entry[codec::ID_BYTES])?;
        let (source, target) = if self.incoming {
            (neighbor, self.node.clone())
        } else {
            (self.node.clone(), neighbor)
        };
        Ok(GraphLink {
            project: self.snapshot.project_identity().clone(),
            generation: self.snapshot.revision().generation_id.clone(),
            source,
            target,
            kind,
            entry,
        })
    }

    fn cursor_at(&self, block: RecordKey, offset: usize) -> GraphCursor {
        GraphCursor {
            project: self.snapshot.project_identity().clone(),
            generation: self.snapshot.revision().generation_id.clone(),
            node: self.node.clone(),
            incoming: self.incoming,
            block,
            offset,
        }
    }
}

fn validate_block<'a>(key: &RecordKey, prefix: &[u8], bytes: &'a [u8]) -> Result<&'a [u8]> {
    let payload = bytes
        .strip_prefix(codec::BLOCK_HEADER)
        .ok_or_else(codec::corrupt)?;
    let depth_bytes: [u8; 2] = payload
        .get(..2)
        .ok_or_else(codec::corrupt)?
        .try_into()
        .map_err(|_| codec::corrupt())?;
    let depth = usize::from(u16::from_be_bytes(depth_bytes));
    let entries = payload.get(2..).ok_or_else(codec::corrupt)?;
    let suffix = key
        .as_bytes()
        .strip_prefix(prefix)
        .ok_or_else(codec::corrupt)?;
    if entries.is_empty()
        || entries.len() % codec::ENTRY_BYTES != 0
        || entries.len() / codec::ENTRY_BYTES > codec::BLOCK_CAPACITY
        || depth > codec::ENTRY_BYTES * 8
        || (depth == 0 && !suffix.is_empty())
        || (depth > 0 && suffix.len() != codec::ENTRY_BYTES)
    {
        return Err(codec::corrupt());
    }
    let mut previous = None;
    for entry in entries.as_chunks::<{ codec::ENTRY_BYTES }>().0 {
        if (depth > 0 && codec::radix_prefix(entry, depth)?.as_slice() != suffix)
            || previous.is_some_and(|previous| previous >= entry)
        {
            return Err(codec::corrupt());
        }
        codec::kind(entry[codec::ID_BYTES])?;
        previous = Some(entry);
    }
    Ok(entries)
}
