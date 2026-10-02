use std::{
    collections::{BTreeMap, btree_map::Entry},
    io::{self, Write},
    mem::size_of,
};

use cartograph_domain::{FileId, FileParseStatus, GenerationDigestVersion, SymbolId};
use serde_json::Value;
use thiserror::Error;

use crate::StorageError;

use super::{
    digest::logical_digest,
    model::{
        CanonicalGenerationFacts, CanonicalSearchDocument, EdgeInput, FileInput, GenerationFacts,
        GenerationMemoryModelError, NumericalSiteInput, ReferenceInput, SearchDocumentInput,
        SymbolInput, ValidatedFactTables,
    },
};

const MAX_PATH_BYTES: usize = 4_096;
const MAX_LANGUAGE_BYTES: usize = 64;
const MAX_SYMBOL_KIND_BYTES: usize = 64;
const MAX_QUALIFIED_NAME_BYTES: usize = 2_048;
const MAX_SIGNATURE_BYTES: usize = 64 * 1_024;
const MAX_PROVENANCE_BYTES: usize = 256;
const MAX_REFERENCE_NAME_BYTES: usize = 4_096;
const MAX_REFERENCE_KIND_BYTES: usize = 64;
const MAX_NUMERICAL_CATEGORY_BYTES: usize = 64;
const MAX_NUMERICAL_UNKNOWNS_BYTES: usize = 256;
const MAX_CODE_BYTES: usize = 8 * 1_024 * 1_024;
const MAX_NATURAL_TEXT_BYTES: usize = 1_024 * 1_024;
const MAX_METADATA_BYTES: usize = 64 * 1_024;
const MAX_METADATA_DEPTH: usize = 64;
const MAX_SITE_COUNT: u32 = 100_000_000;
const MIN_CANONICAL_OBJECT_ENTRY_BYTES: usize = 4;
const MAX_DATABASE_BIGINT: u64 = i64::MAX.unsigned_abs();
const MAX_DATABASE_INTEGER: u32 = i32::MAX.unsigned_abs();
const MAX_VALIDATION_RETAINED_BYTES: u64 = 256 * 1024 * 1024 * 1024;
const MAP_NODE_ALLOWANCE: u64 = 128;

/// Explicit output and transient-working-set bounds for canonical validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenerationValidationLimits {
    maximum_output_bytes: u64,
    maximum_working_bytes: u64,
}

impl GenerationValidationLimits {
    /// Require nonzero limits with enough working room to retain the output.
    /// # Errors
    ///
    /// Returns an error if either limit is zero, the working limit is below
    /// the output limit, or retained validation memory exceeds its hard cap.
    pub const fn new(
        maximum_output_bytes: u64,
        maximum_working_bytes: u64,
    ) -> Result<Self, GenerationValidationError> {
        if maximum_output_bytes == 0
            || maximum_working_bytes < maximum_output_bytes
            || maximum_working_bytes > MAX_VALIDATION_RETAINED_BYTES
        {
            return Err(GenerationValidationError::RetainedLimit);
        }
        Ok(Self {
            maximum_output_bytes,
            maximum_working_bytes,
        })
    }

    /// Maximum canonical output bytes.
    #[must_use]
    pub const fn maximum_output_bytes(self) -> u64 {
        self.maximum_output_bytes
    }

    /// Maximum conservative cumulative validation allocation charge.
    #[must_use]
    pub const fn maximum_working_bytes(self) -> u64 {
        self.maximum_working_bytes
    }
}

/// Fixed-size proof of bounded canonical validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenerationValidationReport {
    input: u64,
    output: u64,
    charged_high_water: u64,
}

impl GenerationValidationReport {
    /// Conservative raw input bytes retained at validation admission.
    #[must_use]
    pub const fn input_bytes(self) -> u64 {
        self.input
    }

    /// Conservative canonical output bytes retained for COPY.
    #[must_use]
    pub const fn output_bytes(self) -> u64 {
        self.output
    }

    /// Monotonic conservative allocation charge; released allocations remain charged.
    #[must_use]
    pub const fn charged_high_water_bytes(self) -> u64 {
        self.charged_high_water
    }
}

/// Canonical validation failed without rendering source or credentials.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum GenerationValidationError {
    /// A storage field or relation violated its public boundary.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// One extracted reference name exceeded the canonical byte boundary.
    #[error("Cartograph generation reference name exceeds the canonical byte limit")]
    ReferenceNameTooLong,
    /// Cooperative validation cancellation was observed.
    #[error("Cartograph generation validation was cancelled")]
    Cancelled,
    /// Input, output, or conservative transient allocation exceeded policy.
    #[error("Cartograph generation validation exceeded its retained-memory limit")]
    RetainedLimit,
}

impl From<GenerationMemoryModelError> for GenerationValidationError {
    fn from(error: GenerationMemoryModelError) -> Self {
        match error {
            GenerationMemoryModelError::Cancelled => Self::Cancelled,
            GenerationMemoryModelError::RetainedLimit => Self::RetainedLimit,
            GenerationMemoryModelError::MetadataDepth => Self::Storage(invalid("metadata_depth")),
        }
    }
}

struct ValidationControl<Cancel> {
    cancelled: Cancel,
    limits: GenerationValidationLimits,
    input_bytes: u64,
    charged_bytes: u64,
}

impl<Cancel> ValidationControl<Cancel>
where
    Cancel: FnMut() -> bool,
{
    const fn new(limits: GenerationValidationLimits, cancelled: Cancel) -> Self {
        Self {
            cancelled,
            limits,
            input_bytes: 0,
            charged_bytes: 0,
        }
    }

    fn admit(&mut self, facts: &GenerationFacts) -> Result<(), GenerationValidationError> {
        self.poll()?;
        let measurement = {
            let cancelled = &mut self.cancelled;
            facts.measure_retained_bytes(self.limits.maximum_working_bytes, cancelled)?
        };
        let charged_bytes = measurement
            .retained_bytes()
            .checked_add(measurement.transient_bytes())
            .ok_or(GenerationValidationError::RetainedLimit)?;
        if charged_bytes > self.limits.maximum_working_bytes {
            return Err(GenerationValidationError::RetainedLimit);
        }
        self.input_bytes = measurement.retained_bytes();
        self.charged_bytes = charged_bytes;
        Ok(())
    }

    fn poll(&mut self) -> Result<(), GenerationValidationError> {
        if (self.cancelled)() {
            Err(GenerationValidationError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn charge(&mut self, bytes: u64) -> Result<(), GenerationValidationError> {
        self.poll()?;
        self.charged_bytes = self
            .charged_bytes
            .checked_add(bytes)
            .ok_or(GenerationValidationError::RetainedLimit)?;
        if self.charged_bytes > self.limits.maximum_working_bytes {
            return Err(GenerationValidationError::RetainedLimit);
        }
        Ok(())
    }

    fn finish(
        mut self,
        facts: &CanonicalGenerationFacts,
    ) -> Result<GenerationValidationReport, GenerationValidationError> {
        self.poll()?;
        let measurement = {
            let cancelled = &mut self.cancelled;
            facts.measure_retained_bytes(self.limits.maximum_output_bytes, cancelled)?
        };
        let output_bytes = measurement.retained_bytes();
        self.charge(measurement.transient_bytes())?;
        Ok(GenerationValidationReport {
            input: self.input_bytes,
            output: output_bytes,
            charged_high_water: self.charged_bytes,
        })
    }
}

#[cfg(test)]
fn validate_and_reduce(facts: GenerationFacts) -> Result<CanonicalGenerationFacts, StorageError> {
    let limits = GenerationValidationLimits::new(
        MAX_VALIDATION_RETAINED_BYTES / 4,
        MAX_VALIDATION_RETAINED_BYTES,
    )
    .map_err(|_| invalid("generation_validation_limits"))?;
    validate_generation_facts(facts, limits, || false)
        .map(|(facts, _)| facts)
        .map_err(|error| match error {
            GenerationValidationError::Storage(error) => error,
            GenerationValidationError::ReferenceNameTooLong => invalid("reference_name"),
            GenerationValidationError::Cancelled => invalid("generation_validation_cancelled"),
            GenerationValidationError::RetainedLimit => invalid("generation_retained_bytes"),
        })
}

/// Validate, reduce, digest, and bound one unordered payload with cooperative polling.
/// # Errors
///
/// Returns an error if validation is cancelled, memory/output bounds are
/// exceeded, identities conflict, or cross-table relationships are invalid.
pub fn validate_generation_facts<Cancel>(
    facts: GenerationFacts,
    limits: GenerationValidationLimits,
    cancelled: Cancel,
) -> Result<(CanonicalGenerationFacts, GenerationValidationReport), GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    validate_generation_facts_with_version(
        facts,
        GenerationValidationPolicy {
            limits,
            digest_version: GenerationDigestVersion::CURRENT,
        },
        cancelled,
    )
}

pub(crate) fn validate_spill_fact_batch<Cancel>(
    facts: GenerationFacts,
    limits: GenerationValidationLimits,
    cancelled: Cancel,
) -> Result<(ValidatedFactTables, u64), GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    let mut control = ValidationControl::new(limits, cancelled);
    control.admit(&facts)?;
    let tables = ValidatedFactTables {
        files: reduce_files(facts.files, &mut control)?,
        symbols: reduce_symbols(facts.symbols, &mut control)?,
        edges: reduce_edges(facts.edges, &mut control)?,
        references: reduce_references(facts.references, &mut control)?,
        numerical_sites: reduce_numerical_sites(facts.numerical_sites, &mut control)?,
        documents: reduce_documents(facts.documents, &mut control)?,
    };
    control.poll()?;
    Ok((tables, control.input_bytes))
}

pub(crate) fn validate_generation_facts_for_v1_import<Cancel>(
    facts: GenerationFacts,
    limits: GenerationValidationLimits,
    cancelled: Cancel,
) -> Result<(CanonicalGenerationFacts, GenerationValidationReport), GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    if !facts.numerical_sites.is_empty() {
        return Err(invalid("v1_import_numerical_sites").into());
    }
    validate_generation_facts_with_version(
        facts,
        GenerationValidationPolicy {
            limits,
            digest_version: GenerationDigestVersion::V6,
        },
        cancelled,
    )
}

