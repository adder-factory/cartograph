use std::mem::size_of;

use blake3::Hasher;
use cartograph_domain::{ContentDigest, GenerationDigestVersion};

use super::model::{
    CanonicalSearchDocument, EdgeInput, FileInput, NumericalSiteInput, ReferenceInput, SymbolInput,
    ValidatedFactTables,
};

const DIGEST_V1_TO_V6_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v4";
const DIGEST_V7_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v5";
const DIGEST_V8_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v6";
const DIGEST_V9_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v7";
const DIGEST_V10_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v8";
const DIGEST_V11_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v9";
const DIGEST_V12_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v10";
const DIGEST_V13_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v11";
const DIGEST_V14_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v12";
const DIGEST_V15_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v13";
const DIGEST_V16_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v14";

pub(super) fn logical_digest<Cancel>(
    facts: &ValidatedFactTables,
    version: GenerationDigestVersion,
    mut cancelled: Cancel,
) -> Result<ContentDigest, ()>
where
    Cancel: FnMut() -> bool,
{
    let mut digest = LogicalDigestBuilder::new(version);
    digest.begin_files(usize_to_u64(facts.files.len()));
    push_digest_rows(&facts.files, &mut cancelled, |file| digest.push_file(file))?;
    digest.begin_symbols(usize_to_u64(facts.symbols.len()));
    push_digest_rows(&facts.symbols, &mut cancelled, |symbol| {
        digest.push_symbol(symbol);
    })?;
    digest.begin_edges(usize_to_u64(facts.edges.len()));
    push_digest_rows(&facts.edges, &mut cancelled, |edge| digest.push_edge(edge))?;
    digest.begin_references(usize_to_u64(facts.references.len()));
    push_digest_rows(&facts.references, &mut cancelled, |reference| {
        digest.push_reference(reference);
    })?;
    if digest_includes_numerical_sites(version) {
        digest.begin_numerical_sites(usize_to_u64(facts.numerical_sites.len()));
        push_digest_rows(&facts.numerical_sites, &mut cancelled, |site| {
            digest.push_numerical_site(site);
        })?;
    }
    digest.begin_documents(usize_to_u64(facts.documents.len()));
    push_digest_rows(&facts.documents, &mut cancelled, |document| {
        digest.push_document(document);
    })?;
    require_digest_not_cancelled(&mut cancelled)?;
    Ok(digest.finish())
}

fn push_digest_rows<Row, Cancel, Push>(
    rows: &[Row],
    cancelled: &mut Cancel,
    mut push: Push,
) -> Result<(), ()>
where
    Cancel: FnMut() -> bool,
    Push: FnMut(&Row),
{
    for row in rows {
        if cancelled() {
            return Err(());
        }
        push(row);
    }
    Ok(())
}

fn require_digest_not_cancelled<Cancel>(cancelled: &mut Cancel) -> Result<(), ()>
where
    Cancel: FnMut() -> bool,
{
    if cancelled() { Err(()) } else { Ok(()) }
}

const fn digest_includes_numerical_sites(version: GenerationDigestVersion) -> bool {
    matches!(
        version,
        GenerationDigestVersion::V7
            | GenerationDigestVersion::V8
            | GenerationDigestVersion::V9
            | GenerationDigestVersion::V10
            | GenerationDigestVersion::V11
            | GenerationDigestVersion::V12
            | GenerationDigestVersion::V13
            | GenerationDigestVersion::V14
            | GenerationDigestVersion::V15
            | GenerationDigestVersion::V16
    )
}

pub(crate) struct LogicalDigestBuilder {
    digest: CanonicalDigest,
}

