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
const DIGEST_V17_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v15";
const DIGEST_V18_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v16";
const DIGEST_V19_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v17";
const DIGEST_V20_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v18";
const DIGEST_V21_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v19";
const DIGEST_V22_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v20";
const DIGEST_V23_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v21";
const DIGEST_V24_DOMAIN: &[u8] = b"cartograph-v3-logical-generation-v24";

/// Each digest contract's hash domain, in [`GenerationDigestVersion::ALL`]
/// order. The length follows `ALL`, so a new contract does not compile until
/// it names its own domain instead of silently reusing an older one.
const DIGEST_DOMAINS: [&[u8]; GenerationDigestVersion::ALL.len()] = [
    DIGEST_V1_TO_V6_DOMAIN,
    DIGEST_V1_TO_V6_DOMAIN,
    DIGEST_V1_TO_V6_DOMAIN,
    DIGEST_V1_TO_V6_DOMAIN,
    DIGEST_V1_TO_V6_DOMAIN,
    DIGEST_V1_TO_V6_DOMAIN,
    DIGEST_V7_DOMAIN,
    DIGEST_V8_DOMAIN,
    DIGEST_V9_DOMAIN,
    DIGEST_V10_DOMAIN,
    DIGEST_V11_DOMAIN,
    DIGEST_V12_DOMAIN,
    DIGEST_V13_DOMAIN,
    DIGEST_V14_DOMAIN,
    DIGEST_V15_DOMAIN,
    DIGEST_V16_DOMAIN,
    DIGEST_V17_DOMAIN,
    DIGEST_V18_DOMAIN,
    DIGEST_V19_DOMAIN,
    DIGEST_V20_DOMAIN,
    DIGEST_V21_DOMAIN,
    DIGEST_V22_DOMAIN,
    DIGEST_V23_DOMAIN,
    DIGEST_V24_DOMAIN,
];

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
            | GenerationDigestVersion::V17
            | GenerationDigestVersion::V18
            | GenerationDigestVersion::V19
            | GenerationDigestVersion::V20
            | GenerationDigestVersion::V21
            | GenerationDigestVersion::V22
            | GenerationDigestVersion::V23
            | GenerationDigestVersion::V24
    )
}

pub(crate) struct LogicalDigestBuilder {
    digest: CanonicalDigest,
}