#[derive(Clone, Copy)]
struct GenerationValidationPolicy {
    limits: GenerationValidationLimits,
    digest_version: GenerationDigestVersion,
}

fn validate_generation_facts_with_version<Cancel>(
    facts: GenerationFacts,
    policy: GenerationValidationPolicy,
    cancelled: Cancel,
) -> Result<(CanonicalGenerationFacts, GenerationValidationReport), GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    let mut control = ValidationControl::new(policy.limits, cancelled);
    control.admit(&facts)?;
    let tables = ValidatedFactTables {
        files: reduce_files(facts.files, &mut control)?,
        symbols: reduce_symbols(facts.symbols, &mut control)?,
        edges: reduce_edges(facts.edges, &mut control)?,
        references: reduce_references(facts.references, &mut control)?,
        numerical_sites: reduce_numerical_sites(facts.numerical_sites, &mut control)?,
        documents: reduce_documents(facts.documents, &mut control)?,
    };
    validate_relations(&tables, &mut control)?;
    control.poll()?;
    let digest = logical_digest(&tables, policy.digest_version, || (control.cancelled)())
        .map_err(|()| GenerationValidationError::Cancelled)?;
    control.charge(usize_to_u64(digest.as_str().len()))?;
    let facts = CanonicalGenerationFacts {
        tables,
        digest,
        digest_version: policy.digest_version,
    };
    let report = control.finish(&facts)?;
    Ok((facts, report))
}

fn reduce_files<Cancel>(
    files: Vec<FileInput>,
    control: &mut ValidationControl<Cancel>,
) -> Result<Vec<FileInput>, GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    let mut by_id = BTreeMap::<String, FileInput>::new();
    let mut paths = BTreeMap::<String, String>::new();
    for file in files {
        control.charge(
            MAP_NODE_ALLOWANCE
                .saturating_mul(2)
                .saturating_add(usize_to_u64(size_of::<FileInput>()))
                .saturating_add(usize_to_u64(size_of::<String>()))
                .saturating_add(usize_to_u64(file.file_id.as_str().len()).saturating_mul(3))
                .saturating_add(usize_to_u64(file.normalized_path.len())),
        )?;
        validate_project_path(&file.normalized_path, "normalized_path")?;
        validate_language(&file.language)?;
        if file.byte_size > MAX_DATABASE_BIGINT {
            return Err(invalid("byte_size").into());
        }
        let id = file.file_id.as_str().to_owned();
        match by_id.entry(id.clone()) {
            Entry::Occupied(entry) if entry.get() != &file => {
                return Err(invalid("duplicate_file_id").into());
            }
            Entry::Occupied(_) => continue,
            Entry::Vacant(_) => {}
        }
        match paths.entry(file.normalized_path.clone()) {
            Entry::Occupied(entry) if entry.get() != &id => {
                return Err(invalid("duplicate_normalized_path").into());
            }
            Entry::Occupied(_) => {}
            Entry::Vacant(entry) => {
                entry.insert(id.clone());
            }
        }
        by_id.insert(id, file);
    }
    collect_values(by_id, control)
}

fn reduce_symbols<Cancel>(
    symbols: Vec<SymbolInput>,
    control: &mut ValidationControl<Cancel>,
) -> Result<Vec<SymbolInput>, GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    let mut by_id = BTreeMap::<String, SymbolInput>::new();
    for symbol in symbols {
        control.charge(
            MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<SymbolInput>()))
                .saturating_add(usize_to_u64(symbol.symbol_id.as_str().len())),
        )?;
        validate_bounded_text(&symbol.symbol_kind, "symbol_kind", MAX_SYMBOL_KIND_BYTES)?;
        validate_bounded_text(
            &symbol.qualified_name,
            "qualified_name",
            MAX_QUALIFIED_NAME_BYTES,
        )?;
        validate_optional_text(&symbol.signature, "signature", MAX_SIGNATURE_BYTES)?;
        validate_span(symbol.start_byte, symbol.end_byte, "symbol_byte_span")?;
        validate_lines(symbol.start_line, symbol.end_line)?;
        if symbol
            .betweenness_ppb
            .is_some_and(|score| score > 1_000_000_000)
        {
            return Err(invalid("symbol_betweenness").into());
        }
        if symbol
            .pagerank_ppb
            .is_some_and(|score| score > 1_000_000_000)
        {
            return Err(invalid("symbol_pagerank").into());
        }
        insert_unique(
            &mut by_id,
            UniqueInput {
                key: symbol.symbol_id.as_str().to_owned(),
                value: symbol,
                field: "duplicate_symbol_id",
            },
        )?;
    }
    collect_values(by_id, control)
}

fn reduce_edges<Cancel>(
    edges: Vec<EdgeInput>,
    control: &mut ValidationControl<Cancel>,
) -> Result<Vec<EdgeInput>, GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    let mut by_key = BTreeMap::<(String, String, String, String), EdgeInput>::new();
    for mut edge in edges {
        control.charge(
            MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<EdgeInput>()))
                .saturating_add(usize_to_u64(edge.source_symbol_id.as_str().len()))
                .saturating_add(usize_to_u64(edge.target_symbol_id.as_str().len()))
                .saturating_add(usize_to_u64(edge.kind.as_str().len()))
                .saturating_add(usize_to_u64(edge.provenance.len())),
        )?;
        edge.confidence = validate_confidence(edge.confidence)?;
        if edge.site_count == 0 || edge.site_count > MAX_SITE_COUNT {
            return Err(invalid("edge_site_count").into());
        }
        validate_bounded_text(&edge.provenance, "provenance", MAX_PROVENANCE_BYTES)?;
        let key = (
            edge.source_symbol_id.as_str().to_owned(),
            edge.target_symbol_id.as_str().to_owned(),
            edge.kind.as_str().to_owned(),
            edge.provenance.clone(),
        );
        match by_key.entry(key) {
            Entry::Occupied(mut entry) => {
                let existing = entry.get_mut();
                existing.site_count = existing
                    .site_count
                    .checked_add(edge.site_count)
                    .filter(|count| *count <= MAX_SITE_COUNT)
                    .ok_or_else(|| invalid("edge_site_count"))?;
                existing.confidence = existing.confidence.max(edge.confidence);
            }
            Entry::Vacant(entry) => {
                entry.insert(edge);
            }
        }
    }
    collect_values(by_key, control)
}