impl LogicalDigestBuilder {
    pub(crate) fn new(version: GenerationDigestVersion) -> Self {
        let domain = match version {
            GenerationDigestVersion::V16 => DIGEST_V16_DOMAIN,
            GenerationDigestVersion::V15 => DIGEST_V15_DOMAIN,
            GenerationDigestVersion::V14 => DIGEST_V14_DOMAIN,
            GenerationDigestVersion::V13 => DIGEST_V13_DOMAIN,
            GenerationDigestVersion::V12 => DIGEST_V12_DOMAIN,
            GenerationDigestVersion::V11 => DIGEST_V11_DOMAIN,
            GenerationDigestVersion::V10 => DIGEST_V10_DOMAIN,
            GenerationDigestVersion::V9 => DIGEST_V9_DOMAIN,
            GenerationDigestVersion::V8 => DIGEST_V8_DOMAIN,
            GenerationDigestVersion::V7 => DIGEST_V7_DOMAIN,
            _ => DIGEST_V1_TO_V6_DOMAIN,
        };
        Self {
            digest: CanonicalDigest::new(domain),
        }
    }

    pub(crate) fn begin_files(&mut self, count: u64) {
        self.digest.section("files", count);
    }

    pub(crate) fn push_file(&mut self, file: &FileInput) {
        self.digest.text(file.file_id.as_str());
        self.digest.text(&file.normalized_path);
        self.digest.text(&file.language);
        self.digest.text(file.content_hash.as_str());
        self.digest.u64(file.byte_size);
        self.digest.text(file.parse_status.as_str());
    }

    pub(crate) fn begin_symbols(&mut self, count: u64) {
        self.digest.section("symbols", count);
    }

    pub(crate) fn push_symbol(&mut self, symbol: &SymbolInput) {
        self.digest.text(symbol.symbol_id.as_str());
        self.digest.text(symbol.file_id.as_str());
        self.digest.text(&symbol.symbol_kind);
        self.digest.text(&symbol.qualified_name);
        self.digest.text(&symbol.signature);
        self.digest.u64(symbol.start_byte);
        self.digest.u64(symbol.end_byte);
        self.digest.u32(symbol.start_line);
        self.digest.u32(symbol.end_line);
        self.digest.text(symbol.structural_digest.as_str());
        self.digest
            .optional_text(symbol.visibility.map(cartograph_domain::Visibility::as_str));
        self.digest.boolean(symbol.export.exported);
        self.digest.boolean(symbol.export.default_export);
        self.digest.boolean(symbol.execution.async_symbol);
        self.digest.boolean(symbol.execution.static_member);
        self.digest.boolean(symbol.declaration_only);
    }

    pub(crate) fn begin_edges(&mut self, count: u64) {
        self.digest.section("edges", count);
    }

    pub(crate) fn push_edge(&mut self, edge: &EdgeInput) {
        self.digest.text(edge.source_symbol_id.as_str());
        self.digest.text(edge.target_symbol_id.as_str());
        self.digest.text(edge.kind.as_str());
        self.digest.u32(edge.confidence.to_bits());
        self.digest.text(&edge.provenance);
        self.digest.u32(edge.site_count);
    }

    pub(crate) fn begin_references(&mut self, count: u64) {
        self.digest.section("references", count);
    }

    pub(crate) fn push_reference(&mut self, reference: &ReferenceInput) {
        self.digest.text(reference.file_id.as_str());
        self.digest.optional_text(
            reference
                .owner_symbol_id
                .as_ref()
                .map(cartograph_domain::SymbolId::as_str),
        );
        self.digest.optional_text(
            reference
                .target_symbol_id
                .as_ref()
                .map(cartograph_domain::SymbolId::as_str),
        );
        self.digest.text(&reference.reference_name);
        self.digest.text(&reference.reference_kind);
        self.digest.u64(reference.start_byte);
        self.digest.u64(reference.end_byte);
        self.digest.u32(reference.confidence.to_bits());
        self.digest.text(&reference.resolution_provenance);
        self.digest.u32(reference.site_count);
        self.digest.text(reference.span_precision.as_str());
    }

    pub(crate) fn begin_numerical_sites(&mut self, count: u64) {
        self.digest.section("numerical_sites", count);
    }

