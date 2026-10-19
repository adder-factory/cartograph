//! Lossless publication blocks. Canonical validation remains a separate phase.

use std::{borrow::Cow, mem::size_of};

use cartograph_domain::{EdgeKind, GenerationDigestVersion};
use serde::Deserialize;

use super::{
    CanonicalGenerationFacts, DOCUMENT, EDGE, FILE, GraphInfo, GraphPolicy, INCOMING, NUMERICAL,
    OUTGOING, REFERENCE, Result, RocksError, SYMBOL, StorageRequest, codec, count, ingest, names,
};
use crate::{
    FileInput,
    rocks::{Collection, RecordKey},
};

const BLOCK_BYTES: usize = 64 * 1_024;
const MAXIMUM_ROW_BYTES: usize = super::super::engine::MAXIMUM_RECORD_BYTES;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Table {
    Files,
    Symbols,
    Edges,
    References,
    Numerical,
    Documents,
    Outgoing,
    Incoming,
    Names,
}

impl Table {
    const ALL: [Self; 9] = [
        Self::Files,
        Self::Symbols,
        Self::Edges,
        Self::References,
        Self::Numerical,
        Self::Documents,
        Self::Outgoing,
        Self::Incoming,
        Self::Names,
    ];

    fn prefix(self) -> &'static [u8] {
        match self {
            Self::Files => FILE,
            Self::Symbols => SYMBOL,
            Self::Edges => EDGE,
            Self::References => REFERENCE,
            Self::Numerical => NUMERICAL,
            Self::Documents => DOCUMENT,
            Self::Outgoing => OUTGOING,
            Self::Incoming => INCOMING,
            Self::Names => names::PREFIX,
        }
    }

    fn collection(self) -> Collection {
        if self == Self::Documents {
            Collection::Search
        } else {
            Collection::Graph
        }
    }

    fn canonical(self) -> bool {
        !matches!(self, Self::Outgoing | Self::Incoming | Self::Names)
    }
}

struct Block {
    bytes: Vec<u8>,
    decoded_length: u32,
    checksum: [u8; 32],
    compressed: bool,
}

pub(super) struct PackedTable {
    kind: Table,
    rows: u64,
    blocks: Vec<Block>,
}

pub(super) struct PackedGraph {
    pub(super) files: Vec<FileInput>,
    pub(super) info: GraphInfo,
    pub(super) digest_version: GenerationDigestVersion,
    pub(super) adjacency_blocks: u64,
    pub(super) maximum_degree: u64,
    tables: Vec<PackedTable>,
}

impl PackedGraph {
    pub(super) fn reclaimable_bytes(&self) -> Result<usize> {
        self.tables.iter().try_fold(
            self.tables
                .capacity()
                .checked_mul(size_of::<PackedTable>())
                .ok_or_else(super::limit)?,
            |bytes, table| {
                bytes
                    .checked_add(table.allocation_bytes()?)
                    .ok_or_else(super::limit)
            },
        )
    }

