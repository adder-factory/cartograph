//! Reuse resolution only when the extractor proves its complete inputs unchanged.
//! Exhaustive typed comparison makes new extractor fields a compilation error
//! until their local and global effects have been reviewed explicitly.

use cartograph_db::rocks::{RocksError, StorageRequest};
use cartograph_db::{CanonicalGenerationFacts, CanonicalSearchDocument, SearchDocumentInput};

use super::{
    BTreeMap, CacheWriteRequest, DUPLICATE_MAXIMUM_LITERAL_RATIO_PPM, DUPLICATE_MINIMUM_LINES,
    ExtractedFile, GenerationFacts, NativeEvidencePolicy, NativeFileFacts, NativeParseCache,
    NativeParseCacheReport, NativePipelineConfig, NormalizedPath, ParseSourceRequest,
    SourceLanguage, SourceManifestEntry, SourceReadOptions, SourceRoot, SymbolKind, block_in_place,
    decode_cached_file, exact_limit_ceiling, is_production_clone_path, json, native_file_symbol_id,
    numerical_site_input, parse_manifest_entry, store_cached_manifest, symbol_document_code,
    symbol_health_metadata,
};

const MAXIMUM_LOCAL_SOURCE_BYTES: u64 = 256 * 1024;

/// Inputs borrowed from the existing project owner and its validated warm base.
pub struct NativeLocalEditRequest<'a> {
    /// Source reader using the current complete discovery policy.
    pub source_root: &'a SourceRoot,
    /// The exact current validated graph, never a previous generation cache.
    pub base: &'a CanonicalGenerationFacts,
    /// Complete changed-file list from a freshly hashed manifest.
    pub paths: &'a [NormalizedPath],
    /// Existing bounded extraction cache under the current parser contract.
    pub cache: NativeParseCache,
    /// Existing native extraction and evidence policy.
    pub config: NativePipelineConfig,
}

/// Re-extract bounded edits and return complete replacement file facts.
///
/// New/deleted declarations, imports, calls, receiver evidence, or clone inputs
/// return `None`, selecting full project reconciliation. Resolution and derived
/// topology are reused only after exact extractor-input comparison. This is a
/// conservative optimization; it never changes which facts a cold build emits.
/// # Errors
/// Returns cooperative cancellation/deadline errors. Cache or admission failures
/// select the existing complete build instead.
pub async fn try_native_local_edit(
    input: NativeLocalEditRequest<'_>,
    request: &StorageRequest,
) -> Result<Option<GenerationFacts>, RocksError> {
    cartograph_db::NativeDatabase::with_request(request.clone(), local_edit(input, request)).await
}

async fn local_edit(
    input: NativeLocalEditRequest<'_>,
    request: &StorageRequest,
) -> Result<Option<GenerationFacts>, RocksError> {
    request.check()?;
    if input.paths.is_empty() || input.paths.len() > 16 || !input.cache.read_enabled {
        return Ok(None);
    }
    let cache = input
        .cache
        .clone()
        .with_maximum_ast_depth(input.config.maximum_ast_depth());
    let mut replacements = GenerationFacts::default();
    for path in input.paths {
        request.check()?;
        let Some(file) = input
            .base
            .files()
            .iter()
            .find(|file| file.normalized_path == path.as_str())
        else {
            return Ok(None);
        };
        if file.byte_size > MAXIMUM_LOCAL_SOURCE_BYTES {
            return Ok(None);
        }
        let Some(language) = SourceLanguage::from_stable_str(&file.language) else {
            return Ok(None);
        };
        let old_manifest = SourceManifestEntry {
            path: path.clone(),
            language,
            file_id: file.file_id.clone(),
            content_hash: file.content_hash.clone(),
            byte_size: file.byte_size,
        };
        let key = cache.key(&old_manifest);
        let loaded = cache.database.load_native_parse_cache(&key).await;
        request.check()?;
        let Ok(Some(record)) = loaded else {
            return Ok(None);
        };
        if record.payload().len() > 4 * 1_048_576 {
            return Ok(None);
        }
        let Some(old) = block_in_place(|| decode_cached_file(&old_manifest, &record)) else {
            return Ok(None);
        };
        let parsed = block_in_place(|| parse_local_edit(&input, path, &old, request));
        request.check()?;
        let Some(next) = parsed else {
            return Ok(None);
        };
        let manifest = SourceManifestEntry {
            path: path.clone(),
            language: next.language,
            file_id: next.file_id.clone(),
            content_hash: next.content_hash.clone(),
            byte_size: next.byte_size,
        };
        let key = cache.key(&manifest);
        let mut metrics = NativeParseCacheReport::default();
        // A failed cache write cannot change the correctness of this build.
        let _ = store_cached_manifest(CacheWriteRequest {
            cache: &cache,
            key: &key,
            file: &next,
            metrics: &mut metrics,
        })
        .await;
        request.check()?;
        let built = block_in_place(|| {
            replacement_facts(
                input.base,
                next,
                input.config.evidence_policy(),
                &mut replacements,
            )?;
            replacements
                .measure_retained_bytes(32 * 1_048_576, || request.check().is_err())
                .ok()?;
            Some(())
        });
        request.check()?;
        if built.is_none() {
            return Ok(None);
        }
    }
    Ok(Some(replacements))
}