impl LogicalDigestBuilder {
    pub(crate) fn new(version: GenerationDigestVersion) -> Self {
        // `ALL` lists every contract, so the fallback is never taken.
        let domain = GenerationDigestVersion::ALL
            .iter()
            .zip(DIGEST_DOMAINS)
            .find_map(|(candidate, domain)| (*candidate == version).then_some(domain))
            .unwrap_or(DIGEST_V1_TO_V6_DOMAIN);
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

    pub(crate) fn finish(self) -> ContentDigest {
        self.digest.finish().0
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

struct CanonicalDigest {
    hasher: Hasher,
    encoded_bytes: u64,
    // Bound the transient stream workspace while letting BLAKE3 process whole
    // chunks. The logical bytes, domains and row-level cancellation stay exact.
    buffer: [u8; 8192],
    buffered: usize,
}

impl CanonicalDigest {
    fn new(domain: &[u8]) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(domain);
        Self {
            hasher,
            encoded_bytes: 0,
            buffer: [0; 8192],
            buffered: 0,
        }
    }

    fn section(&mut self, name: &str, count: u64) {
        self.text(name);
        self.text(itoa::Buffer::new().format(count));
    }

    fn text(&mut self, value: &str) {
        let mut decimal = itoa::Buffer::new();
        let length = decimal.format(value.len());
        self.encoded_bytes = self
            .encoded_bytes
            .saturating_add(usize_to_u64(length.len()))
            .saturating_add(1)
            .saturating_add(usize_to_u64(value.len()));
        self.bytes(length.as_bytes());
        self.bytes(&[0]);
        self.bytes(value.as_bytes());
    }

    fn optional_text(&mut self, value: Option<&str>) {
        self.encoded_bytes = self.encoded_bytes.saturating_add(1);
        match value {
            Some(value) => {
                self.bytes(&[1]);
                self.text(value);
            }
            None => {
                self.bytes(&[0]);
            }
        }
    }

    fn u64(&mut self, value: u64) {
        self.encoded_bytes = self
            .encoded_bytes
            .saturating_add(usize_to_u64(size_of::<u64>()));
        self.bytes(&value.to_be_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.encoded_bytes = self
            .encoded_bytes
            .saturating_add(usize_to_u64(size_of::<u32>()));
        self.bytes(&value.to_be_bytes());
    }

    fn boolean(&mut self, value: bool) {
        self.encoded_bytes = self.encoded_bytes.saturating_add(1);
        self.bytes(&[u8::from(value)]);
    }

    fn bytes(&mut self, mut bytes: &[u8]) {
        if bytes.len() > self.buffer.len() - self.buffered {
            let remaining = self.buffer.len() - self.buffered;
            self.buffer[self.buffered..].copy_from_slice(&bytes[..remaining]);
            self.buffered = self.buffer.len();
            self.flush();
            bytes = &bytes[remaining..];
            let whole_buffers = bytes.len() / self.buffer.len() * self.buffer.len();
            if whole_buffers != 0 {
                self.hasher.update(&bytes[..whole_buffers]);
                bytes = &bytes[whole_buffers..];
            }
        }
        let end = self.buffered + bytes.len();
        self.buffer[self.buffered..end].copy_from_slice(bytes);
        self.buffered = end;
    }

    fn flush(&mut self) {
        self.hasher.update(&self.buffer[..self.buffered]);
        self.buffered = 0;
    }

    fn finish(mut self) -> (ContentDigest, u64) {
        self.flush();
        (
            ContentDigest::from_bytes(*self.hasher.finalize().as_bytes()),
            self.encoded_bytes,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::CanonicalDigest;
    use cartograph_domain::ContentDigest;

    #[test]
    fn canonical_wire_format_preserves_lengths_markers_and_integer_widths() {
        let domain = b"canonical-wire-contract";
        let mut digest = CanonicalDigest::new(domain);
        digest.section("rows", u64::MAX);
        digest.text("");
        digest.text("é🦀");
        digest.optional_text(None);
        digest.optional_text(Some(""));
        digest.optional_text(Some("λ"));
        digest.u64(u64::MAX);
        digest.u32(u32::MAX);
        digest.boolean(true);
        digest.boolean(false);
        // This is the existing wire contract, specified independently of the
        // optimized encoder. Decimal lengths count UTF-8 bytes, not characters.
        let encoded = b"4\x00rows20\x00184467440737095516150\x006\x00\xc3\xa9\xf0\x9f\xa6\x80\x00\x010\x00\x012\x00\xce\xbb\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\x01\x00";
        let mut oracle = blake3::Hasher::new();
        oracle.update(domain);
        oracle.update(encoded);
        let (actual, bytes) = digest.finish();
        assert_eq!(
            actual,
            ContentDigest::from_bytes(*oracle.finalize().as_bytes())
        );
        assert_eq!(bytes, u64::try_from(encoded.len()).unwrap_or(u64::MAX));
    }

    #[test]
    fn canonical_stream_matches_legacy_encoding_across_buffer_boundaries() {
        let domain = b"canonical-stream-contract";
        let mut digest = CanonicalDigest::new(domain);
        let mut oracle = blake3::Hasher::new();
        oracle.update(domain);
        let mut encoded_bytes = 0_u64;
        let values =
            [0, 1, 63, 1023, 8191, 8192, 8193, 32769, 7, 8190].map(|length| "x".repeat(length));
        for value in values.into_iter().chain(["é\0🦀".to_owned()]) {
            digest.text(&value);
            // Preserve the original unbuffered decimal framing as the oracle.
            let prefix = value.len().to_string();
            oracle.update(prefix.as_bytes());
            oracle.update(&[0]);
            oracle.update(value.as_bytes());
            encoded_bytes += u64::try_from(prefix.len() + 1 + value.len()).unwrap_or(u64::MAX);
        }
        let (actual, bytes) = digest.finish();
        assert_eq!(
            actual,
            ContentDigest::from_bytes(*oracle.finalize().as_bytes())
        );
        assert_eq!(bytes, encoded_bytes);
    }
}