    pub(super) fn build(
        facts: CanonicalGenerationFacts,
        policy: GraphPolicy,
        request: &StorageRequest,
    ) -> Result<Self> {
        request.check()?;
        let info = GraphInfo {
            format_version: 1,
            name_index_version: names::version(&facts, policy),
            content_digest: facts.digest().clone(),
            files: count(facts.files().len())?,
            symbols: count(facts.symbols().len())?,
            edges: count(facts.edges().len())?,
            references: count(facts.references().len())?,
            numerical_sites: count(facts.numerical_sites().len())?,
            documents: count(facts.documents().len())?,
        };
        let mut tables = Vec::new();
        tables
            .try_reserve_exact(Table::ALL.len())
            .map_err(|_| super::limit())?;
        let base = size_of::<Self>()
            .checked_add(
                tables
                    .capacity()
                    .checked_mul(size_of::<PackedTable>())
                    .ok_or_else(super::limit)?,
            )
            .and_then(|bytes| bytes.checked_add(info.content_digest.as_str().len()))
            .ok_or_else(super::limit)?;
        let mut budget = Budget {
            bytes: u64::try_from(base).map_err(|_| super::limit())?,
            maximum: policy.maximum_input_bytes,
        };
        budget.check()?;
        let mut adjacency_blocks = 0_u64;
        let mut maximum_degree = 0;
        for kind in Table::ALL {
            request.check()?;
            let mut builder = Builder::new(kind, &mut budget)?;
            if kind.canonical() {
                let length = match kind {
                    Table::Files => facts.files().len(),
                    Table::Symbols => facts.symbols().len(),
                    Table::Edges => facts.edges().len(),
                    Table::References => facts.references().len(),
                    Table::Numerical => facts.numerical_sites().len(),
                    Table::Documents => facts.documents().len(),
                    _ => return Err(codec::corrupt()),
                };
                ingest::visit_table_rows(
                    &facts,
                    kind.prefix(),
                    length,
                    policy,
                    request,
                    |_, row, _| builder.row(&row.canonical_encode()?, request),
                )?;
            } else if kind == Table::Names {
                pack_names(
                    &facts,
                    info.name_index_version != 0,
                    policy,
                    request,
                    &mut builder,
                )?;
            } else {
                let report = ingest::adjacency::visit(
                    facts.edges(),
                    kind == Table::Incoming,
                    policy,
                    request,
                    |key, value| builder.record(&key, &value, request),
                )?;
                adjacency_blocks = adjacency_blocks
                    .checked_add(report.blocks)
                    .ok_or_else(super::limit)?;
                maximum_degree = maximum_degree.max(report.maximum_degree);
            }
            tables.push(builder.finish(request)?);
        }
        let digest_version = facts.digest_version();
        // Move the small source fence; dropping the other canonical tables is
        // mandatory before the storage owner can admit this package.
        let files = facts.into_source_manifest();
        let graph = Self {
            files,
            info,
            digest_version,
            adjacency_blocks,
            maximum_degree,
            tables,
        };
        if graph.retained_bytes()? > policy.maximum_input_bytes {
            return Err(input_limit());
        }
        request.check()?;
        Ok(graph)
    }

    pub(super) fn retained_bytes(&self) -> Result<u64> {
        let mut bytes = size_of::<Self>()
            .checked_add(
                self.tables
                    .capacity()
                    .checked_mul(size_of::<PackedTable>())
                    .ok_or_else(super::limit)?,
            )
            .and_then(|n| n.checked_add(self.files.capacity().checked_mul(size_of::<FileInput>())?))
            .and_then(|n| n.checked_add(self.info.content_digest.as_str().len()))
            .ok_or_else(super::limit)?;
        for file in &self.files {
            for amount in [
                file.file_id.as_str().len(),
                file.normalized_path.capacity(),
                file.language.capacity(),
                file.content_hash.as_str().len(),
            ] {
                bytes = bytes.checked_add(amount).ok_or_else(super::limit)?;
            }
        }
        for table in &self.tables {
            bytes = bytes
                .checked_add(
                    table
                        .blocks
                        .capacity()
                        .checked_mul(size_of::<Block>())
                        .ok_or_else(super::limit)?,
                )
                .ok_or_else(super::limit)?;
            for block in &table.blocks {
                bytes = bytes
                    .checked_add(block.bytes.capacity())
                    .ok_or_else(super::limit)?;
            }
        }
        u64::try_from(bytes).map_err(|_| super::limit())
    }

    pub(super) fn replay(
        &mut self,
        mut table: impl FnMut(Collection, &'static [u8], &PackedTable) -> Result<()>,
        request: &StorageRequest,
        mut reclaim: impl FnMut(usize) -> Result<()>,
    ) -> Result<()> {
        if self.tables.len() != Table::ALL.len() || count(self.files.len())? != self.info.files {
            return Err(codec::corrupt());
        }
        let counts = [
            self.info.files,
            self.info.symbols,
            self.info.edges,
            self.info.references,
            self.info.numerical_sites,
            self.info.documents,
        ];
        let mut adjacency = 0_u64;
        let descriptors = self
            .tables
            .capacity()
            .checked_mul(size_of::<PackedTable>())
            .ok_or_else(super::limit)?;
        // IntoIter still owns the descriptor allocation, including spare slots.
        // Individual tables release only their own block allocations below.
        let mut tables = std::mem::take(&mut self.tables).into_iter();
        for (index, &expected) in Table::ALL.iter().enumerate() {
            let rows = tables.next().ok_or_else(codec::corrupt)?;
            request.check()?;
            if rows.kind != expected || counts.get(index).is_some_and(|&count| count != rows.rows) {
                return Err(codec::corrupt());
            }
            if matches!(expected, Table::Outgoing | Table::Incoming) {
                adjacency = adjacency.checked_add(rows.rows).ok_or_else(super::limit)?;
            }
            if expected == Table::Names
                && ((self.info.name_index_version == 0 && rows.rows != 0)
                    || rows.rows > self.info.symbols.checked_mul(2).ok_or_else(super::limit)?)
            {
                return Err(codec::corrupt());
            }
            table(expected.collection(), expected.prefix(), &rows)?;
            let released = rows.allocation_bytes()?;
            drop(rows);
            reclaim(released)?;
        }
        drop(tables);
        reclaim(descriptors)?;
        if adjacency != self.adjacency_blocks {
            return Err(codec::corrupt());
        }
        Ok(())
    }
}

fn input_limit() -> RocksError {
    RocksError::Limit {
        resource: "graph_input_bytes",
    }
}

struct Budget {
    bytes: u64,
    maximum: u64,
}

impl Budget {
    fn check(&self) -> Result<()> {
        if self.bytes > self.maximum {
            Err(input_limit())
        } else {
            Ok(())
        }
    }
    fn add(&mut self, bytes: usize) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(u64::try_from(bytes).map_err(|_| super::limit())?)
            .ok_or_else(super::limit)?;
        self.check()
    }
}