fn parse_local_edit(
    input: &NativeLocalEditRequest<'_>,
    path: &NormalizedPath,
    old: &ExtractedFile,
    request: &StorageRequest,
) -> Option<ExtractedFile> {
    let limits = exact_limit_ceiling(MAXIMUM_LOCAL_SOURCE_BYTES).ok()?;
    let snapshot = input
        .source_root
        .read_with_cancellation(
            path,
            SourceReadOptions::new(limits, || request.check().is_err()),
        )
        .ok()?;
    let manifest = SourceManifestEntry {
        path: path.clone(),
        language: snapshot.language(),
        file_id: snapshot.file_id().clone(),
        content_hash: snapshot.content_hash().clone(),
        byte_size: snapshot.byte_size(),
    };
    let next = parse_manifest_entry(
        &ParseSourceRequest {
            source_root: input.source_root,
            manifest,
            maximum_ast_depth: input.config.maximum_ast_depth(),
        },
        || request.check().is_err(),
    )
    .ok()?;
    preserves_resolution(old, &next, input, request).then_some(next)
}

fn preserves_resolution(
    old: &ExtractedFile,
    next: &ExtractedFile,
    input: &NativeLocalEditRequest<'_>,
    request: &StorageRequest,
) -> bool {
    if old.parse_status != cartograph_domain::FileParseStatus::Parsed
        || !same_file_resolution(old, next)
        || old.symbols.len() != next.symbols.len()
    {
        return false;
    }
    let clone_enabled = is_production_clone_path(old.path.as_str())
        && !input
            .source_root
            .duplicate_code_allowlisted(old.path.as_str());
    for (previous, current) in old.symbols.iter().zip(&next.symbols) {
        if request.check().is_err() || !same_symbol_resolution(previous, current) {
            return false;
        }
        // MyBatis uses XML body hints as declaration roles when resolving
        // references from other files. These are global resolution inputs.
        if old.language == SourceLanguage::Xml
            && (previous.body_search_text != current.body_search_text
                || previous.body_search_truncated != current.body_search_truncated)
        {
            return false;
        }
        if clone_enabled && !preserves_clone_membership(previous, current, input, next, request) {
            return false;
        }
    }
    true
}

fn preserves_clone_membership(
    previous: &cartograph_extract::ExtractedSymbol,
    current: &cartograph_extract::ExtractedSymbol,
    input: &NativeLocalEditRequest<'_>,
    next: &ExtractedFile,
    request: &StorageRequest,
) -> bool {
    if !matches!(
        previous.kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Component
    ) || [previous, current].iter().all(|symbol| {
        symbol
            .span
            .end_line()
            .saturating_sub(symbol.span.start_line())
            .saturating_add(1)
            < DUPLICATE_MINIMUM_LINES
    }) {
        return true;
    }
    if previous.clone_shape_digest != current.clone_shape_digest
        || previous.clone_token_profile != current.clone_token_profile
        || clone_literal_heavy(previous) != clone_literal_heavy(current)
    {
        return false;
    }
    previous.structural_digest == current.structural_digest
        || input.paths.len() == 1 && unique_exact_edit(input.base, previous, current, next, request)
}

fn same_file_resolution(old: &ExtractedFile, next: &ExtractedFile) -> bool {
    let ExtractedFile {
        file_id,
        path,
        language,
        content_hash: _,
        byte_size: _,
        line_count: _,
        parse_status,
        symbols: _,
        containments,
        references,
        call_scope_sites,
        javascript_member_calls,
        resolution_abstentions,
        local_type_scopes,
        receiver_evidence,
        numerical_sites: _,
        import_bindings,
        has_inline_tests,
        test_search_text: _,
        test_search_truncated: _,
        diagnostics,
    } = old;
    file_id == &next.file_id
        && path == &next.path
        && language == &next.language
        && parse_status == &next.parse_status
        && containments == &next.containments
        && references == &next.references
        && call_scope_sites == &next.call_scope_sites
        && javascript_member_calls == &next.javascript_member_calls
        && resolution_abstentions == &next.resolution_abstentions
        && local_type_scopes == &next.local_type_scopes
        && receiver_evidence == &next.receiver_evidence
        && import_bindings == &next.import_bindings
        && has_inline_tests == &next.has_inline_tests
        && diagnostics == &next.diagnostics
}