fn reduce_references<Cancel>(
    references: Vec<ReferenceInput>,
    control: &mut ValidationControl<Cancel>,
) -> Result<Vec<ReferenceInput>, GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    let mut by_key = BTreeMap::<
        (
            String,
            Option<String>,
            Option<String>,
            String,
            String,
            u64,
            u64,
            String,
        ),
        ReferenceInput,
    >::new();
    for mut reference in references {
        control.charge(
            MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<ReferenceInput>()))
                .saturating_add(usize_to_u64(reference.file_id.as_str().len()))
                .saturating_add(
                    reference
                        .owner_symbol_id
                        .as_ref()
                        .map_or(0, |id| usize_to_u64(id.as_str().len())),
                )
                .saturating_add(
                    reference
                        .target_symbol_id
                        .as_ref()
                        .map_or(0, |id| usize_to_u64(id.as_str().len())),
                )
                .saturating_add(usize_to_u64(reference.reference_name.len()))
                .saturating_add(usize_to_u64(reference.reference_kind.len()))
                .saturating_add(usize_to_u64(reference.resolution_provenance.len())),
        )?;
        validate_reference_name(&reference.reference_name)?;
        validate_bounded_text(
            &reference.reference_kind,
            "reference_kind",
            MAX_REFERENCE_KIND_BYTES,
        )?;
        validate_bounded_text(
            &reference.resolution_provenance,
            "resolution_provenance",
            MAX_PROVENANCE_BYTES,
        )?;
        validate_span(
            reference.start_byte,
            reference.end_byte,
            "reference_byte_span",
        )?;
        if reference.site_count == 0 || reference.site_count > MAX_SITE_COUNT {
            return Err(invalid("reference_site_count").into());
        }
        reference.confidence = validate_confidence(reference.confidence)?;
        let key = (
            reference.file_id.as_str().to_owned(),
            reference
                .owner_symbol_id
                .as_ref()
                .map(|symbol| symbol.as_str().to_owned()),
            reference
                .target_symbol_id
                .as_ref()
                .map(|symbol| symbol.as_str().to_owned()),
            reference.reference_name.clone(),
            reference.reference_kind.clone(),
            reference.start_byte,
            reference.end_byte,
            reference.resolution_provenance.clone(),
        );
        insert_unique(
            &mut by_key,
            UniqueInput {
                key,
                value: reference,
                field: "duplicate_reference",
            },
        )?;
    }
    collect_values(by_key, control)
}

fn reduce_numerical_sites<Cancel>(
    sites: Vec<NumericalSiteInput>,
    control: &mut ValidationControl<Cancel>,
) -> Result<Vec<NumericalSiteInput>, GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    let mut by_id = BTreeMap::<String, NumericalSiteInput>::new();
    for site in sites {
        control.charge(
            MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<NumericalSiteInput>()))
                .saturating_add(usize_to_u64(site.site_id.as_str().len())),
        )?;
        validate_span(site.start_byte, site.end_byte, "numerical_site_byte_span")?;
        validate_lines(site.start_line, site.end_line)?;
        for (value, field) in [
            (&site.operation, "numerical_operation"),
            (&site.hazard, "numerical_hazard"),
            (&site.precision, "numerical_precision"),
            (&site.evidence_level, "numerical_evidence_level"),
        ] {
            validate_machine_token(value, field, MAX_NUMERICAL_CATEGORY_BYTES)?;
        }
        validate_machine_token(
            &site.provenance,
            "numerical_provenance",
            MAX_PROVENANCE_BYTES,
        )?;
        validate_optional_text(
            &site.unknowns,
            "numerical_unknowns",
            MAX_NUMERICAL_UNKNOWNS_BYTES,
        )?;
        if site.confidence_ppm > 1_000_000
            || !matches!(
                site.evidence_level.as_str(),
                "proven" | "heuristic" | "coverage_gap"
            )
            || !valid_machine_token_list(&site.unknowns)
        {
            return Err(invalid("numerical_site").into());
        }
        insert_unique(
            &mut by_id,
            UniqueInput {
                key: site.site_id.as_str().to_owned(),
                value: site,
                field: "duplicate_numerical_site_id",
            },
        )?;
    }
    collect_values(by_id, control)
}

fn reduce_documents<Cancel>(
    documents: Vec<SearchDocumentInput>,
    control: &mut ValidationControl<Cancel>,
) -> Result<Vec<CanonicalSearchDocument>, GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    let mut by_id = BTreeMap::<String, CanonicalSearchDocument>::new();
    for document in documents {
        control.poll()?;
        let validated = validate_document(document, control)?;
        control.charge(
            MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<CanonicalSearchDocument>()))
                .saturating_add(usize_to_u64(validated.document_id.as_str().len())),
        )?;
        insert_unique(
            &mut by_id,
            UniqueInput {
                key: validated.document_id.as_str().to_owned(),
                value: validated,
                field: "duplicate_document_id",
            },
        )?;
    }
    collect_values(by_id, control)
}

fn collect_values<Key, Value, Cancel>(
    values: BTreeMap<Key, Value>,
    control: &mut ValidationControl<Cancel>,
) -> Result<Vec<Value>, GenerationValidationError>
where
    Key: Ord,
    Cancel: FnMut() -> bool,
{
    let requested_bytes = vector_bytes::<Value>(values.len());
    control.charge(requested_bytes)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(values.len())
        .map_err(|_| GenerationValidationError::RetainedLimit)?;
    let actual_bytes = vector_capacity_bytes(&output);
    control.charge(actual_bytes.saturating_sub(requested_bytes))?;
    for (_, value) in values {
        control.poll()?;
        output.push(value);
    }
    Ok(output)
}

fn validate_document<Cancel>(
    document: SearchDocumentInput,
    control: &mut ValidationControl<Cancel>,
) -> Result<CanonicalSearchDocument, GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    validate_project_path(&document.path, "path")?;
    validate_language(&document.language)?;
    validate_optional_text(
        &document.qualified_name,
        "qualified_name",
        MAX_QUALIFIED_NAME_BYTES,
    )?;
    validate_optional_text(&document.code, "code", MAX_CODE_BYTES)?;
    validate_optional_text(
        &document.natural_text,
        "natural_text",
        MAX_NATURAL_TEXT_BYTES,
    )?;
    if document.qualified_name.is_empty()
        && document.code.is_empty()
        && document.natural_text.is_empty()
    {
        return Err(invalid("searchable_text").into());
    }
    if !document.metadata.is_object() {
        return Err(invalid("metadata").into());
    }
    let metadata_json = canonical_json(&document.metadata, control)?;
    if metadata_json.len() > MAX_METADATA_BYTES || metadata_json.contains('\0') {
        return Err(invalid("metadata").into());
    }
    Ok(CanonicalSearchDocument {
        document_id: document.document_id,
        file_id: document.file_id,
        symbol_id: document.symbol_id,
        path: document.path,
        language: document.language,
        kind: document.kind,
        qualified_name: document.qualified_name,
        code: document.code,
        natural_text: document.natural_text,
        metadata_json,
    })
}