struct Builder<'a> {
    table: PackedTable,
    buffer: Vec<u8>,
    budget: &'a mut Budget,
}

impl<'a> Builder<'a> {
    fn new(kind: Table, budget: &'a mut Budget) -> Result<Self> {
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(BLOCK_BYTES)
            .map_err(|_| super::limit())?;
        Ok(Self {
            table: PackedTable {
                kind,
                rows: 0,
                blocks: Vec::new(),
            },
            buffer,
            budget,
        })
    }

    fn row(&mut self, bytes: &[u8], request: &StorageRequest) -> Result<()> {
        if bytes.is_empty() || bytes.len() > MAXIMUM_ROW_BYTES {
            return Err(codec::corrupt());
        }
        let length = u32::try_from(bytes.len()).map_err(|_| super::limit())?;
        self.write(&length.to_be_bytes(), request)?;
        self.write(bytes, request)?;
        self.table.rows = self.table.rows.checked_add(1).ok_or_else(super::limit)?;
        Ok(())
    }

    fn record(&mut self, key: &RecordKey, value: &[u8], request: &StorageRequest) -> Result<()> {
        let key_length = u16::try_from(key.as_bytes().len()).map_err(|_| codec::corrupt())?;
        let length = 2_usize
            .checked_add(key.as_bytes().len())
            .and_then(|n| n.checked_add(value.len()))
            .ok_or_else(super::limit)?;
        if length > MAXIMUM_ROW_BYTES {
            return Err(codec::corrupt());
        }
        self.write(
            &u32::try_from(length)
                .map_err(|_| super::limit())?
                .to_be_bytes(),
            request,
        )?;
        self.write(&key_length.to_be_bytes(), request)?;
        self.write(key.as_bytes(), request)?;
        self.write(value, request)?;
        self.table.rows = self.table.rows.checked_add(1).ok_or_else(super::limit)?;
        Ok(())
    }

    fn write(&mut self, mut bytes: &[u8], request: &StorageRequest) -> Result<()> {
        while !bytes.is_empty() {
            request.check()?;
            let take = bytes.len().min(BLOCK_BYTES - self.buffer.len());
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buffer.len() == BLOCK_BYTES {
                self.flush(request)?;
            }
        }
        Ok(())
    }

    fn flush(&mut self, request: &StorageRequest) -> Result<()> {
        request.check()?;
        if self.buffer.is_empty() {
            return Ok(());
        }
        let block = encode_block(&self.buffer)?;
        self.budget.add(block.bytes.capacity())?;
        if self.table.blocks.len() == self.table.blocks.capacity() {
            let previous = self.table.blocks.capacity();
            let extra = 64_usize
                .checked_mul(size_of::<Block>())
                .ok_or_else(super::limit)?;
            // Fail before reserving an unbounded descriptor array.
            if self
                .budget
                .bytes
                .checked_add(u64::try_from(extra).map_err(|_| super::limit())?)
                .is_none_or(|n| n > self.budget.maximum)
            {
                return Err(input_limit());
            }
            self.table
                .blocks
                .try_reserve_exact(64)
                .map_err(|_| super::limit())?;
            self.budget.add(
                self.table
                    .blocks
                    .capacity()
                    .checked_sub(previous)
                    .and_then(|n| n.checked_mul(size_of::<Block>()))
                    .ok_or_else(super::limit)?,
            )?;
        }
        self.table.blocks.push(block);
        self.buffer.clear();
        request.check()
    }