fn same_symbol_resolution(
    old: &cartograph_extract::ExtractedSymbol,
    next: &cartograph_extract::ExtractedSymbol,
) -> bool {
    let cartograph_extract::ExtractedSymbol {
        id,
        kind,
        name,
        qualified_name,
        span,
        signature,
        docstring: _,
        body_search_text: _,
        body_search_truncated: _,
        health,
        implementation,
        export,
        execution,
        declaration_syntax,
        visibility,
        structural_digest: _,
        clone_shape_digest: _,
        clone_token_profile: _,
    } = old;
    id == &next.id
        && kind == &next.kind
        && name == &next.name
        && qualified_name == &next.qualified_name
        && span == &next.span
        && signature == &next.signature
        && health.parameter_count == next.health.parameter_count
        && implementation == &next.implementation
        && export == &next.export
        && execution == &next.execution
        && declaration_syntax == &next.declaration_syntax
        && visibility == &next.visibility
}

fn unique_exact_edit(
    base: &CanonicalGenerationFacts,
    old: &cartograph_extract::ExtractedSymbol,
    next: &cartograph_extract::ExtractedSymbol,
    file: &ExtractedFile,
    request: &StorageRequest,
) -> bool {
    // Literal edits keep normalized clone inputs. Both concrete digests must be
    // unique so Type-1 membership and Type-2/3 precedence stay unchanged.
    for symbol in base.symbols() {
        if request.check().is_err()
            || symbol.symbol_id != old.id
                && (symbol.structural_digest == old.structural_digest
                    || symbol.structural_digest == next.structural_digest)
        {
            return false;
        }
    }
    !file
        .symbols
        .iter()
        .any(|symbol| symbol.id != next.id && symbol.structural_digest == next.structural_digest)
}

fn clone_literal_heavy(symbol: &cartograph_extract::ExtractedSymbol) -> bool {
    u64::from(symbol.health.literal_bytes).saturating_mul(1_000_000)
        > symbol
            .span
            .end_byte()
            .saturating_sub(symbol.span.start_byte())
            .max(1)
            .saturating_mul(DUPLICATE_MAXIMUM_LITERAL_RATIO_PPM)
}

fn replacement_facts(
    base: &CanonicalGenerationFacts,
    next: ExtractedFile,
    policy: NativeEvidencePolicy,
    output: &mut GenerationFacts,
) -> Option<()> {
    let file = NativeFileFacts::from_extracted(next).ok()?;
    let old_symbols = base
        .symbols()
        .iter()
        .filter(|row| row.file_id == file.file.file_id)
        .map(|row| (&row.symbol_id, row))
        .collect::<BTreeMap<_, _>>();
    let documents = base
        .documents()
        .iter()
        .filter(|row| row.file_id() == Some(&file.file.file_id))
        .filter_map(|row| Some((row.symbol_id()?, row)))
        .collect::<BTreeMap<_, _>>();
    let file_symbol_id = native_file_symbol_id(&file.file.file_id);
    let mut file_symbol = (*old_symbols.get(&file_symbol_id)?).clone();
    file_symbol.end_byte = file.file.byte_size;
    file_symbol.end_line = file.line_count;
    file_symbol.structural_digest = file.file.content_hash.clone();
    output.symbols.push(file_symbol);
    let old_document = documents.get(&file_symbol_id)?;
    let mut file_document = raw_document(old_document)?;
    file_document
        .natural_text
        .clone_from(&file.test_search_text);
    file_document.metadata["byte_size"] = json!(file.file.byte_size);
    file_document.metadata["test_search_truncated"] = json!(file.test_search_truncated);
    output.documents.push(file_document);
    for mut symbol in file.symbols {
        let previous = old_symbols.get(&symbol.input.symbol_id)?;
        symbol.input.pagerank_ppb = previous.pagerank_ppb;
        symbol.input.betweenness_ppb = previous.betweenness_ppb;
        let previous_document = documents.get(&symbol.input.symbol_id)?;
        let mut document = raw_document(previous_document)?;
        document.code = symbol_document_code(&symbol).ok()?;
        document.natural_text = if policy.retention.docstrings {
            symbol.docstring.take().unwrap_or_default()
        } else {
            String::new()
        };
        document.metadata["health"] = symbol_health_metadata(&symbol.health);
        document.metadata["body_search_truncated"] = json!(symbol.body_search_truncated);
        document.metadata["clone_shape_digest"] = json!(symbol.clone_shape_digest.as_str());
        output.documents.push(document);
        output.symbols.push(symbol.input);
    }
    output.references.extend(
        base.references()
            .iter()
            .filter(|row| row.file_id == file.file.file_id)
            .cloned(),
    );
    output.numerical_sites.extend(
        file.numerical_sites
            .into_iter()
            .map(|site| numerical_site_input(&file.file.file_id, site)),
    );
    output.files.push(file.file);
    Some(())
}

fn raw_document(document: &CanonicalSearchDocument) -> Option<SearchDocumentInput> {
    Some(SearchDocumentInput {
        document_id: document.document_id().clone(),
        file_id: document.file_id().cloned(),
        symbol_id: document.symbol_id().cloned(),
        path: document.path().to_owned(),
        language: document.language().to_owned(),
        kind: document.kind(),
        qualified_name: document.qualified_name().to_owned(),
        code: document.code().to_owned(),
        natural_text: document.natural_text().to_owned(),
        metadata: serde_json::from_str(document.metadata_json()).ok()?,
    })
}