fn validate_relations<Cancel>(
    facts: &ValidatedFactTables,
    control: &mut ValidationControl<Cancel>,
) -> Result<(), GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    let mut files = BTreeMap::new();
    for file in &facts.files {
        control.charge(
            MAP_NODE_ALLOWANCE.saturating_add(usize_to_u64(size_of::<(&str, &FileInput)>())),
        )?;
        files.insert(file.file_id.as_str(), file);
    }
    let mut symbols = BTreeMap::new();
    for symbol in &facts.symbols {
        control.charge(
            MAP_NODE_ALLOWANCE.saturating_add(usize_to_u64(size_of::<(&str, &SymbolInput)>())),
        )?;
        symbols.insert(symbol.symbol_id.as_str(), symbol);
    }
    for symbol in &facts.symbols {
        control.poll()?;
        let file = require_file(&files, &symbol.file_id, "symbol_file_id")?;
        require_structural_file(file, "symbol_file_parse_status")?;
        require_within_file(symbol.end_byte, file, "symbol_byte_span")?;
    }
    for edge in &facts.edges {
        control.poll()?;
        require_symbol(&symbols, &edge.source_symbol_id, "edge_source_symbol_id")?;
        require_symbol(&symbols, &edge.target_symbol_id, "edge_target_symbol_id")?;
    }
    for reference in &facts.references {
        control.poll()?;
        validate_reference_relations(reference, &files, &symbols)?;
    }
    for site in &facts.numerical_sites {
        control.poll()?;
        validate_numerical_site_relations(site, &files, &symbols)?;
    }
    for document in &facts.documents {
        control.poll()?;
        validate_document_relations(document, &files, &symbols)?;
    }
    Ok(())
}

fn validate_reference_relations(
    reference: &ReferenceInput,
    files: &BTreeMap<&str, &FileInput>,
    symbols: &BTreeMap<&str, &SymbolInput>,
) -> Result<(), GenerationValidationError> {
    let file = require_file(files, &reference.file_id, "reference_file_id")?;
    require_structural_file(file, "reference_file_parse_status")?;
    require_within_file(reference.end_byte, file, "reference_byte_span")?;
    if let Some(owner) = &reference.owner_symbol_id {
        let owner = require_symbol(symbols, owner, "reference_owner_symbol_id")?;
        if owner.file_id != reference.file_id {
            return Err(invalid("reference_owner_file_id").into());
        }
    }
    if let Some(symbol) = &reference.target_symbol_id {
        require_symbol(symbols, symbol, "reference_target_symbol_id")?;
    }
    Ok(())
}

fn validate_numerical_site_relations(
    site: &NumericalSiteInput,
    files: &BTreeMap<&str, &FileInput>,
    symbols: &BTreeMap<&str, &SymbolInput>,
) -> Result<(), GenerationValidationError> {
    let file = require_file(files, &site.file_id, "numerical_site_file_id")?;
    require_structural_file(file, "numerical_site_file_parse_status")?;
    require_within_file(site.end_byte, file, "numerical_site_byte_span")?;
    if let Some(owner) = &site.owner_symbol_id {
        let owner = require_symbol(symbols, owner, "numerical_site_owner_symbol_id")?;
        if owner.file_id != site.file_id
            || site.start_byte < owner.start_byte
            || site.end_byte > owner.end_byte
        {
            return Err(invalid("numerical_site_owner_span").into());
        }
    }
    Ok(())
}

fn validate_document_relations(
    document: &CanonicalSearchDocument,
    files: &BTreeMap<&str, &FileInput>,
    symbols: &BTreeMap<&str, &SymbolInput>,
) -> Result<(), StorageError> {
    let explicit_file = document
        .file_id
        .as_ref()
        .map(|file| require_file(files, file, "document_file_id"))
        .transpose()?;
    let symbol_file = document
        .symbol_id
        .as_ref()
        .map(|symbol| {
            require_symbol(symbols, symbol, "document_symbol_id")
                .and_then(|symbol| require_file(files, &symbol.file_id, "document_symbol_file"))
        })
        .transpose()?;
    if let (Some(explicit), Some(symbol)) = (explicit_file, symbol_file)
        && explicit.file_id != symbol.file_id
    {
        return Err(invalid("document_symbol_file"));
    }
    if let Some(file) = explicit_file.or(symbol_file) {
        if document.path != file.normalized_path {
            return Err(invalid("document_path"));
        }
        if document.language != file.language {
            return Err(invalid("document_language"));
        }
    }
    Ok(())
}

fn require_file<'a>(
    files: &BTreeMap<&str, &'a FileInput>,
    file_id: &FileId,
    field: &'static str,
) -> Result<&'a FileInput, StorageError> {
    files.get(file_id.as_str()).copied().ok_or(invalid(field))
}

fn require_symbol<'a>(
    symbols: &BTreeMap<&str, &'a SymbolInput>,
    symbol_id: &SymbolId,
    field: &'static str,
) -> Result<&'a SymbolInput, StorageError> {
    symbols
        .get(symbol_id.as_str())
        .copied()
        .ok_or(invalid(field))
}

fn require_within_file(
    end_byte: u64,
    file: &FileInput,
    field: &'static str,
) -> Result<(), StorageError> {
    (end_byte <= file.byte_size).ok_or(invalid(field))
}

fn require_structural_file(file: &FileInput, field: &'static str) -> Result<(), StorageError> {
    match file.parse_status {
        FileParseStatus::Parsed | FileParseStatus::Partial => Ok(()),
        FileParseStatus::Failed | FileParseStatus::Skipped => Err(invalid(field)),
    }
}

fn insert_unique<K, V>(
    values: &mut BTreeMap<K, V>,
    input: UniqueInput<K, V>,
) -> Result<(), StorageError>
where
    K: Ord,
    V: PartialEq,
{
    match values.entry(input.key) {
        Entry::Occupied(entry) if entry.get() != &input.value => Err(invalid(input.field)),
        Entry::Occupied(_) => Ok(()),
        Entry::Vacant(entry) => {
            entry.insert(input.value);
            Ok(())
        }
    }
}

struct UniqueInput<K, V> {
    key: K,
    value: V,
    field: &'static str,
}

fn vector_bytes<T>(length: usize) -> u64 {
    usize_to_u64(length).saturating_mul(usize_to_u64(size_of::<T>()))
}

fn vector_capacity_bytes<T>(values: &Vec<T>) -> u64 {
    usize_to_u64(values.capacity()).saturating_mul(usize_to_u64(size_of::<T>()))
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn validate_project_path(value: &str, field: &'static str) -> Result<(), StorageError> {
    validate_bounded_text(value, field, MAX_PATH_BYTES)?;
    let has_drive_prefix = value.as_bytes().get(1).copied() == Some(b':');
    let invalid_segment = value
        .split('/')
        .any(|segment| segment.is_empty() || matches!(segment, "." | ".."));
    if value.starts_with('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || has_drive_prefix
        || invalid_segment
    {
        return Err(invalid(field));
    }
    Ok(())
}

fn validate_language(value: &str) -> Result<(), StorageError> {
    validate_bounded_text(value, "language", MAX_LANGUAGE_BYTES)?;
    if value.bytes().all(|byte| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'_' | b'+' | b'.' | b'-')
    }) {
        Ok(())
    } else {
        Err(invalid("language"))
    }
}

fn validate_span(start: u64, end: u64, field: &'static str) -> Result<(), StorageError> {
    if start > MAX_DATABASE_BIGINT || end > MAX_DATABASE_BIGINT || end < start {
        Err(invalid(field))
    } else {
        Ok(())
    }
}

fn validate_lines(start: u32, end: u32) -> Result<(), StorageError> {
    if start == 0 || end < start || end > MAX_DATABASE_INTEGER {
        Err(invalid("symbol_line_span"))
    } else {
        Ok(())
    }
}

fn validate_confidence(confidence: f32) -> Result<f32, StorageError> {
    if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
        return Err(invalid("confidence"));
    }
    Ok(if confidence == 0.0 { 0.0 } else { confidence })
}

fn validate_bounded_text(
    value: &str,
    field: &'static str,
    maximum: usize,
) -> Result<(), StorageError> {
    if value.trim().is_empty() || value.len() > maximum || value.contains('\0') {
        Err(invalid(field))
    } else {
        Ok(())
    }
}

fn validate_reference_name(value: &str) -> Result<(), GenerationValidationError> {
    if value.len() > MAX_REFERENCE_NAME_BYTES {
        Err(GenerationValidationError::ReferenceNameTooLong)
    } else {
        validate_bounded_text(value, "reference_name", MAX_REFERENCE_NAME_BYTES).map_err(Into::into)
    }
}