    pub(crate) fn push_numerical_site(&mut self, site: &NumericalSiteInput) {
        self.digest.text(site.site_id.as_str());
        self.digest.text(site.file_id.as_str());
        self.digest.optional_text(
            site.owner_symbol_id
                .as_ref()
                .map(cartograph_domain::SymbolId::as_str),
        );
        self.digest.u64(site.start_byte);
        self.digest.u64(site.end_byte);
        self.digest.u32(site.start_line);
        self.digest.u32(site.end_line);
        self.digest.text(&site.operation);
        self.digest.text(&site.hazard);
        self.digest.text(&site.precision);
        self.digest.text(site.expression_digest.as_str());
        self.digest.u32(site.confidence_ppm);
        self.digest.text(&site.provenance);
        self.digest.text(&site.evidence_level);
        self.digest.text(&site.unknowns);
    }

    pub(crate) fn begin_documents(&mut self, count: u64) {
        self.digest.section("search_documents", count);
    }

    pub(crate) fn push_document(&mut self, document: &CanonicalSearchDocument) {
        self.digest.text(document.document_id.as_str());
        self.digest.optional_text(
            document
                .file_id
                .as_ref()
                .map(cartograph_domain::FileId::as_str),
        );
        self.digest.optional_text(
            document
                .symbol_id
                .as_ref()
                .map(cartograph_domain::SymbolId::as_str),
        );
        self.digest.text(&document.path);
        self.digest.text(&document.language);
        self.digest.text(document.kind.as_str());
        self.digest.text(&document.qualified_name);
        self.digest.text(&document.code);
        self.digest.text(&document.natural_text);
        self.digest.text(&document.metadata_json);
    }

    pub(crate) fn push_encoded_row(&mut self, row: &[u8]) {
        self.digest.encoded_bytes = self
            .digest
            .encoded_bytes
            .saturating_add(usize_to_u64(row.len()));
        self.digest.hasher.update(row);
    }

    pub(crate) fn finish(self) -> ContentDigest {
        self.digest.finish().0
    }

    pub(crate) fn finish_with_encoded_bytes(self) -> (ContentDigest, u64) {
        self.digest.finish()
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

struct CanonicalDigest {
    hasher: Hasher,
    encoded_bytes: u64,
}

impl CanonicalDigest {
    fn new(domain: &[u8]) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(domain);
        Self {
            hasher,
            encoded_bytes: 0,
        }
    }

    fn section(&mut self, name: &str, count: u64) {
        self.text(name);
        self.text(&count.to_string());
    }

    fn text(&mut self, value: &str) {
        let length = value.len().to_string();
        self.encoded_bytes = self
            .encoded_bytes
            .saturating_add(usize_to_u64(length.len()))
            .saturating_add(1)
            .saturating_add(usize_to_u64(value.len()));
        self.hasher.update(length.as_bytes());
        self.hasher.update(&[0]);
        self.hasher.update(value.as_bytes());
    }

    fn optional_text(&mut self, value: Option<&str>) {
        self.encoded_bytes = self.encoded_bytes.saturating_add(1);
        match value {
            Some(value) => {
                self.hasher.update(&[1]);
                self.text(value);
            }
            None => {
                self.hasher.update(&[0]);
            }
        }
    }

    fn u64(&mut self, value: u64) {
        self.encoded_bytes = self
            .encoded_bytes
            .saturating_add(usize_to_u64(size_of::<u64>()));
        self.hasher.update(&value.to_be_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.encoded_bytes = self
            .encoded_bytes
            .saturating_add(usize_to_u64(size_of::<u32>()));
        self.hasher.update(&value.to_be_bytes());
    }

    fn boolean(&mut self, value: bool) {
        self.encoded_bytes = self.encoded_bytes.saturating_add(1);
        self.hasher.update(&[u8::from(value)]);
    }

    fn finish(self) -> (ContentDigest, u64) {
        (
            ContentDigest::from_bytes(*self.hasher.finalize().as_bytes()),
            self.encoded_bytes,
        )
    }
}