    fn finish(mut self, request: &StorageRequest) -> Result<PackedTable> {
        self.flush(request)?;
        Ok(self.table)
    }
}

fn encode_block(raw: &[u8]) -> Result<Block> {
    if raw.is_empty() || raw.len() > BLOCK_BYTES {
        return Err(codec::corrupt());
    }
    let maximum = lz4_flex::block::get_maximum_output_size(raw.len());
    if maximum > 2 * BLOCK_BYTES {
        return Err(codec::corrupt());
    }
    let mut scratch = Vec::new();
    scratch
        .try_reserve_exact(maximum)
        .map_err(|_| super::limit())?;
    scratch.resize(maximum, 0);
    let used = lz4_flex::block::compress_into(raw, &mut scratch).map_err(|_| codec::corrupt())?;
    let compressed = used < raw.len();
    let source = if compressed { &scratch[..used] } else { raw };
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(source.len())
        .map_err(|_| super::limit())?;
    bytes.extend_from_slice(source);
    Ok(Block {
        bytes,
        decoded_length: u32::try_from(raw.len()).map_err(|_| super::limit())?,
        checksum: *blake3::hash(raw).as_bytes(),
        compressed,
    })
}

struct Reader<'a> {
    blocks: &'a [Block],
    next_block: usize,
    decoded: Vec<u8>,
    offset: usize,
}

impl Reader<'_> {
    fn fill(&mut self, request: &StorageRequest) -> Result<bool> {
        request.check()?;
        if self.offset < self.decoded.len() {
            return Ok(true);
        }
        let Some(block) = self.blocks.get(self.next_block) else {
            return Ok(false);
        };
        let length = usize::try_from(block.decoded_length).map_err(|_| codec::corrupt())?;
        if length == 0 || length > BLOCK_BYTES || block.bytes.len() > 2 * BLOCK_BYTES {
            return Err(codec::corrupt());
        }
        self.decoded.clear();
        self.decoded
            .try_reserve_exact(length)
            .map_err(|_| super::limit())?;
        self.decoded.resize(length, 0);
        if block.compressed {
            if lz4_flex::block::decompress_into(&block.bytes, &mut self.decoded)
                .map_err(|_| codec::corrupt())?
                != length
            {
                return Err(codec::corrupt());
            }
        } else {
            if block.bytes.len() != length {
                return Err(codec::corrupt());
            }
            self.decoded.copy_from_slice(&block.bytes);
        }
        if blake3::hash(&self.decoded).as_bytes() != &block.checksum {
            return Err(codec::corrupt());
        }
        self.next_block += 1;
        self.offset = 0;
        request.check()?;
        Ok(true)
    }

    fn read(&mut self, mut output: &mut [u8], request: &StorageRequest) -> Result<()> {
        while !output.is_empty() {
            if !self.fill(request)? {
                return Err(codec::corrupt());
            }
            let take = output.len().min(self.decoded.len() - self.offset);
            output[..take].copy_from_slice(&self.decoded[self.offset..self.offset + take]);
            self.offset += take;
            output = &mut output[take..];
        }
        Ok(())
    }

    fn row(&mut self, request: &StorageRequest) -> Result<Option<Vec<u8>>> {
        if !self.fill(request)? {
            return Ok(None);
        }
        let mut length = [0; 4];
        self.read(&mut length, request)?;
        let length = usize::try_from(u32::from_be_bytes(length)).map_err(|_| codec::corrupt())?;
        if length == 0 || length > MAXIMUM_ROW_BYTES {
            return Err(codec::corrupt());
        }
        let mut row = Vec::new();
        row.try_reserve_exact(length).map_err(|_| super::limit())?;
        row.resize(length, 0);
        self.read(&mut row, request)?;
        Ok(Some(row))
    }
}

impl PackedTable {
    fn allocation_bytes(&self) -> Result<usize> {
        self.blocks.iter().try_fold(
            self.blocks
                .capacity()
                .checked_mul(size_of::<Block>())
                .ok_or_else(super::limit)?,
            |bytes, block| {
                bytes
                    .checked_add(block.bytes.capacity())
                    .ok_or_else(super::limit)
            },
        )
    }