fn validate_optional_text(
    value: &str,
    field: &'static str,
    maximum: usize,
) -> Result<(), StorageError> {
    if value.len() > maximum || value.contains('\0') {
        Err(invalid(field))
    } else {
        Ok(())
    }
}

fn validate_machine_token(
    value: &str,
    field: &'static str,
    maximum: usize,
) -> Result<(), StorageError> {
    validate_bounded_text(value, field, maximum)?;
    if value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        Ok(())
    } else {
        Err(invalid(field))
    }
}

fn valid_machine_token_list(value: &str) -> bool {
    value.is_empty()
        || value.split(',').all(|token| {
            !token.is_empty()
                && token
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
}

fn canonical_json<Cancel>(
    value: &Value,
    control: &mut ValidationControl<Cancel>,
) -> Result<String, GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    let mut output = BoundedJsonWriter::new(control);
    write_canonical_json(value, &mut output, 0)?;
    String::from_utf8(output.into_bytes()).map_err(|_| invalid("metadata").into())
}

pub(crate) fn canonical_stored_metadata(
    value: &Value,
) -> Result<String, GenerationValidationError> {
    let limits = GenerationValidationLimits {
        maximum_output_bytes: usize_to_u64(MAX_METADATA_BYTES),
        maximum_working_bytes: usize_to_u64(MAX_METADATA_BYTES).saturating_mul(8),
    };
    let mut control = ValidationControl::new(limits, || false);
    canonical_json(value, &mut control)
}

fn write_canonical_json<Cancel>(
    value: &Value,
    output: &mut BoundedJsonWriter<'_, Cancel>,
    depth: usize,
) -> Result<(), GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    output.poll()?;
    if depth > MAX_METADATA_DEPTH {
        return Err(invalid("metadata_depth").into());
    }
    match value {
        Value::Null => write_fragment(output, b"null")?,
        Value::Bool(boolean) => {
            write_fragment(output, if *boolean { b"true" } else { b"false" })?;
        }
        Value::Number(number) => write_json_value(output, number)?,
        Value::String(string) => write_json_value(output, string)?,
        Value::Array(values) => write_canonical_array(values, output, depth)?,
        Value::Object(values) => write_canonical_object(values, output, depth)?,
    }
    Ok(())
}

fn write_canonical_array<Cancel>(
    values: &[Value],
    output: &mut BoundedJsonWriter<'_, Cancel>,
    depth: usize,
) -> Result<(), GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    write_fragment(output, b"[")?;
    for (index, value) in values.iter().enumerate() {
        output.poll()?;
        if index > 0 {
            write_fragment(output, b",")?;
        }
        write_canonical_json(value, output, depth + 1)?;
    }
    write_fragment(output, b"]")
}

fn write_canonical_object<Cancel>(
    values: &serde_json::Map<String, Value>,
    output: &mut BoundedJsonWriter<'_, Cancel>,
    depth: usize,
) -> Result<(), GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    let maximum_entries = MAX_METADATA_BYTES / MIN_CANONICAL_OBJECT_ENTRY_BYTES;
    if values.len() > maximum_entries {
        return Err(invalid("metadata").into());
    }
    let requested_bytes = vector_bytes::<(&String, &Value)>(values.len());
    output.charge(requested_bytes)?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(values.len())
        .map_err(|_| GenerationValidationError::RetainedLimit)?;
    output.charge(vector_capacity_bytes(&entries).saturating_sub(requested_bytes))?;
    for entry in values {
        output.poll()?;
        entries.push(entry);
    }
    entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
    write_fragment(output, b"{")?;
    for (index, (key, value)) in entries.into_iter().enumerate() {
        output.poll()?;
        if index > 0 {
            write_fragment(output, b",")?;
        }
        write_json_value(output, key)?;
        write_fragment(output, b":")?;
        write_canonical_json(value, output, depth + 1)?;
    }
    write_fragment(output, b"}")
}

fn write_json_value<T: serde::Serialize + ?Sized, Cancel>(
    output: &mut BoundedJsonWriter<'_, Cancel>,
    value: &T,
) -> Result<(), GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    serde_json::to_writer(output, value).map_err(|_| invalid("metadata").into())
}

fn write_fragment<Cancel>(
    output: &mut BoundedJsonWriter<'_, Cancel>,
    fragment: &[u8],
) -> Result<(), GenerationValidationError>
where
    Cancel: FnMut() -> bool,
{
    output
        .write_all(fragment)
        .map_err(|_| invalid("metadata").into())
}

struct BoundedJsonWriter<'a, Cancel> {
    bytes: Vec<u8>,
    control: &'a mut ValidationControl<Cancel>,
}