    pub(super) fn visit(
        &self,
        request: &StorageRequest,
        mut emit: impl FnMut(RecordKey, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        let mut reader = Reader {
            blocks: &self.blocks,
            next_block: 0,
            decoded: Vec::new(),
            offset: 0,
        };
        let mut previous: Option<RecordKey> = None;
        let mut rows = 0_u64;
        while let Some(bytes) = reader.row(request)? {
            let (key, value) = record(self.kind, bytes)?;
            if !key.as_bytes().starts_with(self.kind.prefix())
                || previous
                    .as_ref()
                    .is_some_and(|old| old.as_bytes() >= key.as_bytes())
            {
                return Err(codec::corrupt());
            }
            previous = Some(key.clone());
            emit(key, value)?;
            rows = rows.checked_add(1).ok_or_else(super::limit)?;
            if rows > self.rows {
                return Err(codec::corrupt());
            }
        }
        if rows != self.rows {
            return Err(codec::corrupt());
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct IdFields<'a> {
    #[serde(rename = "file_id")]
    file: Option<&'a str>,
    #[serde(rename = "symbol_id")]
    symbol: Option<&'a str>,
    #[serde(rename = "document_id")]
    document: Option<&'a str>,
    #[serde(rename = "site_id")]
    site: Option<&'a str>,
}

#[derive(Deserialize)]
struct EdgeFields<'a> {
    source_symbol_id: &'a str,
    target_symbol_id: &'a str,
    kind: EdgeKind,
    confidence: f32,
    #[serde(borrow)]
    provenance: Cow<'a, str>,
    site_count: u32,
}

fn record(kind: Table, bytes: Vec<u8>) -> Result<(RecordKey, Vec<u8>)> {
    if !kind.canonical() {
        let header: [u8; 2] = bytes
            .get(..2)
            .ok_or_else(codec::corrupt)?
            .try_into()
            .map_err(|_| codec::corrupt())?;
        let length = usize::from(u16::from_be_bytes(header));
        let end = 2_usize.checked_add(length).ok_or_else(codec::corrupt)?;
        let key = RecordKey::new(bytes.get(2..end).ok_or_else(codec::corrupt)?.to_vec())?;
        return Ok((key, bytes.get(end..).ok_or_else(codec::corrupt)?.to_vec()));
    }
    if kind == Table::Edges {
        let edge: EdgeFields<'_> = serde_json::from_slice(&bytes).map_err(|_| codec::corrupt())?;
        let mut suffix = codec::uuid(edge.source_symbol_id)?.to_vec();
        suffix.extend_from_slice(&codec::uuid(edge.target_symbol_id)?);
        suffix.push(edge.kind as u8);
        suffix.extend_from_slice(blake3::hash(edge.provenance.as_bytes()).as_bytes());
        let key = codec::key(EDGE, &suffix)?;
        let value = codec::encode(&ingest::EdgeProperties {
            confidence: edge.confidence,
            provenance: edge.provenance.into_owned(),
            site_count: edge.site_count,
        })?;
        return Ok((key, value));
    }
    let ids: IdFields<'_> = serde_json::from_slice(&bytes).map_err(|_| codec::corrupt())?;
    let id = match kind {
        Table::Files | Table::References => ids.file,
        Table::Symbols => ids.symbol,
        Table::Numerical => ids.site,
        Table::Documents => ids.document,
        _ => return Err(codec::corrupt()),
    }
    .ok_or_else(codec::corrupt)?;
    let mut suffix = codec::uuid(id)?.to_vec();
    if kind == Table::References {
        suffix.extend_from_slice(blake3::hash(&bytes).as_bytes());
    }
    Ok((codec::key(kind.prefix(), &suffix)?, bytes))
}

fn pack_names(
    facts: &CanonicalGenerationFacts,
    enabled: bool,
    policy: GraphPolicy,
    request: &StorageRequest,
    builder: &mut Builder<'_>,
) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    let length = facts
        .symbols()
        .len()
        .checked_mul(2)
        .ok_or_else(super::limit)?;
    super::check_workspace::<[u8; names::SUFFIX_BYTES]>(length, 0, policy)?;
    let mut keys = Vec::new();
    keys.try_reserve_exact(length).map_err(|_| super::limit())?;
    super::check_workspace::<[u8; names::SUFFIX_BYTES]>(keys.capacity(), 0, policy)?;
    for symbol in facts.symbols() {
        request.check()?;
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
    for key in keys {
        request.check()?;
        builder.record(&codec::key(names::PREFIX, &key)?, &[1], request)?;
    }
    Ok(())
}