impl<'a, Cancel> BoundedJsonWriter<'a, Cancel>
where
    Cancel: FnMut() -> bool,
{
    fn new(control: &'a mut ValidationControl<Cancel>) -> Self {
        Self {
            bytes: Vec::new(),
            control,
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    fn poll(&mut self) -> Result<(), GenerationValidationError> {
        self.control.poll()
    }

    fn charge(&mut self, bytes: u64) -> Result<(), GenerationValidationError> {
        self.control.charge(bytes)
    }
}

impl<Cancel> Write for BoundedJsonWriter<'_, Cancel>
where
    Cancel: FnMut() -> bool,
{
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > MAX_METADATA_BYTES {
            return Err(io::Error::other(
                "canonical metadata exceeds its byte budget",
            ));
        }
        let previous_capacity = self.bytes.capacity();
        self.bytes
            .try_reserve_exact(bytes.len())
            .map_err(|_| io::Error::other("canonical metadata allocation failed"))?;
        let added_capacity = self.bytes.capacity().saturating_sub(previous_capacity);
        self.control
            .charge(usize_to_u64(added_capacity))
            .map_err(|_| io::Error::other("canonical metadata validation stopped"))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

const fn invalid(field: &'static str) -> StorageError {
    StorageError::InvalidInput { field }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use cartograph_domain::{
        ContentDigest, DocumentId, DocumentKind, FileParseStatus, NumericalSiteId,
    };
    use std::cell::Cell;

    const TEST_DIGEST_BYTE: u8 = 0x11;
    const TEST_DIGEST_LENGTH: usize = 32;
    const TEST_FILE_SIZE: u64 = 10;
    const TEST_SYMBOL_END: u64 = TEST_FILE_SIZE;
    const MODEL_SCAN_FILE_COUNT: usize = 1_024;
    const MODEL_SCAN_CANCELLATION_POLLS: u64 = 32;
    const MAP_VALUE_COUNT: u32 = 128;
    const RELATION_FILE_COUNT: u8 = 64;
    const RELATION_CANCELLATION_POLLS: u64 = 16;
    const UUID_BYTE_LENGTH: usize = 16;
    const UUID_SUFFIX_INDEX: usize = UUID_BYTE_LENGTH - 1;
    const FIRST_UUID_SUFFIX: u8 = 1;
    const POLL_INCREMENT: u64 = 1;
    const VALIDATION_OUTPUT_WORKING_RATIO: u64 = 4;
    const BASE_EDGE_CONFIDENCE: f32 = 0.4;
    const MAXIMUM_EDGE_CONFIDENCE: f32 = 0.9;
    const BASE_EDGE_SITE_COUNT: u32 = 2;
    const REPEATED_EDGE_SITE_COUNT: u32 = 3;
    const REDUCED_EDGE_SITE_COUNT: u32 = 5;
    const CHANGED_EDGE_SITE_COUNT: u32 = 6;

    fn digest() -> ContentDigest {
        ContentDigest::from_bytes([TEST_DIGEST_BYTE; TEST_DIGEST_LENGTH])
    }

    fn file() -> FileInput {
        FileInput {
            file_id: file_id(),
            normalized_path: "src/lib.rs".to_owned(),
            language: "rust".to_owned(),
            content_hash: digest(),
            byte_size: TEST_FILE_SIZE,
            parse_status: FileParseStatus::Parsed,
        }
    }

    fn file_id() -> FileId {
        match FileId::parse("11111111-1111-4111-8111-111111111111") {
            Ok(id) => id,
            Err(error) => panic!("fixture file ID is invalid: {error}"),
        }
    }

    fn document_id() -> cartograph_domain::DocumentId {
        match DocumentId::parse("22222222-2222-4222-8222-222222222222") {
            Ok(id) => id,
            Err(error) => panic!("fixture document ID is invalid: {error}"),
        }
    }

    fn symbol_id() -> SymbolId {
        match SymbolId::parse("33333333-3333-4333-8333-333333333333") {
            Ok(id) => id,
            Err(error) => panic!("fixture symbol ID is invalid: {error}"),
        }
    }

    fn symbol() -> SymbolInput {
        SymbolInput {
            symbol_id: symbol_id(),
            file_id: file_id(),
            symbol_kind: "function".to_owned(),
            qualified_name: "fixture::symbol".to_owned(),
            signature: "fn symbol()".to_owned(),
            start_byte: 0,
            end_byte: TEST_SYMBOL_END,
            start_line: 1,
            end_line: 1,
            structural_digest: digest(),
            visibility: None,
            export: cartograph_domain::SymbolExportFlags::default(),
            execution: cartograph_domain::SymbolExecutionFlags::default(),
            declaration_only: false,
            betweenness_ppb: None,
            pagerank_ppb: None,
        }
    }

    fn reference() -> ReferenceInput {
        ReferenceInput {
            file_id: file_id(),
            owner_symbol_id: None,
            target_symbol_id: None,
            reference_name: "fixture_call".to_owned(),
            reference_kind: "call".to_owned(),
            start_byte: 0,
            end_byte: TEST_FILE_SIZE,
            confidence: 0.0,
            resolution_provenance: "test-unresolved".to_owned(),
            site_count: 1,
            span_precision: super::super::ReferenceSpanPrecision::Exact,
        }
    }

    fn numerical_site() -> NumericalSiteInput {
        NumericalSiteInput {
            site_id: NumericalSiteId::parse("44444444-4444-4444-8444-444444444444")
                .unwrap_or_else(|error| panic!("fixture numerical site ID is invalid: {error}")),
            file_id: file_id(),
            owner_symbol_id: Some(symbol_id()),
            start_byte: 1,
            end_byte: 5,
            start_line: 1,
            end_line: 1,
            operation: "multiplication".to_owned(),
            hazard: "arithmetic_before_widening".to_owned(),
            precision: "f32".to_owned(),
            expression_digest: digest(),
            confidence_ppm: 900_000,
            provenance: "rust_ast_v1".to_owned(),
            evidence_level: "heuristic".to_owned(),
            unknowns: "operand_precision,overflow_or_rounding".to_owned(),
        }
    }

    fn document() -> SearchDocumentInput {
        SearchDocumentInput {
            document_id: document_id(),
            file_id: Some(file_id()),
            symbol_id: None,
            path: "src/lib.rs".to_owned(),
            language: "rust".to_owned(),
            kind: DocumentKind::File,
            qualified_name: String::new(),
            code: "fn main() {}".to_owned(),
            natural_text: String::new(),
            metadata: serde_json::json!({"z": 1, "a": {"y": 2, "b": 3}}),
        }
    }

    fn validation_limits() -> GenerationValidationLimits {
        GenerationValidationLimits::new(
            MAX_VALIDATION_RETAINED_BYTES / VALIDATION_OUTPUT_WORKING_RATIO,
            MAX_VALIDATION_RETAINED_BYTES,
        )
        .unwrap_or_else(|error| panic!("test validation limits were invalid: {error}"))
    }

    #[test]
    fn reduction_deduplicates_equal_facts_but_rejects_conflicting_identity() {
        let base = file();
        let equal = base.clone();
        let facts = GenerationFacts {
            files: vec![base.clone(), equal],
            ..GenerationFacts::default()
        };
        assert_matches!(
            validate_and_reduce(facts).map(|facts| facts.tables.files.len()),
            Ok(1)
        );

        let mut conflicting = base.clone();
        conflicting.normalized_path = "src/other.rs".to_owned();
        let facts = GenerationFacts {
            files: vec![base, conflicting],
            ..GenerationFacts::default()
        };
        assert_matches!(
            validate_and_reduce(facts).err(),
            Some(StorageError::InvalidInput {
                field: "duplicate_file_id"
            })
        );
    }

    #[test]
    fn validation_rejects_unresolved_relations_and_canonicalizes_metadata() {
        let document = document();
        let missing_file = GenerationFacts {
            documents: vec![document.clone()],
            ..GenerationFacts::default()
        };
        assert_matches!(
            validate_and_reduce(missing_file).err(),
            Some(StorageError::InvalidInput {
                field: "document_file_id"
            })
        );

        let facts = GenerationFacts {
            files: vec![file()],
            documents: vec![document],
            ..GenerationFacts::default()
        };
        let validated = match validate_and_reduce(facts) {
            Ok(validated) => validated,
            Err(error) => panic!("valid fact set was rejected: {error}"),
        };
        assert_eq!(
            validated.tables.documents[0].metadata_json,
            r#"{"a":{"b":3,"y":2},"z":1}"#
        );
    }

    #[test]
    fn validation_rejects_spans_outside_the_owning_file() {
        let mut outside_symbol = symbol();
        outside_symbol.end_byte = TEST_FILE_SIZE + 1;
        let facts = GenerationFacts {
            files: vec![file()],
            symbols: vec![outside_symbol],
            ..GenerationFacts::default()
        };
        assert_matches!(
            validate_and_reduce(facts).err(),
            Some(StorageError::InvalidInput {
                field: "symbol_byte_span"
            })
        );

        let facts = GenerationFacts {
            files: vec![file()],
            references: vec![ReferenceInput {
                end_byte: TEST_FILE_SIZE + 1,
                ..reference()
            }],
            ..GenerationFacts::default()
        };
        assert_matches!(
            validate_and_reduce(facts).err(),
            Some(StorageError::InvalidInput {
                field: "reference_byte_span"
            })
        );
    }

    #[test]
    fn reference_name_validation_distinguishes_too_long_from_other_invalid_text() {
        let validate = |name: String| {
            let mut candidate = reference();
            candidate.reference_name = name;
            validate_generation_facts(
                GenerationFacts {
                    files: vec![file()],
                    references: vec![candidate],
                    ..GenerationFacts::default()
                },
                validation_limits(),
                || false,
            )
        };

        assert!(validate("x".repeat(MAX_REFERENCE_NAME_BYTES)).is_ok());
        assert_matches!(
            validate("x".repeat(MAX_REFERENCE_NAME_BYTES + 1)).err(),
            Some(GenerationValidationError::ReferenceNameTooLong)
        );
        for invalid_name in [String::new(), "valid\0name".to_owned()] {
            assert_matches!(
                validate(invalid_name).err(),
                Some(GenerationValidationError::Storage(
                    StorageError::InvalidInput {
                        field: "reference_name"
                    }
                ))
            );
        }
    }

    #[test]
    fn validation_rejects_structural_facts_for_unparsed_files() {
        let mut failed = file();
        failed.parse_status = FileParseStatus::Failed;
        let facts = GenerationFacts {
            files: vec![failed],
            symbols: vec![symbol()],
            ..GenerationFacts::default()
        };
        assert_matches!(
            validate_and_reduce(facts).err(),
            Some(StorageError::InvalidInput {
                field: "symbol_file_parse_status"
            })
        );

        let mut skipped = file();
        skipped.parse_status = FileParseStatus::Skipped;
        let facts = GenerationFacts {
            files: vec![skipped],
            references: vec![reference()],
            ..GenerationFacts::default()
        };
        assert_matches!(
            validate_and_reduce(facts).err(),
            Some(StorageError::InvalidInput {
                field: "reference_file_parse_status"
            })
        );

        let mut partial = file();
        partial.parse_status = FileParseStatus::Partial;
        let facts = GenerationFacts {
            files: vec![partial],
            symbols: vec![symbol()],
            references: vec![reference()],
            ..GenerationFacts::default()
        };
        assert!(validate_and_reduce(facts).is_ok());
    }

    #[test]
    fn numerical_sites_are_relationship_strict_and_part_of_digest_v7() {
        let site = numerical_site();
        let canonical = validate_and_reduce(GenerationFacts {
            files: vec![file()],
            symbols: vec![symbol()],
            numerical_sites: vec![site.clone(), site.clone()],
            ..GenerationFacts::default()
        })
        .unwrap_or_else(|error| panic!("numerical site was rejected: {error}"));
        assert_eq!(canonical.numerical_sites(), std::slice::from_ref(&site));
        assert_eq!(canonical.digest_version(), GenerationDigestVersion::CURRENT);

        let without_site = validate_and_reduce(GenerationFacts {
            files: vec![file()],
            symbols: vec![symbol()],
            ..GenerationFacts::default()
        })
        .unwrap_or_else(|error| panic!("site-free generation was rejected: {error}"));
        assert_ne!(canonical.digest(), without_site.digest());

        let mut changed = site.clone();
        changed.hazard = "none_observed".to_owned();
        let changed = validate_and_reduce(GenerationFacts {
            files: vec![file()],
            symbols: vec![symbol()],
            numerical_sites: vec![changed],
            ..GenerationFacts::default()
        })
        .unwrap_or_else(|error| panic!("changed numerical site was rejected: {error}"));
        assert_ne!(canonical.digest(), changed.digest());

        let mut outside_owner = symbol();
        outside_owner.start_byte = 2;
        assert_matches!(
            validate_and_reduce(GenerationFacts {
                files: vec![file()],
                symbols: vec![outside_owner],
                numerical_sites: vec![site.clone()],
                ..GenerationFacts::default()
            })
            .err(),
            Some(StorageError::InvalidInput {
                field: "numerical_site_owner_span"
            })
        );

        let mut invalid_unknowns = site;
        invalid_unknowns.unknowns = "operand_precision,".to_owned();
        assert_matches!(
            validate_and_reduce(GenerationFacts {
                files: vec![file()],
                symbols: vec![symbol()],
                numerical_sites: vec![invalid_unknowns],
                ..GenerationFacts::default()
            })
            .err(),
            Some(StorageError::InvalidInput {
                field: "numerical_site"
            })
        );
    }

    #[test]
    fn v1_import_validation_remains_digest_v6_and_refuses_new_fact_families() {
        let (legacy, _) = validate_generation_facts_for_v1_import(
            GenerationFacts {
                files: vec![file()],
                ..GenerationFacts::default()
            },
            validation_limits(),
            || false,
        )
        .unwrap_or_else(|error| panic!("legacy fact mapping was rejected: {error}"));
        assert_eq!(legacy.digest_version(), GenerationDigestVersion::V6);

        assert_matches!(
            validate_generation_facts_for_v1_import(
                GenerationFacts {
                    files: vec![file()],
                    symbols: vec![symbol()],
                    numerical_sites: vec![numerical_site()],
                    ..GenerationFacts::default()
                },
                validation_limits(),
                || false,
            )
            .err(),
            Some(GenerationValidationError::Storage(
                StorageError::InvalidInput {
                    field: "v1_import_numerical_sites"
                }
            ))
        );
    }

    #[test]
    fn validation_rejects_document_identity_that_contradicts_its_file() {
        let mut wrong_path = document();
        wrong_path.path = "src/other.rs".to_owned();
        let facts = GenerationFacts {
            files: vec![file()],
            documents: vec![wrong_path],
            ..GenerationFacts::default()
        };
        assert_matches!(
            validate_and_reduce(facts).err(),
            Some(StorageError::InvalidInput {
                field: "document_path"
            })
        );

        let mut symbol_document = document();
        symbol_document.file_id = None;
        symbol_document.symbol_id = Some(symbol_id());
        symbol_document.language = "typescript".to_owned();
        let facts = GenerationFacts {
            files: vec![file()],
            symbols: vec![symbol()],
            documents: vec![symbol_document],
            ..GenerationFacts::default()
        };
        assert_matches!(
            validate_and_reduce(facts).err(),
            Some(StorageError::InvalidInput {
                field: "document_language"
            })
        );
    }

    #[test]
    fn search_document_validation_preserves_bounded_searchable_contract() {
        let mut non_object = document();
        non_object.metadata = serde_json::json!(["not", "an", "object"]);
        assert_matches!(
            validate_test_document(non_object),
            Err(GenerationValidationError::Storage(
                StorageError::InvalidInput { field: "metadata" }
            ))
        );

        let mut unbounded = document();
        unbounded.code = "x".repeat(MAX_CODE_BYTES + 1);
        assert_matches!(
            validate_test_document(unbounded),
            Err(GenerationValidationError::Storage(
                StorageError::InvalidInput { field: "code" }
            ))
        );

        let mut oversized_metadata = document();
        oversized_metadata.metadata = serde_json::json!({"large": "x".repeat(MAX_METADATA_BYTES)});
        assert_matches!(
            validate_test_document(oversized_metadata),
            Err(GenerationValidationError::Storage(
                StorageError::InvalidInput { field: "metadata" }
            ))
        );

        let mut wide = serde_json::Map::new();
        let excessive_entries = MAX_METADATA_BYTES / MIN_CANONICAL_OBJECT_ENTRY_BYTES + 1;
        for index in 0..excessive_entries {
            wide.insert(index.to_string(), Value::Null);
        }
        let mut wide_metadata = document();
        wide_metadata.metadata = Value::Object(wide);
        assert_matches!(
            validate_test_document(wide_metadata),
            Err(GenerationValidationError::Storage(
                StorageError::InvalidInput { field: "metadata" }
            ))
        );

        let mut empty = document();
        empty.code.clear();
        assert_matches!(
            validate_test_document(empty),
            Err(GenerationValidationError::Storage(
                StorageError::InvalidInput {
                    field: "searchable_text"
                }
            ))
        );
    }

    #[test]
    fn retained_model_counts_spare_outer_and_json_array_capacity() {
        const FILE_CAPACITY: usize = 16_384;
        const JSON_CAPACITY: usize = 65_536;

        let mut files = Vec::with_capacity(FILE_CAPACITY);
        files.push(file());
        let mut values = Vec::with_capacity(JSON_CAPACITY);
        values.push(Value::Null);
        let mut metadata = serde_json::Map::new();
        metadata.insert("values".to_owned(), Value::Array(values));
        let mut search_document = document();
        search_document.metadata = Value::Object(metadata);
        let facts = GenerationFacts {
            files,
            documents: vec![search_document],
            ..GenerationFacts::default()
        };

        let minimum_capacity_bytes = usize_to_u64(FILE_CAPACITY)
            .saturating_mul(usize_to_u64(size_of::<FileInput>()))
            .saturating_add(
                usize_to_u64(JSON_CAPACITY).saturating_mul(usize_to_u64(size_of::<Value>())),
            );
        let measurement = facts
            .measure_retained_bytes(MAX_VALIDATION_RETAINED_BYTES, || false)
            .unwrap_or_else(|error| panic!("capacity-aware model failed: {error:?}"));
        assert!(measurement.retained_bytes() >= minimum_capacity_bytes);
    }

    #[test]
    fn retained_model_observes_mid_scan_cancellation() {
        let facts = GenerationFacts {
            files: vec![file(); MODEL_SCAN_FILE_COUNT],
            ..GenerationFacts::default()
        };
        let limits = validation_limits();
        let polls = Cell::<u64>::default();
        let result = validate_generation_facts(facts, limits, || {
            let next = polls.get().saturating_add(POLL_INCREMENT);
            polls.set(next);
            next >= MODEL_SCAN_CANCELLATION_POLLS
        });
        assert_matches!(result.err(), Some(GenerationValidationError::Cancelled));
        assert_eq!(polls.get(), MODEL_SCAN_CANCELLATION_POLLS);
    }

    #[test]
    fn map_collection_and_relation_map_build_observe_mid_scan_cancellation() {
        let limits = validation_limits();
        let values = (u32::MIN..MAP_VALUE_COUNT)
            .map(|value| (value, value))
            .collect();
        let collection_polls = Cell::<u64>::default();
        let mut collection_control = ValidationControl::new(limits, || {
            let next = collection_polls.get().saturating_add(POLL_INCREMENT);
            collection_polls.set(next);
            next >= RELATION_CANCELLATION_POLLS
        });
        assert_matches!(
            collect_values(values, &mut collection_control),
            Err(GenerationValidationError::Cancelled)
        );
        assert_eq!(collection_polls.get(), RELATION_CANCELLATION_POLLS);

        let files = (u8::MIN..RELATION_FILE_COUNT)
            .map(|suffix| {
                let mut bytes = [u8::MIN; UUID_BYTE_LENGTH];
                bytes[UUID_SUFFIX_INDEX] = suffix.saturating_add(FIRST_UUID_SUFFIX);
                FileInput {
                    file_id: FileId::from_uuid_v8(bytes),
                    normalized_path: format!("src/{suffix}.rs"),
                    ..file()
                }
            })
            .collect();
        let tables = ValidatedFactTables {
            files,
            symbols: Vec::new(),
            edges: Vec::new(),
            references: Vec::new(),
            numerical_sites: Vec::new(),
            documents: Vec::new(),
        };
        let relation_polls = Cell::<u64>::default();
        let mut relation_control = ValidationControl::new(limits, || {
            let next = relation_polls.get().saturating_add(POLL_INCREMENT);
            relation_polls.set(next);
            next >= RELATION_CANCELLATION_POLLS
        });
        assert_matches!(
            validate_relations(&tables, &mut relation_control),
            Err(GenerationValidationError::Cancelled)
        );
        assert_eq!(relation_polls.get(), RELATION_CANCELLATION_POLLS);
    }

    #[test]
    fn every_canonical_table_observes_cancellation_before_reduction_work() {
        let limits = GenerationValidationLimits::new(
            MAX_VALIDATION_RETAINED_BYTES / 4,
            MAX_VALIDATION_RETAINED_BYTES,
        )
        .unwrap_or_else(|error| panic!("test validation limits were invalid: {error}"));

        let mut files_control = ValidationControl::new(limits, || true);
        assert_matches!(
            reduce_files(vec![file()], &mut files_control),
            Err(GenerationValidationError::Cancelled)
        );

        let mut symbols_control = ValidationControl::new(limits, || true);
        assert_matches!(
            reduce_symbols(vec![symbol()], &mut symbols_control),
            Err(GenerationValidationError::Cancelled)
        );

        let mut edges_control = ValidationControl::new(limits, || true);
        assert_matches!(
            reduce_edges(
                vec![EdgeInput {
                    source_symbol_id: symbol_id(),
                    target_symbol_id: symbol_id(),
                    kind: cartograph_domain::EdgeKind::Calls,
                    confidence: 1.0,
                    provenance: "test".to_owned(),
                    site_count: 1,
                }],
                &mut edges_control,
            ),
            Err(GenerationValidationError::Cancelled)
        );

        let mut references_control = ValidationControl::new(limits, || true);
        assert_matches!(
            reduce_references(vec![reference()], &mut references_control),
            Err(GenerationValidationError::Cancelled)
        );

        let mut numerical_control = ValidationControl::new(limits, || true);
        assert_matches!(
            reduce_numerical_sites(vec![numerical_site()], &mut numerical_control),
            Err(GenerationValidationError::Cancelled)
        );

        let mut documents_control = ValidationControl::new(limits, || true);
        assert_matches!(
            reduce_documents(vec![document()], &mut documents_control),
            Err(GenerationValidationError::Cancelled)
        );
    }

    #[test]
    fn canonical_digest_and_rows_preserve_complete_reference_evidence() {
        let mut resolved = reference();
        resolved.owner_symbol_id = Some(symbol_id());
        resolved.target_symbol_id = Some(symbol_id());
        resolved.confidence = 0.95;
        resolved.resolution_provenance = "test-exact".to_owned();
        let facts = GenerationFacts {
            files: vec![file()],
            symbols: vec![symbol()],
            references: vec![resolved.clone()],
            ..GenerationFacts::default()
        };
        let canonical = validate_and_reduce(facts)
            .unwrap_or_else(|error| panic!("reference evidence was rejected: {error}"));
        assert_eq!(canonical.references(), [resolved.clone()]);

        let mut changed = resolved.clone();
        changed.reference_name = "different_name".to_owned();
        let changed = validate_and_reduce(GenerationFacts {
            files: vec![file()],
            symbols: vec![symbol()],
            references: vec![changed],
            ..GenerationFacts::default()
        })
        .unwrap_or_else(|error| panic!("changed reference evidence was rejected: {error}"));
        assert_ne!(canonical.digest(), changed.digest());

        let mut multiplicity = resolved.clone();
        multiplicity.site_count = 2;
        let multiplicity = validate_and_reduce(GenerationFacts {
            files: vec![file()],
            symbols: vec![symbol()],
            references: vec![multiplicity],
            ..GenerationFacts::default()
        })
        .unwrap_or_else(|error| panic!("reference multiplicity was rejected: {error}"));
        assert_ne!(canonical.digest(), multiplicity.digest());

        let mut coarse = resolved;
        coarse.span_precision = super::super::ReferenceSpanPrecision::CoarsePoint;
        let coarse = validate_and_reduce(GenerationFacts {
            files: vec![file()],
            symbols: vec![symbol()],
            references: vec![coarse],
            ..GenerationFacts::default()
        })
        .unwrap_or_else(|error| panic!("reference precision was rejected: {error}"));
        assert_ne!(canonical.digest(), coarse.digest());
        assert_eq!(canonical.digest_version(), GenerationDigestVersion::CURRENT);
    }

    #[test]
    fn edge_reduction_sums_sites_uses_maximum_confidence_and_digests_multiplicity() {
        let edge = EdgeInput {
            source_symbol_id: symbol_id(),
            target_symbol_id: symbol_id(),
            kind: cartograph_domain::EdgeKind::Calls,
            confidence: BASE_EDGE_CONFIDENCE,
            provenance: "test-sites".to_owned(),
            site_count: BASE_EDGE_SITE_COUNT,
        };
        let mut repeated = edge.clone();
        repeated.confidence = MAXIMUM_EDGE_CONFIDENCE;
        repeated.site_count = REPEATED_EDGE_SITE_COUNT;
        let canonical = validate_and_reduce(GenerationFacts {
            files: vec![file()],
            symbols: vec![symbol()],
            edges: vec![edge, repeated],
            ..GenerationFacts::default()
        })
        .unwrap_or_else(|error| panic!("repeated edge sites were rejected: {error}"));
        assert_eq!(canonical.edges().len(), 1);
        assert_eq!(canonical.edges()[0].site_count, REDUCED_EDGE_SITE_COUNT);
        assert_eq!(
            canonical.edges()[0].confidence.to_bits(),
            MAXIMUM_EDGE_CONFIDENCE.to_bits()
        );

        let mut changed = canonical.edges()[0].clone();
        changed.site_count = CHANGED_EDGE_SITE_COUNT;
        let changed = validate_and_reduce(GenerationFacts {
            files: vec![file()],
            symbols: vec![symbol()],
            edges: vec![changed],
            ..GenerationFacts::default()
        })
        .unwrap_or_else(|error| panic!("changed edge sites were rejected: {error}"));
        assert_ne!(canonical.digest(), changed.digest());
    }

    fn validate_test_document(
        document: SearchDocumentInput,
    ) -> Result<CanonicalSearchDocument, GenerationValidationError> {
        let limits = GenerationValidationLimits::new(
            MAX_VALIDATION_RETAINED_BYTES / 4,
            MAX_VALIDATION_RETAINED_BYTES,
        )
        .unwrap_or_else(|error| panic!("test validation limits were invalid: {error}"));
        let mut control = ValidationControl::new(limits, || false);
        validate_document(document, &mut control)
    }
}
