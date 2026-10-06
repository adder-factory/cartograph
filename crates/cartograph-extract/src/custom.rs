use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::{
    ContentDigest, FileParseStatus, ReferenceKind, SourceLanguage, SourcePosition, SourceSpan,
    SymbolId, SymbolKind, Visibility,
};
use serde_json::Value;

use crate::{
    Containment, DiagnosticCode, ExtractError, ExtractedFile, ExtractedImportBinding,
    ExtractedReference, ExtractedSymbol, ExtractionDiagnostic, ImportBindingKind, SourceSnapshot,
    SymbolExecutionFlags, SymbolExportFlags, SymbolImplementationFlags,
    bounded_name::{MAX_CANONICAL_QUALIFIED_NAME_BYTES, shortened_canonical_name},
    budget::{
        ExtractionBudget, containment_budget_bytes, diagnostic_budget_bytes,
        import_binding_budget_bytes, reference_budget_bytes, symbol_budget_bytes,
    },
    identity::SymbolIdentity,
    source_lines::{LineMap, SourceByteRange, physical_lines},
    walk::specifier_safety::specifier_may_carry_credential,
};

const CUSTOM_DIGEST_CONTEXT: &str = "cartograph.v2.custom-structural-digest.2026-07-24";
const MAX_REFERENCE_NAME_BYTES: usize = 4_096;
const CUSTOM_CANCELLATION_POLL_BYTES: usize = 4_096;

mod bg3;
mod bg3_tokens;
mod game_scripting;
mod liquid;
mod pascal_form;
mod rhai;
mod web_component;

pub(crate) use web_component::file_component_symbol;

/// Whether a grammar-backed language's snapshot is scanned here instead,
/// such as a Delphi form file in the Pascal language mode.
pub(crate) fn scans_snapshot(snapshot: &SourceSnapshot) -> bool {
    pascal_form::supports(snapshot)
}

fn poll_cancellation(
    cancelled: &mut dyn FnMut() -> bool,
    cursor: usize,
    next_poll: &mut usize,
) -> Result<(), ExtractError> {
    if cursor < *next_poll {
        return Ok(());
    }
    if cancelled() {
        return Err(ExtractError::Cancelled);
    }
    *next_poll = cursor.saturating_add(CUSTOM_CANCELLATION_POLL_BYTES);
    Ok(())
}

/// Scan one snapshot. `maximum_ast_depth` is the configured structural nesting
/// ceiling for scanners that track nesting (Delphi form components).
pub(crate) fn extract(
    snapshot: &SourceSnapshot,
    maximum_ast_depth: usize,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<ExtractedFile, ExtractError> {
    let mut builder = CustomBuilder::new(snapshot, maximum_ast_depth, cancelled)?;
    if pascal_form::supports(snapshot) {
        let parse_status = pascal_form::extract(&mut builder, maximum_ast_depth)?;
        return builder.finish(parse_status);
    }
    if snapshot.language() == SourceLanguage::Rhai {
        let parse_status = rhai::extract(&mut builder)?;
        return builder.finish(parse_status);
    }
    if game_scripting::supports(snapshot.language()) {
        let parse_status = game_scripting::extract(&mut builder)?;
        return builder.finish(parse_status);
    }
    let parse_status = extract_existing_custom(&mut builder)?;
    builder.finish(parse_status)
}

fn extract_existing_custom(
    builder: &mut CustomBuilder<'_, '_>,
) -> Result<FileParseStatus, ExtractError> {
    match builder.snapshot.language() {
        SourceLanguage::Properties => extract_properties(builder)?,
        SourceLanguage::Toml => {}
        SourceLanguage::Liquid => extract_liquid(builder)?,
        SourceLanguage::Svelte | SourceLanguage::Vue => {
            return web_component::extract_component_file(builder);
        }
        SourceLanguage::Aura | SourceLanguage::Visualforce => {
            extract_salesforce_markup(builder)?;
        }
        SourceLanguage::Vb6 => extract_vb6(builder)?,
        SourceLanguage::Xml => extract_xml(builder)?,
        SourceLanguage::Bg3Anubis => extract_anubis(builder)?,
        SourceLanguage::Bg3Stats => extract_bg3_stats(builder)?,
        SourceLanguage::Osiris => extract_osiris(builder)?,
        SourceLanguage::Bg3Resource => extract_bg3_resource(builder)?,
        _ => return Err(ExtractError::UnsupportedLanguage),
    }
    Ok(FileParseStatus::Parsed)
}

#[derive(Default)]
struct SymbolOptions {
    signature: Option<String>,
    body_search_text: String,
    declaration_only: bool,
    export: SymbolExportFlags,
    async_symbol: bool,
    static_member: bool,
    visibility: Option<Visibility>,
    parent: Option<SymbolId>,
}

struct CustomSymbolInput<'name> {
    kind: SymbolKind,
    name: &'name str,
    qualified_name: String,
    start: usize,
    end: usize,
    options: SymbolOptions,
}

impl<'name> CustomSymbolInput<'name> {
    fn new(kind: SymbolKind, name: &'name str, qualified_name: String) -> Self {
        Self {
            kind,
            name,
            qualified_name,
            start: 0,
            end: 0,
            options: SymbolOptions::default(),
        }
    }

    const fn at(mut self, start: usize, end: usize) -> Self {
        self.start = start;
        self.end = end;
        self
    }

    fn with_options(mut self, options: SymbolOptions) -> Self {
        self.options = options;
        self
    }
}

struct CustomReferenceInput<'name> {
    owner: Option<SymbolId>,
    name: &'name str,
    resolution_name: Option<&'name str>,
    kind: ReferenceKind,
    start: usize,
    end: usize,
}

impl<'name> CustomReferenceInput<'name> {
    fn new(owner: Option<SymbolId>, name: &'name str, kind: ReferenceKind) -> Self {
        Self {
            owner,
            name,
            resolution_name: None,
            kind,
            start: 0,
            end: 0,
        }
    }

    const fn with_resolution(mut self, resolution_name: &'name str) -> Self {
        self.resolution_name = Some(resolution_name);
        self
    }

    const fn at(mut self, start: usize, end: usize) -> Self {
        self.start = start;
        self.end = end;
        self
    }
}

struct CustomImportInput<'name> {
    owner: Option<SymbolId>,
    kind: ImportBindingKind,
    module: &'name str,
    imported: &'name str,
    local: &'name str,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy)]
struct SourceSliceInput<'source> {
    value: &'source str,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy)]
struct OwnedSourceInput<'owner, 'source> {
    owner: &'owner SymbolId,
    source: &'source str,
    offset: usize,
}

#[derive(Clone, Copy)]
struct MybatisMapperInput<'source> {
    tags: &'source [MarkupTag<'source>],
    namespace: &'source str,
    namespace_offset: usize,
}

#[derive(Clone, Copy)]
struct MybatisBodyInput<'owner, 'source> {
    owner: &'owner SymbolId,
    namespace: &'source str,
    /// Java package of the mapper namespace (`com.example` for
    /// `com.example.OrderMapper`), when the namespace is package-qualified.
    package: Option<&'source str>,
    statement: &'source str,
    start: usize,
    end: usize,
}

struct OsirisDeclarationInput<'source> {
    goal: SymbolId,
    line: &'source str,
    raw_line: &'source str,
    line_start: usize,
}

impl<'name> CustomImportInput<'name> {
    fn new(owner: Option<SymbolId>, module: &'name str) -> Self {
        Self {
            owner,
            kind: ImportBindingKind::Named,
            module,
            imported: module,
            local: module,
            start: 0,
            end: 0,
        }
    }

    const fn binding(mut self, imported: &'name str, local: &'name str) -> Self {
        self.imported = imported;
        self.local = local;
        self
    }

    const fn with_kind(mut self, kind: ImportBindingKind) -> Self {
        self.kind = kind;
        self
    }

    const fn at(mut self, start: usize, end: usize) -> Self {
        self.start = start;
        self.end = end;
        self
    }
}

struct CustomBuilder<'source, 'cancel> {
    snapshot: &'source SourceSnapshot,
    cancelled: &'cancel mut dyn FnMut() -> bool,
    lines: LineMap,
    budget: ExtractionBudget,
    identities: SymbolIdentity<'source>,
    symbols: Vec<ExtractedSymbol>,
    containments: Vec<Containment>,
    references: Vec<ExtractedReference>,
    import_bindings: Vec<ExtractedImportBinding>,
    /// Whether a synthesized qualified name was shortened to its canonical bound.
    shortened_canonical_names: bool,
    /// Diagnostics of embedded component regions that lost facts (script
    /// syntax errors, template expressions too deep to walk, a template scan
    /// budget used up by unterminated structure).
    diagnostics: Vec<ExtractionDiagnostic>,
    /// Project AST-depth ceiling for embedded component scripts.
    maximum_ast_depth: usize,
}

impl<'source, 'cancel> CustomBuilder<'source, 'cancel> {
    fn new(
        snapshot: &'source SourceSnapshot,
        maximum_ast_depth: usize,
        cancelled: &'cancel mut dyn FnMut() -> bool,
    ) -> Result<Self, ExtractError> {
        Ok(Self {
            snapshot,
            cancelled,
            lines: LineMap::new(snapshot.source())?,
            budget: ExtractionBudget::new(snapshot)?,
            identities: SymbolIdentity::new(snapshot.path()),
            symbols: Vec::new(),
            containments: Vec::new(),
            references: Vec::new(),
            import_bindings: Vec::new(),
            shortened_canonical_names: false,
            diagnostics: Vec::new(),
            maximum_ast_depth,
        })
    }

    fn check_cancelled(&mut self) -> Result<(), ExtractError> {
        if (self.cancelled)() {
            Err(ExtractError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn source(&self) -> &'source str {
        self.snapshot.source()
    }

    fn path(&self) -> &str {
        self.snapshot.path().as_str()
    }

    fn span(&self, start: usize, end: usize) -> Result<SourceSpan, ExtractError> {
        // An empty file's convention-derived facts (an Aura component, a
        // Visualforce route) sit at a zero-width point, as framework landmarks do.
        if start == end && self.source().is_empty() {
            return Ok(SourceSpan::synthetic(
                SourcePosition::new(0, 1, 0).map_err(|_| ExtractError::InvalidSpan)?,
            ));
        }
        self.lines
            .span(SourceByteRange::new(start, end, self.source().len()))
    }

    fn add_symbol(&mut self, input: CustomSymbolInput<'_>) -> Result<SymbolId, ExtractError> {
        let CustomSymbolInput {
            kind,
            name,
            qualified_name,
            start,
            end,
            options,
        } = input;
        if name.is_empty() || qualified_name.is_empty() {
            return Err(ExtractError::InvalidSpan);
        }
        self.check_cancelled()?;
        let qualified_name = self.bound_qualified_name(qualified_name);
        let span = self.span(start, end)?;
        let id = self.identities.next(kind, &qualified_name)?;
        let parent = options.parent.clone();
        let symbol = custom_symbol(CustomSymbolParts {
            id: id.clone(),
            kind,
            name,
            qualified_name,
            span,
            options,
        })?;
        self.budget.reserve_fact(
            symbol_budget_bytes(&symbol),
            [
                symbol.name.as_str(),
                symbol.qualified_name.as_str(),
                symbol.signature.as_deref().unwrap_or(""),
                symbol.body_search_text.as_str(),
            ],
        )?;
        if let Some(parent) = parent {
            let containment = Containment {
                parent,
                child: id.clone(),
            };
            self.budget
                .reserve_fact(containment_budget_bytes(&containment), std::iter::empty())?;
            self.containments.push(containment);
        }
        self.symbols.push(symbol);
        Ok(id)
    }

    /// Shorten a synthesized qualified name past its canonical storage bound
    /// (a deeply nested component path), recording that the file carries one.
    fn bound_qualified_name(&mut self, name: String) -> String {
        match shortened_canonical_name(&name, MAX_CANONICAL_QUALIFIED_NAME_BYTES) {
            Some(shortened) => {
                self.shortened_canonical_names = true;
                shortened
            }
            None => name,
        }
    }

    fn add_reference(&mut self, input: CustomReferenceInput<'_>) -> Result<(), ExtractError> {
        self.add_reference_with_resolution(input)
    }

    /// Normalize a reference name. In BG3 game data a GUIDSTRING
    /// (`Name_<uuid>`) is a public object identity whose only high-entropy
    /// part is a canonical UUID, so it is exempt from the entropy screen
    /// there and nowhere else.
    fn normalize_reference_name(&self, raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        if matches!(
            self.snapshot.language(),
            SourceLanguage::Bg3Anubis
                | SourceLanguage::Bg3Resource
                | SourceLanguage::Bg3Stats
                | SourceLanguage::Osiris
        ) && let Some(name) = bg3_tokens::guid_string_name(trimmed)
        {
            // Only the entropy screen is waived: the name part must still
            // pass every credential check.
            return (!looks_sensitive(name))
                .then(|| bounded_string(trimmed).ok())
                .flatten();
        }
        normalize_reference(raw)
    }

    fn add_reference_with_resolution(
        &mut self,
        input: CustomReferenceInput<'_>,
    ) -> Result<(), ExtractError> {
        let CustomReferenceInput {
            owner,
            name,
            resolution_name,
            kind,
            start,
            end,
        } = input;
        let Some(name) = self.normalize_reference_name(name) else {
            return Ok(());
        };
        let resolution_name = resolution_name.and_then(normalize_reference);
        let reference = ExtractedReference {
            owner,
            name,
            resolution_name,
            kind,
            span: self.span(start, end)?,
        };
        self.budget.reserve_fact(
            reference_budget_bytes(&reference),
            [
                reference.name.as_str(),
                reference.resolution_name.as_deref().unwrap_or(""),
            ],
        )?;
        self.references.push(reference);
        Ok(())
    }

    fn add_import(&mut self, input: &CustomImportInput<'_>) -> Result<(), ExtractError> {
        self.add_reference(
            CustomReferenceInput::new(input.owner.clone(), input.module, ReferenceKind::Imports)
                .at(input.start, input.end),
        )?;
        self.add_import_binding(input)
    }

    fn add_import_binding(&mut self, input: &CustomImportInput<'_>) -> Result<(), ExtractError> {
        if [input.module, input.imported, input.local]
            .into_iter()
            .any(specifier_may_carry_credential)
        {
            return Ok(());
        }
        let binding = ExtractedImportBinding {
            kind: input.kind,
            module_specifier: bounded_string(input.module)?,
            imported_name: bounded_string(input.imported)?,
            local_name: bounded_string(input.local)?,
            span: self.span(input.start, input.end)?,
        };
        self.budget.reserve_fact(
            import_binding_budget_bytes(&binding),
            [
                binding.module_specifier.as_str(),
                binding.imported_name.as_str(),
                binding.local_name.as_str(),
            ],
        )?;
        self.import_bindings.push(binding);
        Ok(())
    }

    fn finish(mut self, parse_status: FileParseStatus) -> Result<ExtractedFile, ExtractError> {
        // A shortened name keeps the generation publishable, but the file no
        // longer carries the exact synthesized identity and must say so.
        let mut diagnostics = self.diagnostics;
        if self.shortened_canonical_names {
            self.budget
                .reserve_fact(diagnostic_budget_bytes(), std::iter::empty())?;
            diagnostics.push(ExtractionDiagnostic {
                code: DiagnosticCode::CanonicalNameTruncated,
                span: None,
            });
        }
        let output_limit = self.budget.output_limit();
        let file = ExtractedFile {
            file_id: self.snapshot.file_id().clone(),
            path: self.snapshot.path().clone(),
            language: self.snapshot.language(),
            content_hash: self.snapshot.content_hash().clone(),
            byte_size: self.snapshot.byte_size(),
            line_count: self.snapshot.line_count(),
            parse_status,
            symbols: self.symbols,
            containments: self.containments,
            references: self.references,
            call_scope_sites: Vec::new(),
            javascript_member_calls: Vec::new(),
            resolution_abstentions: Vec::new(),
            local_type_scopes: Vec::new(),
            receiver_evidence: None,
            numerical_sites: Vec::new(),
            import_bindings: self.import_bindings,
            has_inline_tests: false,
            test_search_text: String::new(),
            test_search_truncated: false,
            diagnostics,
        };
        if file.modeled_retained_bytes() > output_limit {
            return Err(ExtractError::OutputLimit);
        }
        Ok(file)
    }
}

/// One custom-structural symbol before budget accounting and containment.
struct CustomSymbolParts<'name> {
    id: SymbolId,
    kind: SymbolKind,
    name: &'name str,
    qualified_name: String,
    span: SourceSpan,
    options: SymbolOptions,
}

fn custom_symbol(parts: CustomSymbolParts<'_>) -> Result<ExtractedSymbol, ExtractError> {
    let CustomSymbolParts {
        id,
        kind,
        name,
        qualified_name,
        span,
        options,
    } = parts;
    let structural_digest = custom_digest(kind, &qualified_name, &options.body_search_text);
    let clone_shape_digest = structural_digest.clone();
    Ok(ExtractedSymbol {
        id,
        kind,
        name: bounded_string(name)?,
        qualified_name,
        span,
        signature: options
            .signature
            .filter(|text| !text.split_whitespace().any(specifier_may_carry_credential)),
        docstring: None,
        body_search_text: options.body_search_text,
        body_search_truncated: false,
        health: crate::SymbolHealthMetrics::default(),
        implementation: SymbolImplementationFlags {
            declaration_only: options.declaration_only,
            test_symbol: false,
        },
        export: options.export,
        execution: SymbolExecutionFlags {
            async_symbol: options.async_symbol,
            static_member: options.static_member,
        },
        declaration_syntax: crate::DeclarationSyntax::Other,
        visibility: options.visibility,
        structural_digest,
        clone_shape_digest,
        clone_token_profile: None,
    })
}

fn custom_digest(kind: SymbolKind, qualified_name: &str, safe_structure: &str) -> ContentDigest {
    let mut hasher = blake3::Hasher::new_derive_key(CUSTOM_DIGEST_CONTEXT);
    for field in [kind.as_str(), qualified_name, safe_structure] {
        hasher.update(&u64::try_from(field.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    ContentDigest::from_bytes(*hasher.finalize().as_bytes())
}

fn bounded_string(value: &str) -> Result<String, ExtractError> {
    if value.len() > MAX_REFERENCE_NAME_BYTES {
        return Err(ExtractError::OutputLimit);
    }
    let mut output = String::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    output.push_str(value);
    Ok(output)
}

fn normalize_reference(raw: &str) -> Option<String> {
    let value = raw.trim().trim_matches(|ch| matches!(ch, '\'' | '"' | '`'));
    if value.is_empty()
        || value.len() > MAX_REFERENCE_NAME_BYTES
        || value.bytes().any(|byte| byte.is_ascii_control())
        || looks_sensitive(value)
    {
        return None;
    }
    bounded_string(value).ok()
}

fn looks_sensitive(value: &str) -> bool {
    if specifier_may_carry_credential(value) {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    if [
        "sk_live_",
        "sk_test_",
        "ghp_",
        "github_pat_",
        "xoxb_",
        "xoxp_",
        "akia",
        "asia",
    ]
    .into_iter()
    .any(|prefix| lower.starts_with(prefix))
    {
        return true;
    }
    let sensitive_word = lower
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|token| {
            matches!(
                token,
                "password"
                    | "passwd"
                    | "secret"
                    | "token"
                    | "apikey"
                    | "privatekey"
                    | "clientsecret"
                    | "credential"
                    | "credentials"
            )
        });
    let high_entropy = value.len() >= 24
        && value.bytes().any(|byte| byte.is_ascii_lowercase())
        && value.bytes().any(|byte| byte.is_ascii_uppercase())
        && value.bytes().any(|byte| byte.is_ascii_digit());
    sensitive_word || high_entropy
}

/// File name of `path` without its final extension.
pub(crate) fn basename_stem(path: &str) -> &str {
    let filename = path.rsplit('/').next().unwrap_or(path);
    filename
        .rfind('.')
        .map_or(filename, |extension| &filename[..extension])
}

fn is_identifier_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_identifier_body(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit() || byte == b'$'
}

fn identifiers(value: &str) -> Vec<(usize, &str)> {
    let bytes = value.as_bytes();
    let mut output = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if !is_identifier_start(bytes[cursor]) {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while cursor < bytes.len() && is_identifier_body(bytes[cursor]) {
            cursor += 1;
        }
        output.push((start, &value[start..cursor]));
    }
    output
}

fn first_identifier(value: &str) -> Option<(usize, &str)> {
    identifiers(value).into_iter().next()
}

fn word_after<'a>(value: &'a str, keyword: &str) -> Option<(usize, &'a str)> {
    let trimmed = value.trim_start();
    let indent = value.len().saturating_sub(trimmed.len());
    let suffix = trimmed.get(keyword.len()..)?;
    if !trimmed[..keyword.len()].eq_ignore_ascii_case(keyword)
        || suffix
            .as_bytes()
            .first()
            .is_some_and(|byte| is_identifier_body(*byte))
    {
        return None;
    }
    let (offset, word) = first_identifier(suffix)?;
    Some((indent + keyword.len() + offset, word))
}

fn quoted_values(value: &str) -> Vec<(usize, &str)> {
    let bytes = value.as_bytes();
    let mut output = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if !matches!(bytes[cursor], b'\'' | b'"') {
            cursor += 1;
            continue;
        }
        let quote = bytes[cursor];
        let start = cursor.saturating_add(1);
        cursor = start;
        while cursor < bytes.len() {
            if bytes[cursor] == b'\\' {
                cursor = cursor.saturating_add(2);
                continue;
            }
            if bytes[cursor] == quote {
                output.push((start, &value[start..cursor]));
                cursor += 1;
                break;
            }
            cursor += 1;
        }
    }
    output
}

fn extract_properties(builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
    let source = builder.source();
    for (line_start, line) in physical_lines(source) {
        builder.check_cancelled()?;
        let Some((key_start, key_end, key)) = properties_key(line) else {
            continue;
        };
        let owner = builder.add_symbol(
            CustomSymbolInput::new(SymbolKind::Constant, &key, key.clone())
                .at(line_start + key_start, line_start + key_end)
                .with_options(SymbolOptions {
                    body_search_text: key.clone(),
                    export: SymbolExportFlags::named(true),
                    visibility: Some(Visibility::Public),
                    ..SymbolOptions::default()
                }),
        )?;
        let value = &line[key_end..];
        for (offset, reference) in interpolation_references(value) {
            builder.add_reference(
                CustomReferenceInput::new(
                    Some(owner.clone()),
                    reference,
                    ReferenceKind::References,
                )
                .at(
                    line_start + key_end + offset,
                    line_start + key_end + offset + reference.len(),
                ),
            )?;
        }
    }
    Ok(())
}

fn properties_key(line: &str) -> Option<(usize, usize, String)> {
    let bytes = line.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t' | 0x0c) {
        cursor += 1;
    }
    if cursor == bytes.len() || matches!(bytes[cursor], b'#' | b'!') {
        return None;
    }
    let start = cursor;
    let mut key = String::new();
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' if cursor + 1 < bytes.len() => {
                let escaped = bytes[cursor + 1];
                if matches!(escaped, b'=' | b':' | b'\\' | b' ' | b'\t') {
                    key.push(char::from(escaped));
                } else {
                    key.push('\\');
                    key.push(char::from(escaped));
                }
                cursor += 2;
            }
            b'=' | b':' | b' ' | b'\t' | 0x0c => break,
            byte if byte.is_ascii() => {
                key.push(char::from(byte));
                cursor += 1;
            }
            _ => {
                let character = line[cursor..].chars().next()?;
                key.push(character);
                cursor += character.len_utf8();
            }
        }
    }
    (!key.is_empty() && cursor < bytes.len() && !specifier_may_carry_credential(&key))
        .then_some((start, cursor, key))
}

fn interpolation_references(value: &str) -> Vec<(usize, &str)> {
    let mut output = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = value[cursor..].find("${") {
        let open = cursor + relative;
        let content_start = open + 2;
        let Some(close_relative) = value[content_start..].find('}') else {
            break;
        };
        let close = content_start + close_relative;
        let name = value[content_start..close].trim();
        let leading =
            value[content_start..close].len() - value[content_start..close].trim_start().len();
        if is_qualified_name(name) {
            output.push((content_start + leading, name));
        }
        cursor = close + 1;
    }
    output
}

fn is_qualified_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_REFERENCE_NAME_BYTES
        && !specifier_may_carry_credential(value)
        && value.bytes().all(|byte| {
            is_identifier_body(byte) || matches!(byte, b'.' | b':' | b'/' | b'-' | b'#')
        })
}

fn extract_liquid(builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
    let source = builder.source();
    let mut cursor = 0;
    let mut schemas = liquid::SchemaBlocks::default();
    while let Some(relative) = source[cursor..].find("{%") {
        builder.check_cancelled()?;
        let open = cursor + relative;
        let Some(close_relative) = source[open + 2..].find("%}") else {
            break;
        };
        let close = open + 2 + close_relative + 2;
        let raw = source[open + 2..close - 2]
            .trim_matches(|character: char| character.is_whitespace() || character == '-');
        let tag = SourceSliceInput {
            value: raw,
            start: open,
            end: close,
        };
        if let Some(block_end) = liquid::extract_schema(builder, tag, &mut schemas)? {
            cursor = block_end;
            continue;
        }
        extract_liquid_tag(builder, tag)?;
        cursor = close;
    }
    extract_liquid_output_references(builder, schemas.ranges())
}

fn extract_liquid_tag(
    builder: &mut CustomBuilder<'_, '_>,
    input: SourceSliceInput<'_>,
) -> Result<(), ExtractError> {
    let SourceSliceInput {
        value: raw,
        start,
        end,
    } = input;
    let Some((_, command)) = first_identifier(raw) else {
        return Ok(());
    };
    let command_end = raw.find(command).unwrap_or(0) + command.len();
    let remainder = &raw[command_end..];
    match command {
        "render" | "include" | "section" => {
            // The partner is the first argument; a dynamic partner followed by
            // a quoted named argument (`render name, label: 'x'`) is not one.
            if !remainder.trim_start().starts_with(['\'', '"']) {
                return Ok(());
            }
            let Some((_, partner)) = quoted_values(remainder).into_iter().next() else {
                return Ok(());
            };
            if !liquid::is_display_name(partner) {
                return Ok(());
            }
            let folder = if command == "section" {
                "sections"
            } else {
                "snippets"
            };
            let module = format!("{folder}/{partner}.liquid");
            let qualified = format!("{}::{command}:{partner}", builder.path());
            // Every partner tag is both a rendered component and an import
            // site, as in v1.1.33; the component owns the import reference.
            let id = builder.add_symbol(
                CustomSymbolInput::new(SymbolKind::Component, partner, qualified.clone())
                    .at(start, end)
                    .with_options(SymbolOptions {
                        body_search_text: format!("{command} {partner}"),
                        ..SymbolOptions::default()
                    }),
            )?;
            builder.add_import(
                &CustomImportInput::new(Some(id), &module)
                    .binding(partner, partner)
                    .at(start, end),
            )?;
            builder.add_symbol(
                CustomSymbolInput::new(SymbolKind::Import, partner, qualified)
                    .at(start, end)
                    .with_options(SymbolOptions {
                        body_search_text: format!("{command} {partner}"),
                        ..SymbolOptions::default()
                    }),
            )?;
        }
        "assign" | "capture" => {
            let Some((_, name)) = first_identifier(remainder) else {
                return Ok(());
            };
            let qualified = format!("{}::{name}", builder.path());
            builder.add_symbol(
                CustomSymbolInput::new(SymbolKind::Variable, name, qualified)
                    .at(start, end)
                    .with_options(SymbolOptions {
                        body_search_text: format!("{command} {name}"),
                        ..SymbolOptions::default()
                    }),
            )?;
        }
        "block" => {
            let Some((_, name)) = first_identifier(remainder) else {
                return Ok(());
            };
            builder.add_symbol(
                CustomSymbolInput::new(
                    SymbolKind::Component,
                    name,
                    format!("{}::block:{name}", builder.path()),
                )
                .at(start, end)
                .with_options(SymbolOptions {
                    body_search_text: format!("block {name}"),
                    ..SymbolOptions::default()
                }),
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// `{{ ... }}` output references, excluding literal schema JSON blocks.
fn extract_liquid_output_references(
    builder: &mut CustomBuilder<'_, '_>,
    schemas: &[std::ops::Range<usize>],
) -> Result<(), ExtractError> {
    let source = builder.source();
    let mut cursor = 0;
    let mut next_schema = 0;
    let mut next_poll = 0;
    let mut seen = BTreeSet::new();
    while let Some(relative) = source[cursor..].find("{{") {
        let open = cursor + relative;
        poll_cancellation(&mut *builder.cancelled, open, &mut next_poll)?;
        while schemas
            .get(next_schema)
            .is_some_and(|schema| schema.end <= open)
        {
            next_schema += 1;
        }
        if let Some(schema) = schemas
            .get(next_schema)
            .filter(|schema| schema.contains(&open))
        {
            cursor = schema.end;
            continue;
        }
        let Some(close_relative) = source[open + 2..].find("}}") else {
            break;
        };
        let close = open + 2 + close_relative;
        let expression = &source[open + 2..close];
        if specifier_may_carry_credential(expression) {
            cursor = close + 2;
            continue;
        }
        for (relative_offset, name) in identifiers(expression) {
            if liquid_keyword(name) || !seen.insert((open, name)) {
                continue;
            }
            let start = open + 2 + relative_offset;
            builder.add_reference(
                CustomReferenceInput::new(None, name, ReferenceKind::References)
                    .at(start, start + name.len()),
            )?;
        }
        cursor = close + 2;
    }
    Ok(())
}

fn liquid_keyword(name: &str) -> bool {
    matches!(
        name,
        "and" | "or" | "contains" | "true" | "false" | "nil" | "null" | "blank" | "empty"
    )
}

#[derive(Clone, Copy)]
struct MarkupTag<'source> {
    name: &'source str,
    raw: &'source str,
    start: usize,
    end: usize,
    closing: bool,
    self_closing: bool,
}

fn markup_tags(source: &str) -> Vec<MarkupTag<'_>> {
    let bytes = source.as_bytes();
    let mut output = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let Some(relative) = source[cursor..].find('<') else {
            break;
        };
        let start = cursor + relative;
        if source[start..].starts_with("<!--") {
            cursor = source[start + 4..]
                .find("-->")
                .map_or(source.len(), |close| start + 4 + close + 3);
            continue;
        }
        let Some(end) = markup_tag_end(bytes, start + 1) else {
            break;
        };
        if let Some(tag) = parse_markup_tag(source, start, end) {
            output.push(tag);
        }
        cursor = end + 1;
    }
    output
}

fn markup_tag_end(bytes: &[u8], mut cursor: usize) -> Option<usize> {
    let mut quote = None;
    while let Some(byte) = bytes.get(cursor).copied() {
        match (quote, byte) {
            (Some(active), current) if current == active => quote = None,
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => return Some(cursor),
            (Some(_) | None, _) => {}
        }
        cursor = cursor.saturating_add(1);
    }
    None
}

fn parse_markup_tag(source: &str, start: usize, end: usize) -> Option<MarkupTag<'_>> {
    let raw = &source[start.saturating_add(1)..end];
    let trimmed = raw.trim();
    let closing = trimmed.starts_with('/');
    let name_source = trimmed.trim_start_matches('/').trim_start();
    if name_source.starts_with(['!', '?']) {
        return None;
    }
    let name_end = name_source
        .find(|character: char| character.is_whitespace() || character == '/')
        .unwrap_or(name_source.len());
    (name_end > 0).then(|| MarkupTag {
        name: &name_source[..name_end],
        raw,
        start,
        end: end.saturating_add(1),
        closing,
        self_closing: !closing && trimmed.ends_with('/'),
    })
}

fn tag_attribute<'source>(tag: MarkupTag<'source>, key: &str) -> Option<(usize, &'source str)> {
    let mut cursor = 0;
    while let Some(attribute) = next_markup_attribute(tag.raw, &mut cursor) {
        if attribute.name.eq_ignore_ascii_case(key) {
            return Some((
                tag.start + 1 + attribute.value_start,
                &tag.raw[attribute.value_start..attribute.value_end],
            ));
        }
    }
    None
}

struct MarkupAttribute<'source> {
    name: &'source str,
    value_start: usize,
    value_end: usize,
}

fn next_markup_attribute_name<'source>(
    raw: &'source str,
    cursor: &mut usize,
) -> Option<&'source str> {
    let bytes = raw.as_bytes();
    while *cursor < bytes.len() && (bytes[*cursor].is_ascii_whitespace() || bytes[*cursor] == b'/')
    {
        *cursor += 1;
    }
    let name_start = *cursor;
    while *cursor < bytes.len()
        && (is_identifier_body(bytes[*cursor])
            || matches!(bytes[*cursor], b':' | b'.' | b'-' | b'@'))
    {
        *cursor += 1;
    }
    if name_start == *cursor {
        *cursor = cursor.saturating_add(1);
        None
    } else {
        Some(&raw[name_start..*cursor])
    }
}

fn next_markup_attribute_value(bytes: &[u8], cursor: &mut usize) -> Option<(usize, usize)> {
    while *cursor < bytes.len() && bytes[*cursor].is_ascii_whitespace() {
        *cursor += 1;
    }
    if *cursor >= bytes.len() || bytes[*cursor] != b'=' {
        return None;
    }
    *cursor += 1;
    while *cursor < bytes.len() && bytes[*cursor].is_ascii_whitespace() {
        *cursor += 1;
    }
    let quote = bytes.get(*cursor).copied();
    if !matches!(quote, Some(b'\'' | b'"')) {
        return None;
    }
    *cursor += 1;
    let value_start = *cursor;
    while *cursor < bytes.len() && Some(bytes[*cursor]) != quote {
        *cursor += 1;
    }
    let value_end = *cursor;
    *cursor = cursor.saturating_add(1);
    Some((value_start, value_end))
}

fn next_markup_attribute<'source>(
    raw: &'source str,
    cursor: &mut usize,
) -> Option<MarkupAttribute<'source>> {
    let bytes = raw.as_bytes();
    while *cursor < bytes.len() {
        let Some(name) = next_markup_attribute_name(raw, cursor) else {
            continue;
        };
        let Some((value_start, value_end)) = next_markup_attribute_value(bytes, cursor) else {
            continue;
        };
        return Some(MarkupAttribute {
            name,
            value_start,
            value_end,
        });
    }
    None
}

fn tag_name_eq(tag: MarkupTag<'_>, expected: &str) -> bool {
    tag.name.eq_ignore_ascii_case(expected)
}

fn find_matching_close(tags: &[MarkupTag<'_>], opening_index: usize) -> Option<(usize, usize)> {
    let opening = tags[opening_index];
    if opening.self_closing {
        return Some((opening_index, opening.end));
    }
    let mut depth = 0_usize;
    for (index, tag) in tags.iter().enumerate().skip(opening_index + 1) {
        if !tag.name.eq_ignore_ascii_case(opening.name) {
            continue;
        }
        if tag.closing {
            if depth == 0 {
                return Some((index, tag.start));
            }
            depth = depth.saturating_sub(1);
        } else if !tag.self_closing {
            depth = depth.saturating_add(1);
        }
    }
    None
}

fn function_like_names(value: &str) -> Vec<(usize, &str)> {
    if specifier_may_carry_credential(value) {
        return Vec::new();
    }
    identifiers(value)
        .into_iter()
        .filter(|(offset, name)| {
            (*offset == 0 || value.as_bytes()[offset - 1] != b'$')
                && value.as_bytes()[offset + name.len()..]
                    .iter()
                    .copied()
                    .find(|byte| !byte.is_ascii_whitespace())
                    == Some(b'(')
        })
        .collect()
}

fn extract_salesforce_markup(builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
    let context = initialize_salesforce_markup(builder)?;
    let source = builder.source();
    for tag in markup_tags(source) {
        builder.check_cancelled()?;
        if !tag.closing {
            scan_salesforce_tag(builder, &context, tag)?;
        }
    }
    if builder.snapshot.language() != SourceLanguage::Aura {
        return Ok(());
    }
    let uncommented = blank_markup_comments(builder, source)?;
    extract_aura_action_refs(
        builder,
        OwnedSourceInput {
            owner: &context.component,
            source: &uncommented,
            offset: 0,
        },
    )
}

/// Opening delimiter of a markup comment.
const MARKUP_COMMENT_OPEN: &str = "<!--";
/// Closing delimiter of a markup comment.
const MARKUP_COMMENT_CLOSE: &str = "-->";

/// Copy `source` with every `<!-- ... -->` comment blanked to spaces (newlines
/// kept), so offsets still address the original text.
fn blank_markup_comments(
    builder: &mut CustomBuilder<'_, '_>,
    source: &str,
) -> Result<String, ExtractError> {
    let mut output = String::new();
    output
        .try_reserve_exact(source.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    let mut cursor = 0;
    let mut next_poll = 0;
    while let Some(open) = source[cursor..]
        .find(MARKUP_COMMENT_OPEN)
        .map(|relative| cursor + relative)
    {
        poll_cancellation(&mut *builder.cancelled, open, &mut next_poll)?;
        let body = open + MARKUP_COMMENT_OPEN.len();
        let close = source[body..]
            .find(MARKUP_COMMENT_CLOSE)
            .map_or(source.len(), |relative| {
                body + relative + MARKUP_COMMENT_CLOSE.len()
            });
        output.push_str(&source[cursor..open]);
        for character in source[open..close].chars() {
            if character == '\n' {
                output.push('\n');
            } else {
                output.extend(std::iter::repeat_n(' ', character.len_utf8()));
            }
        }
        cursor = close;
    }
    output.push_str(&source[cursor..]);
    Ok(output)
}

struct SalesforceMarkupContext {
    name: String,
    component: SymbolId,
}

fn initialize_salesforce_markup(
    builder: &mut CustomBuilder<'_, '_>,
) -> Result<SalesforceMarkupContext, ExtractError> {
    let language = builder.snapshot.language();
    let name = basename_stem(builder.path()).to_owned();
    let extension = builder
        .path()
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_owned();
    let component_kind =
        if language == SourceLanguage::Aura && !matches!(extension.as_str(), "cmp" | "app") {
            SymbolKind::Resource
        } else {
            SymbolKind::Component
        };
    let end = builder
        .source()
        .find('\n')
        .map_or(builder.source().len(), |index| index.saturating_add(1))
        .max(1)
        .min(builder.source().len());
    let component = builder.add_symbol(
        CustomSymbolInput::new(component_kind, &name, name.clone())
            .at(0, end)
            .with_options(SymbolOptions {
                body_search_text: format!("salesforce {} {name}", language.as_str()),
                export: SymbolExportFlags::named(true),
                visibility: Some(Visibility::Public),
                ..SymbolOptions::default()
            }),
    )?;
    if language == SourceLanguage::Visualforce && extension.eq_ignore_ascii_case("page") {
        let route = format!("/apex/{name}");
        builder.add_symbol(
            CustomSymbolInput::new(SymbolKind::Route, &route, route.clone())
                .at(0, end)
                .with_options(SymbolOptions {
                    body_search_text: format!("route {route}"),
                    export: SymbolExportFlags::named(true),
                    visibility: Some(Visibility::Public),
                    parent: Some(component.clone()),
                    ..SymbolOptions::default()
                }),
        )?;
    }
    Ok(SalesforceMarkupContext { name, component })
}

fn scan_salesforce_tag(
    builder: &mut CustomBuilder<'_, '_>,
    context: &SalesforceMarkupContext,
    tag: MarkupTag<'_>,
) -> Result<(), ExtractError> {
    scan_salesforce_attribute(builder, context, &tag)?;
    scan_salesforce_component_reference(builder, context, &tag)?;
    scan_salesforce_controller_references(builder, context, &tag)?;
    if builder.snapshot.language() == SourceLanguage::Visualforce {
        scan_visualforce_action(builder, context, &tag)?;
    }
    Ok(())
}

/// Visualforce invokes controller methods only through `action="{!name}"`;
/// other `{!expr}` merge fields read values rather than call.
fn scan_visualforce_action(
    builder: &mut CustomBuilder<'_, '_>,
    context: &SalesforceMarkupContext,
    tag: &MarkupTag<'_>,
) -> Result<(), ExtractError> {
    let Some((offset, value)) = tag_attribute(*tag, "action") else {
        return Ok(());
    };
    let Some((relative, name)) = salesforce_merge_field(value) else {
        return Ok(());
    };
    builder.add_reference(
        CustomReferenceInput::new(Some(context.component.clone()), name, ReferenceKind::Calls)
            .at(offset + relative, offset + relative + name.len()),
    )
}

/// The identifier of an exact `{!name}` merge field, with its offset.
fn salesforce_merge_field(value: &str) -> Option<(usize, &str)> {
    let leading = value.len() - value.trim_start().len();
    let (offset, name) = merge_field_body(value.trim())?;
    is_salesforce_identifier(name).then_some((leading + offset, name))
}

const fn is_salesforce_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn scan_salesforce_attribute(
    builder: &mut CustomBuilder<'_, '_>,
    context: &SalesforceMarkupContext,
    tag: &MarkupTag<'_>,
) -> Result<(), ExtractError> {
    if !tag.name.eq_ignore_ascii_case("aura:attribute") {
        return Ok(());
    }
    let Some((name_offset, field_name)) = tag_attribute(*tag, "name") else {
        return Ok(());
    };
    if specifier_may_carry_credential(field_name) {
        return Ok(());
    }
    let field = builder.add_symbol(
        CustomSymbolInput::new(
            SymbolKind::Field,
            field_name,
            format!("{}::{field_name}", context.name),
        )
        .at(name_offset, name_offset + field_name.len())
        .with_options(SymbolOptions {
            signature: tag_attribute(*tag, "type")
                .and_then(|(_, type_name)| salesforce_type_signature(type_name)),
            body_search_text: format!("field {field_name}"),
            parent: Some(context.component.clone()),
            ..SymbolOptions::default()
        }),
    )?;
    if let Some((type_offset, type_name)) = tag_attribute(*tag, "type")
        .filter(|(_, type_name)| !specifier_may_carry_credential(type_name))
    {
        let reference = salesforce_type_head(type_name);
        if is_qualified_name(reference) {
            let relative = type_name.find(reference).unwrap_or(0);
            builder.add_reference(
                CustomReferenceInput::new(Some(field), reference, ReferenceKind::TypeOf).at(
                    type_offset + relative,
                    type_offset + relative + reference.len(),
                ),
            )?;
        }
    }
    Ok(())
}

/// The referenced type of an Aura attribute type: `List<Account>` -> `List`,
/// `Opportunity[]` -> `Opportunity`, `Schema.Account` -> `Schema`.
fn salesforce_type_head(type_name: &str) -> &str {
    type_name
        .trim()
        .trim_end_matches("[]")
        .split(['.', '<'])
        .next()
        .unwrap_or_default()
        .trim()
}

/// A literal-free Aura attribute type kept as the field signature.
fn salesforce_type_signature(type_name: &str) -> Option<String> {
    let type_name = type_name.trim();
    (!type_name.is_empty()
        && type_name.len() <= MAX_SALESFORCE_TYPE_BYTES
        && !specifier_may_carry_credential(type_name)
        && type_name.bytes().all(|byte| {
            is_salesforce_word_byte(byte)
                || matches!(byte, b'.' | b'<' | b'>' | b',' | b'[' | b']' | b' ')
        })
        && !type_name
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .any(looks_sensitive))
    .then(|| type_name.to_owned())
}

/// Longest Aura attribute type retained as a field signature.
const MAX_SALESFORCE_TYPE_BYTES: usize = 256;

fn scan_salesforce_component_reference(
    builder: &mut CustomBuilder<'_, '_>,
    context: &SalesforceMarkupContext,
    tag: &MarkupTag<'_>,
) -> Result<(), ExtractError> {
    let Some(raw_name) = tag
        .name
        .get(2..)
        .filter(|_| tag.name[..2].eq_ignore_ascii_case("c:"))
    else {
        return Ok(());
    };
    let reference = salesforce_component_name(raw_name);
    builder.add_reference(
        CustomReferenceInput::new(
            Some(context.component.clone()),
            &reference,
            ReferenceKind::References,
        )
        .at(tag.start + 3, tag.start + 3 + raw_name.len()),
    )?;
    builder.add_import_binding(
        &CustomImportInput::new(None, crate::SALESFORCE_COMPONENT_MODULE)
            .with_kind(ImportBindingKind::Named)
            .binding(raw_name, &reference)
            .at(tag.start + 3, tag.start + 3 + raw_name.len()),
    )
}

fn scan_salesforce_controller_references(
    builder: &mut CustomBuilder<'_, '_>,
    context: &SalesforceMarkupContext,
    tag: &MarkupTag<'_>,
) -> Result<(), ExtractError> {
    for key in ["controller", "extensions"] {
        let Some((offset, value)) = tag_attribute(*tag, key) else {
            continue;
        };
        for candidate in value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty() && is_qualified_name(item))
        {
            builder.check_cancelled()?;
            let relative = value.find(candidate).unwrap_or(0);
            builder.add_reference(
                CustomReferenceInput::new(
                    Some(context.component.clone()),
                    candidate,
                    ReferenceKind::References,
                )
                .at(offset + relative, offset + relative + candidate.len()),
            )?;
            builder.add_import_binding(
                &CustomImportInput::new(None, crate::SALESFORCE_CONTROLLER_MODULE)
                    .with_kind(ImportBindingKind::Namespace)
                    .binding(candidate, candidate)
                    .at(offset + relative, offset + relative + candidate.len()),
            )?;
        }
    }
    Ok(())
}

fn salesforce_component_name(raw: &str) -> String {
    raw.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect()
}

/// Aura invokes client-controller actions only as `{!c.name}` or
/// `{!controller.name}`; value-provider expressions (`{!v.rows}`) read data.
fn extract_aura_action_refs(
    builder: &mut CustomBuilder<'_, '_>,
    input: OwnedSourceInput<'_, '_>,
) -> Result<(), ExtractError> {
    let mut cursor = 0;
    let mut next_poll = 0;
    while let Some(relative) = input.source[cursor..].find(MERGE_FIELD_OPEN) {
        let open = cursor + relative;
        poll_cancellation(&mut *builder.cancelled, open, &mut next_poll)?;
        let body = open + MERGE_FIELD_OPEN.len();
        let Some(close_relative) = input.source[body..].find('}') else {
            break;
        };
        let close = body + close_relative;
        if let Some((relative_name, name)) = aura_action_name(&input.source[open..=close]) {
            let start = input.offset + open + relative_name;
            builder.add_reference(
                CustomReferenceInput::new(Some(input.owner.clone()), name, ReferenceKind::Calls)
                    .at(start, start + name.len()),
            )?;
        }
        cursor = close + 1;
    }
    Ok(())
}

/// The action of an exact `{!c.name}` / `{!controller.name}` expression.
fn aura_action_name(expression: &str) -> Option<(usize, &str)> {
    let (offset, inner) = merge_field_body(expression)?;
    AURA_ACTION_PROVIDERS.into_iter().find_map(|provider| {
        let name = inner.strip_prefix(provider)?;
        is_salesforce_identifier(name).then_some((offset + provider.len(), name))
    })
}

/// Client-controller value providers whose members are invocable actions.
const AURA_ACTION_PROVIDERS: [&str; 2] = ["c.", "controller."];
/// Opening delimiter of a Salesforce `{!expr}` merge field.
const MERGE_FIELD_OPEN: &str = "{!";

/// The trimmed body of an exact `{!...}` merge field and its byte offset.
fn merge_field_body(value: &str) -> Option<(usize, &str)> {
    let body = value.strip_prefix(MERGE_FIELD_OPEN)?.strip_suffix('}')?;
    let leading = body.len() - body.trim_start().len();
    Some((MERGE_FIELD_OPEN.len() + leading, body.trim()))
}

fn is_salesforce_identifier(name: &str) -> bool {
    name.bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic())
        && name.bytes().all(is_salesforce_word_byte)
}

#[derive(Clone)]
struct VbScope {
    id: SymbolId,
    qualified_name: String,
    block: VbBlock,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VbBlock {
    Container,
    Routine,
    Struct,
    Enum,
}

fn extract_vb6(builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
    if builder.source().is_empty() {
        return Ok(());
    }
    if builder.path().to_ascii_lowercase().ends_with(".vbp") {
        return extract_vb6_project(builder);
    }
    let source = builder.source();
    if vb6_container_name(source)
        .as_deref()
        .is_some_and(specifier_may_carry_credential)
    {
        return Ok(());
    }
    let mut state = initialize_vb6_container(builder, source)?;
    for (line_start, raw_line) in physical_lines(source) {
        builder.check_cancelled()?;
        scan_vb6_line(
            builder,
            &mut state,
            VbSourceLine {
                start: line_start,
                raw: raw_line,
                text: vb6_strip_comment(raw_line).trim(),
            },
        )?;
    }
    Ok(())
}

struct VbScanState {
    container_kind: SymbolKind,
    scopes: Vec<VbScope>,
}

#[derive(Clone, Copy)]
struct VbSourceLine<'source> {
    start: usize,
    raw: &'source str,
    text: &'source str,
}

fn initialize_vb6_container(
    builder: &mut CustomBuilder<'_, '_>,
    source: &str,
) -> Result<VbScanState, ExtractError> {
    let container_name =
        vb6_container_name(source).unwrap_or_else(|| basename_stem(builder.path()).to_owned());
    let extension = builder.path().rsplit('.').next().unwrap_or_default();
    let container_kind = if matches!(extension, "frm" | "ctl" | "dob" | "dsr" | "pag") {
        SymbolKind::Component
    } else if extension.eq_ignore_ascii_case("cls") {
        SymbolKind::Class
    } else {
        SymbolKind::Module
    };
    // The module, class or form is the whole file: its span covers the designer
    // header and every member declared after it.
    let container = builder.add_symbol(
        CustomSymbolInput::new(container_kind, &container_name, container_name.clone())
            .at(0, source.len())
            .with_options(SymbolOptions {
                body_search_text: format!("{} {container_name}", container_kind.as_str()),
                export: SymbolExportFlags::named(true),
                visibility: Some(Visibility::Public),
                ..SymbolOptions::default()
            }),
    )?;
    Ok(VbScanState {
        container_kind,
        scopes: vec![VbScope {
            id: container,
            qualified_name: container_name,
            block: VbBlock::Container,
        }],
    })
}

fn scan_vb6_line(
    builder: &mut CustomBuilder<'_, '_>,
    state: &mut VbScanState,
    line: VbSourceLine<'_>,
) -> Result<(), ExtractError> {
    let lower = line.text.to_ascii_lowercase();
    if line.text.is_empty() || lower.starts_with("attribute vb_") || lower.starts_with("version ") {
        return Ok(());
    }
    if let Some(block) = vb6_end_block(line.text) {
        close_vb6_scope(&mut state.scopes, block);
        return Ok(());
    }
    if let Some(declaration) = vb6_declaration(
        line.text,
        state.scopes.last().map(|scope| scope.block),
        state.container_kind,
    ) {
        add_vb6_declaration(builder, state, ParsedVbDeclaration { line, declaration })?;
        return Ok(());
    }
    let Some(routine) = state
        .scopes
        .iter()
        .rev()
        .find(|scope| scope.block == VbBlock::Routine)
    else {
        return Ok(());
    };
    let indent = line.raw.find(line.text).unwrap_or(0);
    for (offset, name) in vb6_calls(line.text) {
        let raw_offset = indent + offset;
        builder.add_reference(
            CustomReferenceInput::new(Some(routine.id.clone()), name, ReferenceKind::Calls).at(
                line.start + raw_offset,
                line.start + raw_offset + name.len(),
            ),
        )?;
    }
    Ok(())
}

fn close_vb6_scope(scopes: &mut Vec<VbScope>, block: VbBlock) {
    while scopes.len() > 1 {
        if scopes.pop().is_some_and(|scope| scope.block == block) {
            break;
        }
    }
}

fn add_vb6_declaration(
    builder: &mut CustomBuilder<'_, '_>,
    state: &mut VbScanState,
    input: ParsedVbDeclaration<'_>,
) -> Result<(), ExtractError> {
    let ParsedVbDeclaration { line, declaration } = input;
    let offset = line.raw.find(declaration.name).unwrap_or(0);
    let start = line.start + offset;
    let parent = state.scopes.last().cloned();
    let qualified = parent.as_ref().map_or_else(
        || declaration.name.to_owned(),
        |scope| format!("{}::{}", scope.qualified_name, declaration.name),
    );
    let id = builder.add_symbol(
        CustomSymbolInput::new(declaration.kind, declaration.name, qualified.clone())
            .at(start, start + declaration.name.len())
            .with_options(SymbolOptions {
                body_search_text: format!("{} {}", declaration.kind.as_str(), declaration.name),
                export: SymbolExportFlags::named(
                    declaration.visibility == Some(Visibility::Public),
                ),
                visibility: declaration.visibility,
                static_member: declaration.static_member,
                parent: parent.map(|scope| scope.id),
                ..SymbolOptions::default()
            }),
    )?;
    if let Some(block) = declaration.block {
        state.scopes.push(VbScope {
            id,
            qualified_name: qualified,
            block,
        });
    }
    Ok(())
}

struct ParsedVbDeclaration<'source> {
    line: VbSourceLine<'source>,
    declaration: VbDeclaration<'source>,
}

fn extract_vb6_project(builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
    let source = builder.source();
    for (line_start, raw_line) in physical_lines(source) {
        let line = raw_line.trim();
        let Some(separator) = line.find('=') else {
            continue;
        };
        let key = line[..separator].trim().to_ascii_lowercase();
        if !matches!(
            key.as_str(),
            "module" | "class" | "form" | "usercontrol" | "userdocument" | "designer"
        ) {
            continue;
        }
        let value = line[separator + 1..].trim();
        let Some(name) = vb6_project_load_name(value) else {
            continue;
        };
        let offset = raw_line.find(name).unwrap_or(0);
        let id = builder.add_symbol(
            CustomSymbolInput::new(
                SymbolKind::Import,
                name,
                format!("{}::{name}", builder.path()),
            )
            .at(line_start + offset, line_start + offset + name.len())
            .with_options(SymbolOptions {
                body_search_text: format!("import {name}"),
                ..SymbolOptions::default()
            }),
        )?;
        builder.add_import(
            &CustomImportInput::new(Some(id), value)
                .binding(name, name)
                .at(line_start + offset, line_start + offset + name.len()),
        )?;
    }
    Ok(())
}

/// A project load name, screening the complete operand before its file projection.
fn vb6_project_load_name(value: &str) -> Option<&str> {
    if specifier_may_carry_credential(value) {
        return None;
    }
    let name = value
        .split(';')
        .next_back()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(value);
    (!name.is_empty()).then_some(name)
}

/// The module/class/form name declared by `Attribute VB_Name` or by the form's
/// `Begin VB.<Type> <Name>` designer block, whichever comes first.
fn vb6_container_name(source: &str) -> Option<String> {
    for (_, line) in physical_lines(source) {
        let trimmed = line.trim();
        if trimmed
            .to_ascii_lowercase()
            .starts_with("attribute vb_name")
        {
            let (_, name) = quoted_values(trimmed).into_iter().next()?;
            return Some(name.to_owned());
        }
        let words = identifiers(trimmed);
        if words.len() >= 3
            && words[0].1.eq_ignore_ascii_case("begin")
            && words[1].1.eq_ignore_ascii_case("vb")
        {
            return Some(words.last()?.1.to_owned());
        }
    }
    None
}

struct VbDeclaration<'source> {
    kind: SymbolKind,
    name: &'source str,
    block: Option<VbBlock>,
    visibility: Option<Visibility>,
    static_member: bool,
}

fn vb6_declaration(
    line: &str,
    parent: Option<VbBlock>,
    container_kind: SymbolKind,
) -> Option<VbDeclaration<'_>> {
    let tokens = identifiers(line);
    let mut index = 0;
    let visibility = tokens
        .get(index)
        .and_then(|(_, token)| vb6_visibility(token));
    if visibility.is_some() {
        index += 1;
    }
    let static_member = tokens
        .get(index)
        .is_some_and(|(_, token)| token.eq_ignore_ascii_case("static"));
    if static_member {
        index += 1;
    }
    vb6_keyword_declaration(VbDeclarationContext {
        tokens: &tokens,
        index,
        parent,
        container_kind,
        visibility,
        static_member,
    })
}

#[derive(Clone, Copy)]
struct VbDeclarationContext<'tokens, 'source> {
    tokens: &'tokens [(usize, &'source str)],
    index: usize,
    parent: Option<VbBlock>,
    container_kind: SymbolKind,
    visibility: Option<Visibility>,
    static_member: bool,
}

fn vb6_keyword_declaration<'source>(
    context: VbDeclarationContext<'_, 'source>,
) -> Option<VbDeclaration<'source>> {
    let lower = context.tokens.get(context.index)?.1.to_ascii_lowercase();
    match lower.as_str() {
        "declare" => vb6_declare(context),
        "sub" | "function" => vb6_named_declaration(
            context,
            VbNamedForm {
                kind: vb6_routine_kind(context.container_kind),
                name_offset: 1,
                block: Some(VbBlock::Routine),
            },
        ),
        "dim" | "public" | "private" | "friend" => {
            vb6_variable_declaration(context, context.index + 1)
        }
        keyword => match VB6_NAMED_KEYWORDS
            .iter()
            .find(|(candidate, _)| *candidate == keyword)
        {
            Some(&(_, form)) => vb6_named_declaration(context, form),
            None => vb6_implicit_declaration(context),
        },
    }
}

/// The shape of a declaration whose keyword names its symbol a fixed number
/// of tokens later (`Type Point`, `Property Get Name`).
#[derive(Clone, Copy)]
struct VbNamedForm {
    kind: SymbolKind,
    name_offset: usize,
    block: Option<VbBlock>,
}

/// Lowercase VB6 keywords that declare one named symbol of a fixed kind.
const VB6_NAMED_KEYWORDS: &[(&str, VbNamedForm)] = &[
    (
        "property",
        VbNamedForm {
            kind: SymbolKind::Property,
            name_offset: 2,
            block: Some(VbBlock::Routine),
        },
    ),
    (
        "type",
        VbNamedForm {
            kind: SymbolKind::Struct,
            name_offset: 1,
            block: Some(VbBlock::Struct),
        },
    ),
    (
        "enum",
        VbNamedForm {
            kind: SymbolKind::Enum,
            name_offset: 1,
            block: Some(VbBlock::Enum),
        },
    ),
    (
        "const",
        VbNamedForm {
            kind: SymbolKind::Constant,
            name_offset: 1,
            block: None,
        },
    ),
];

/// A `Sub` or `Function` is a function in a standard module and a method in a
/// class or form.
fn vb6_routine_kind(container_kind: SymbolKind) -> SymbolKind {
    if container_kind == SymbolKind::Module {
        SymbolKind::Function
    } else {
        SymbolKind::Method
    }
}

fn vb6_named_declaration<'source>(
    context: VbDeclarationContext<'_, 'source>,
    form: VbNamedForm,
) -> Option<VbDeclaration<'source>> {
    Some(VbDeclaration {
        kind: form.kind,
        name: context.tokens.get(context.index + form.name_offset)?.1,
        block: form.block,
        visibility: context.visibility,
        static_member: context.static_member,
    })
}

/// A variable declared by the token at `name_index`: a routine local, or a
/// field of the enclosing module or type.
fn vb6_variable_declaration<'source>(
    context: VbDeclarationContext<'_, 'source>,
    name_index: usize,
) -> Option<VbDeclaration<'source>> {
    Some(VbDeclaration {
        kind: if context.parent == Some(VbBlock::Routine) {
            SymbolKind::Variable
        } else {
            SymbolKind::Field
        },
        name: vb6_variable_name(context.tokens, name_index)?,
        block: None,
        visibility: context.visibility,
        static_member: context.static_member,
    })
}

/// A line without a declaration keyword: a variable after a visibility
/// modifier (`Public Count As Long`), an enum member, or a field of a
/// user-defined type.
fn vb6_implicit_declaration<'source>(
    context: VbDeclarationContext<'_, 'source>,
) -> Option<VbDeclaration<'source>> {
    if context.visibility.is_some() {
        return vb6_variable_declaration(context, context.index);
    }
    let kind = match context.parent {
        Some(VbBlock::Enum) => SymbolKind::EnumMember,
        Some(VbBlock::Struct) => SymbolKind::Field,
        _ => return None,
    };
    Some(VbDeclaration {
        kind,
        name: context.tokens.get(context.index)?.1,
        block: None,
        visibility: None,
        static_member: false,
    })
}

/// VB6 modifier that subscribes an object variable to its events
/// (`Private WithEvents mCustomer As Customer`); it is not the variable name.
const VB6_WITH_EVENTS: &str = "withevents";

/// The variable declared by the token at `index`, past a `WithEvents`
/// modifier (v1 `parseVariable`).
fn vb6_variable_name<'source>(
    tokens: &[(usize, &'source str)],
    index: usize,
) -> Option<&'source str> {
    let (_, token) = tokens.get(index)?;
    if token.eq_ignore_ascii_case(VB6_WITH_EVENTS) {
        tokens.get(index + 1).map(|(_, name)| *name)
    } else {
        Some(token)
    }
}

/// `[Private] Declare [PtrSafe] Sub|Function Name Lib "dll" ...` binds an
/// external routine: an import named `Name` (v1 `parseDeclare`). It opens no
/// scope, and the library literal is never retained.
fn vb6_declare<'source>(
    context: VbDeclarationContext<'_, 'source>,
) -> Option<VbDeclaration<'source>> {
    let mut index = context.index + 1;
    if context.tokens.get(index)?.1.eq_ignore_ascii_case("ptrsafe") {
        index += 1;
    }
    let routine = context.tokens.get(index)?.1;
    if !routine.eq_ignore_ascii_case("sub") && !routine.eq_ignore_ascii_case("function") {
        return None;
    }
    Some(VbDeclaration {
        kind: SymbolKind::Import,
        name: context.tokens.get(index + 1)?.1,
        block: None,
        visibility: context.visibility,
        static_member: false,
    })
}

fn vb6_visibility(value: &str) -> Option<Visibility> {
    if value.eq_ignore_ascii_case("private") {
        Some(Visibility::Private)
    } else if value.eq_ignore_ascii_case("friend") {
        Some(Visibility::Internal)
    } else if value.eq_ignore_ascii_case("public") || value.eq_ignore_ascii_case("global") {
        Some(Visibility::Public)
    } else {
        None
    }
}

fn vb6_end_block(line: &str) -> Option<VbBlock> {
    let words = identifiers(line);
    if words.first()?.1.eq_ignore_ascii_case("end") {
        let keyword = words.get(1)?.1;
        if keyword.eq_ignore_ascii_case("type") {
            Some(VbBlock::Struct)
        } else if keyword.eq_ignore_ascii_case("enum") {
            Some(VbBlock::Enum)
        } else if matches!(
            keyword.to_ascii_lowercase().as_str(),
            "sub" | "function" | "property"
        ) {
            Some(VbBlock::Routine)
        } else {
            None
        }
    } else {
        None
    }
}

/// Calls on one routine statement line: the statement's own callee
/// (`Call X`, or the paren-less `Helper i`) and its first parenthesized call
/// (`x = Other(i)`, `Helper Other(i)`), in source order and deduplicated.
fn vb6_calls(line: &str) -> Vec<(usize, &str)> {
    let line = line.trim_start();
    if word_is(line, "Rem") {
        return Vec::new();
    }
    // Detection runs on a copy whose string contents are blanked, so
    // `MsgBox "Use (x)"` never reads `Use(` from inside the literal; offsets
    // are unchanged and names are sliced from the original line.
    let masked = vb6_mask_strings(line);
    let statement = if word_is(&masked, "Call") {
        let start = skip_vb6_blanks(&masked, "Call".len());
        vb6_member_chain(&masked, start).map(|(callee, _)| callee)
    } else {
        vb6_bare_call(&masked)
    };
    let nested = function_like_names(&masked)
        .into_iter()
        .next()
        .and_then(vb6_callable_name);
    let mut calls = Vec::new();
    for (offset, name) in [statement, nested].into_iter().flatten() {
        if calls.iter().any(|(existing, _)| *existing == offset) {
            continue;
        }
        if let Some(name) = line.get(offset..offset + name.len()) {
            calls.push((offset, name));
        }
    }
    calls.sort_by_key(|(offset, _)| *offset);
    calls
}

/// Whether `line` starts with the keyword `word` as a whole word.
fn word_is(line: &str, word: &str) -> bool {
    line.get(..word.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(word))
        && line
            .as_bytes()
            .get(word.len())
            .is_none_or(|byte| !is_identifier_body(*byte))
}

/// VB statement keywords that start a line without calling anything (v1
/// `VB6_SKIP_CALLS` plus the remaining control-flow and I/O statements).
const VB6_STATEMENT_KEYWORDS: &[&str] = &[
    "call", "case", "close", "debug", "dim", "do", "else", "elseif", "end", "erase", "exit", "for",
    "get", "gosub", "goto", "if", "input", "let", "loop", "next", "on", "open", "option", "print",
    "private", "public", "put", "redim", "resume", "return", "select", "set", "static", "stop",
    "wend", "while", "with", "write",
];

fn vb6_callable_name((offset, name): (usize, &str)) -> Option<(usize, &str)> {
    (!VB6_STATEMENT_KEYWORDS
        .iter()
        .any(|keyword| name.eq_ignore_ascii_case(keyword)))
    .then_some((offset, name))
}

/// A paren-less statement call such as `Helper i`, `MsgBox "hi"`, or
/// `obj.Save arg` (the callee is the last member, as in the parenthesized
/// form). v1 called the line's first identifier; assignments (`x = 1`,
/// `Me.Caption = ...`) and labels (`Retry:`) are not calls.
fn vb6_bare_call(line: &str) -> Option<(usize, &str)> {
    let (callee, end) = vb6_member_chain(line, 0)?;
    let next = line[end..]
        .bytes()
        .find(|byte| !matches!(byte, b' ' | b'\t'));
    (!matches!(next, Some(b'=' | b':' | b'!' | b'(' | b'.'))).then_some(callee)
}

/// The terminal member of the dotted chain starting exactly at `start`
/// (`obj.Save` names `Save`) and the chain's end, unless its head is a VB
/// statement keyword.
fn vb6_member_chain(line: &str, start: usize) -> Option<((usize, &str), usize)> {
    let rest = line.get(start..)?;
    let (offset, first) = first_identifier(rest)?;
    if offset != 0 {
        return None;
    }
    vb6_callable_name((start, first))?;
    let mut callee = (start, first);
    let mut cursor = start + first.len();
    while line.as_bytes().get(cursor) == Some(&b'.') {
        let (member_offset, member) = first_identifier(&line[cursor + 1..])?;
        if member_offset != 0 {
            return None;
        }
        callee = (cursor + 1, member);
        cursor = cursor + 1 + member.len();
    }
    Some((callee, cursor))
}

fn skip_vb6_blanks(line: &str, start: usize) -> usize {
    start
        + line.get(start..).map_or(0, |rest| {
            rest.len() - rest.trim_start_matches([' ', '\t']).len()
        })
}

/// `line` with the contents of every `"..."` literal replaced by one space per
/// byte, so byte offsets are preserved (VB escapes a quote by doubling it,
/// which this toggling handles naturally).
fn vb6_mask_strings(line: &str) -> String {
    let mut masked = String::with_capacity(line.len());
    let mut inside = false;
    for character in line.chars() {
        if character == '"' {
            inside = !inside;
            masked.push(character);
        } else if inside {
            masked.extend(std::iter::repeat_n(' ', character.len_utf8()));
        } else {
            masked.push(character);
        }
    }
    masked
}

/// The line before its `'` comment; an apostrophe inside a string literal does
/// not start a comment.
fn vb6_strip_comment(line: &str) -> &str {
    let mut inside = false;
    for (index, byte) in line.bytes().enumerate() {
        match byte {
            b'"' => inside = !inside,
            b'\'' if !inside => return &line[..index],
            _ => {}
        }
    }
    line
}

fn extract_xml(builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
    let source = builder.source();
    let tags = markup_tags(source);
    if let Some((_, mapper)) = tags
        .iter()
        .copied()
        .enumerate()
        .find(|(_, tag)| !tag.closing && tag_name_eq(*tag, "mapper"))
        && let Some((namespace_offset, namespace)) = tag_attribute(mapper, "namespace")
    {
        return extract_mybatis_mapper(
            builder,
            MybatisMapperInput {
                tags: &tags,
                namespace,
                namespace_offset,
            },
        );
    }
    if tags
        .iter()
        .any(|tag| !tag.closing && tag_name_eq(*tag, "configuration"))
        && tags
            .iter()
            .any(|tag| !tag.closing && tag_name_eq(*tag, "mappers"))
    {
        return extract_mybatis_config(builder, &tags);
    }
    Ok(())
}

fn extract_mybatis_mapper(
    builder: &mut CustomBuilder<'_, '_>,
    input: MybatisMapperInput<'_>,
) -> Result<(), ExtractError> {
    if specifier_may_carry_credential(input.namespace) {
        return Ok(());
    }
    let simple_namespace = input
        .namespace
        .rsplit('.')
        .next()
        .unwrap_or(input.namespace);
    let module = builder.add_symbol(
        CustomSymbolInput::new(
            SymbolKind::Namespace,
            simple_namespace,
            input.namespace.to_owned(),
        )
        .at(
            input.namespace_offset,
            input.namespace_offset + input.namespace.len(),
        )
        .with_options(SymbolOptions {
            body_search_text: format!("mybatis mapper {simple_namespace}"),
            export: SymbolExportFlags::named(true),
            visibility: Some(Visibility::Public),
            ..SymbolOptions::default()
        }),
    )?;
    let state = MybatisMapperState {
        tags: input.tags,
        namespace: simple_namespace,
        package: input
            .namespace
            .rsplit_once('.')
            .map(|(package, _)| package)
            .filter(|package| !package.is_empty()),
        module,
    };
    for (index, tag) in input.tags.iter().copied().enumerate() {
        builder.check_cancelled()?;
        scan_mybatis_mapper_tag(builder, &state, IndexedMarkupTag { index, tag })?;
    }
    Ok(())
}

struct MybatisMapperState<'source> {
    tags: &'source [MarkupTag<'source>],
    namespace: &'source str,
    package: Option<&'source str>,
    module: SymbolId,
}

#[derive(Clone, Copy)]
struct IndexedMarkupTag<'source> {
    index: usize,
    tag: MarkupTag<'source>,
}

fn scan_mybatis_mapper_tag(
    builder: &mut CustomBuilder<'_, '_>,
    state: &MybatisMapperState<'_>,
    input: IndexedMarkupTag<'_>,
) -> Result<(), ExtractError> {
    if input.tag.closing {
        return Ok(());
    }
    let is_statement = matches_ignore_ascii_case(
        input.tag.name,
        &["select", "insert", "update", "delete", "sql"],
    );
    let is_mapping = matches_ignore_ascii_case(input.tag.name, &["resultMap", "parameterMap"]);
    if !is_statement && !is_mapping {
        return Ok(());
    }
    let Some((id_offset, id)) = tag_attribute(input.tag, "id") else {
        return Ok(());
    };
    if !is_qualified_name(id) {
        return Ok(());
    }
    let kind = if is_statement {
        SymbolKind::Method
    } else {
        SymbolKind::TypeAlias
    };
    let owner = builder.add_symbol(
        CustomSymbolInput::new(kind, id, format!("{}::{id}", state.namespace))
            .at(id_offset, id_offset + id.len())
            .with_options(SymbolOptions {
                body_search_text: format!("mybatis {} {id}", input.tag.name),
                export: SymbolExportFlags::named(is_statement),
                visibility: is_statement.then_some(Visibility::Public),
                parent: Some(state.module.clone()),
                ..SymbolOptions::default()
            }),
    )?;
    add_mybatis_tag_references(
        builder,
        MybatisTagReferenceInput {
            state,
            tag: input,
            owner: &owner,
            statement: id,
            is_mapping,
        },
    )?;
    Ok(())
}

#[derive(Clone, Copy)]
struct MybatisTagReferenceInput<'source, 'owner> {
    state: &'owner MybatisMapperState<'source>,
    tag: IndexedMarkupTag<'source>,
    owner: &'owner SymbolId,
    statement: &'source str,
    is_mapping: bool,
}

fn add_mybatis_tag_references(
    builder: &mut CustomBuilder<'_, '_>,
    input: MybatisTagReferenceInput<'_, '_>,
) -> Result<(), ExtractError> {
    add_mybatis_type_reference(builder, &input)?;
    add_mybatis_named_references(builder, &input)?;
    add_mybatis_body_reference(builder, input)
}

fn add_mybatis_type_reference(
    builder: &mut CustomBuilder<'_, '_>,
    input: &MybatisTagReferenceInput<'_, '_>,
) -> Result<(), ExtractError> {
    if input.is_mapping
        && let Some((offset, target)) = tag_attribute(input.tag.tag, "type")
    {
        builder.add_reference(
            CustomReferenceInput::new(
                Some(input.owner.clone()),
                target.rsplit('.').next().unwrap_or(target),
                ReferenceKind::TypeOf,
            )
            .at(offset, offset + target.len()),
        )?;
    }
    Ok(())
}

fn add_mybatis_named_references(
    builder: &mut CustomBuilder<'_, '_>,
    input: &MybatisTagReferenceInput<'_, '_>,
) -> Result<(), ExtractError> {
    for key in ["resultMap", "parameterMap", "extends"] {
        if let Some((offset, value)) = tag_attribute(input.tag.tag, key) {
            let reference = mybatis_qualified_reference(input.state.namespace, value);
            builder.add_reference(
                CustomReferenceInput::new(
                    Some(input.owner.clone()),
                    &reference,
                    ReferenceKind::References,
                )
                .at(offset, offset + value.len()),
            )?;
        }
    }
    Ok(())
}

fn add_mybatis_body_reference(
    builder: &mut CustomBuilder<'_, '_>,
    input: MybatisTagReferenceInput<'_, '_>,
) -> Result<(), ExtractError> {
    let Some((_, body_end)) = find_matching_close(input.state.tags, input.tag.index) else {
        return Ok(());
    };
    let body_start = input.tag.tag.end;
    if body_start < body_end {
        extract_mybatis_body_refs(
            builder,
            MybatisBodyInput {
                owner: input.owner,
                namespace: input.state.namespace,
                package: input.state.package,
                statement: input.statement,
                start: body_start,
                end: body_end,
            },
        )?;
    }
    Ok(())
}

fn extract_mybatis_body_refs(
    builder: &mut CustomBuilder<'_, '_>,
    input: MybatisBodyInput<'_, '_>,
) -> Result<(), ExtractError> {
    let body = &builder.source()[input.start..input.end];
    for tag in markup_tags(body) {
        if tag.closing || !tag_name_eq(tag, "include") {
            continue;
        }
        if let Some((offset, refid)) = tag_attribute(tag, "refid") {
            let reference = mybatis_qualified_reference(input.namespace, refid);
            builder.add_reference(
                CustomReferenceInput::new(
                    Some(input.owner.clone()),
                    &reference,
                    ReferenceKind::References,
                )
                .at(input.start + offset, input.start + offset + refid.len()),
            )?;
        }
    }
    let mut cursor = 0;
    let mut seen = BTreeSet::new();
    while let Some(relative) = body[cursor..].find("#{") {
        let open = cursor + relative;
        let content_start = open + 2;
        let Some(close_relative) = body[content_start..].find('}') else {
            break;
        };
        let close = content_start + close_relative;
        let raw = body[content_start..close]
            .split(',')
            .next()
            .unwrap_or_default();
        let parameter = raw.split('.').next().unwrap_or_default().trim();
        if is_qualified_name(parameter) && seen.insert(parameter.to_owned()) {
            let leading = raw.len() - raw.trim_start().len();
            let parameter_start = input.start + content_start + leading;
            let name = format!("{}::{}::{parameter}", input.namespace, input.statement);
            // A packaged JVM mapper parameter is `com.example::OrderMapper::find::id`.
            let jvm_name = input.package.map(|package| format!("{package}::{name}"));
            let reference = CustomReferenceInput::new(
                Some(input.owner.clone()),
                &name,
                ReferenceKind::References,
            )
            .at(parameter_start, parameter_start + parameter.len());
            builder.add_reference(match jvm_name.as_deref() {
                Some(jvm_name) => reference.with_resolution(jvm_name),
                None => reference,
            })?;
        }
        cursor = close + 1;
    }
    Ok(())
}

fn mybatis_qualified_reference(namespace: &str, raw: &str) -> String {
    let mut parts = raw.rsplitn(2, '.');
    let tail = parts.next().unwrap_or(raw);
    let owner = parts
        .next()
        .and_then(|prefix| prefix.rsplit('.').next())
        .unwrap_or(namespace);
    format!("{owner}::{tail}")
}

fn extract_mybatis_config(
    builder: &mut CustomBuilder<'_, '_>,
    tags: &[MarkupTag<'_>],
) -> Result<(), ExtractError> {
    for tag in tags.iter().copied() {
        builder.check_cancelled()?;
        if tag.closing {
            continue;
        }
        if tag_name_eq(tag, "mapper") {
            if let Some((offset, class)) = tag_attribute(tag, "class") {
                let name = class.rsplit('.').next().unwrap_or(class);
                builder.add_reference(
                    CustomReferenceInput::new(None, name, ReferenceKind::References)
                        .at(offset, offset + class.len()),
                )?;
            }
        } else if tag_name_eq(tag, "typeAlias")
            && let Some((offset, alias)) = tag_attribute(tag, "alias")
            && !specifier_may_carry_credential(alias)
        {
            builder.add_symbol(
                CustomSymbolInput::new(
                    SymbolKind::TypeAlias,
                    alias,
                    format!("{}::{alias}", builder.path()),
                )
                .at(offset, offset + alias.len())
                .with_options(SymbolOptions {
                    body_search_text: format!("mybatis type alias {alias}"),
                    export: SymbolExportFlags::named(true),
                    visibility: Some(Visibility::Public),
                    ..SymbolOptions::default()
                }),
            )?;
            if let Some((type_offset, target)) = tag_attribute(tag, "type") {
                builder.add_reference(
                    CustomReferenceInput::new(
                        None,
                        target.rsplit('.').next().unwrap_or(target),
                        ReferenceKind::TypeOf,
                    )
                    .at(type_offset, type_offset + target.len()),
                )?;
            }
        }
    }
    Ok(())
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn extract_anubis(builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
    let source = builder.source();
    let mut state = AnubisScanState {
        root: None,
        behavior: None,
    };
    for (index, (line_start, raw_line)) in physical_lines(source).enumerate() {
        builder.check_cancelled()?;
        let line = strip_line_comment(raw_line, "--").trim();
        if line.is_empty() {
            continue;
        }
        let line = AnubisLine {
            start: line_start,
            number: index.saturating_add(1),
            raw: raw_line,
            text: line,
        };
        scan_anubis_declaration(builder, &mut state, line)?;
        scan_anubis_references(builder, &state, line)?;
    }
    Ok(())
}

struct AnubisScanState {
    root: Option<(SymbolId, String)>,
    behavior: Option<(SymbolId, String)>,
}

#[derive(Clone, Copy)]
struct AnubisLine<'source> {
    start: usize,
    /// One-based physical line number, the disambiguator of handler names.
    number: usize,
    raw: &'source str,
    text: &'source str,
}

fn scan_anubis_declaration(
    builder: &mut CustomBuilder<'_, '_>,
    state: &mut AnubisScanState,
    line: AnubisLine<'_>,
) -> Result<(), ExtractError> {
    if let Some((name_offset, name, kind)) = anubis_root(line.text) {
        let raw_offset = line.raw.find(line.text).unwrap_or(0) + name_offset;
        let qualified = if kind == SymbolKind::Module {
            format!("game.states.{name}")
        } else {
            format!("game.configs.{name}")
        };
        let id = builder.add_symbol(
            CustomSymbolInput::new(kind, name, qualified.clone())
                .at(
                    line.start + raw_offset,
                    line.start + raw_offset + name.len(),
                )
                .with_options(SymbolOptions {
                    body_search_text: format!("anubis {} {name}", kind.as_str()),
                    export: SymbolExportFlags::named(true),
                    visibility: Some(Visibility::Public),
                    ..SymbolOptions::default()
                }),
        )?;
        state.root = Some((id, qualified));
        state.behavior = None;
    } else if let Some((name_offset, name, shape)) = anubis_behavior(line.text) {
        let raw_offset = line.raw.find(line.text).unwrap_or(0) + name_offset;
        let parent = state.root.as_ref().map(|(id, _)| id.clone());
        let prefix = state
            .root
            .as_ref()
            .map_or_else(|| builder.path().to_owned(), |(_, name)| name.clone());
        let id = builder.add_symbol(
            CustomSymbolInput::new(SymbolKind::Method, name, format!("{prefix}::{name}"))
                .at(
                    line.start + raw_offset,
                    line.start + raw_offset + name.len(),
                )
                .with_options(SymbolOptions {
                    body_search_text: format!("anubis {shape} {name}"),
                    parent,
                    ..SymbolOptions::default()
                }),
        )?;
        state.behavior = Some((id, name.to_owned()));
    } else if let Some((name_offset, name, label)) = anubis_handler(line.text) {
        let raw_offset = line.raw.find(line.text).unwrap_or(0) + name_offset;
        let parent = if label == "event" {
            state.root.as_ref().map(|(id, _)| id.clone())
        } else {
            state
                .behavior
                .as_ref()
                .map(|(id, _)| id.clone())
                .or_else(|| state.root.as_ref().map(|(id, _)| id.clone()))
        };
        let prefix = state
            .root
            .as_ref()
            .map_or_else(|| builder.path().to_owned(), |(_, name)| name.clone());
        let display = format!("{label}:{name}");
        // Handlers are qualified by the root alone, so v1's line suffix keeps
        // same-named callbacks of different behavior nodes distinct.
        let qualified = format!("{prefix}::{display}:{}", line.number);
        builder.add_symbol(
            CustomSymbolInput::new(SymbolKind::Method, &display, qualified)
                .at(
                    line.start + raw_offset,
                    line.start + raw_offset + name.len(),
                )
                .with_options(SymbolOptions {
                    body_search_text: format!("anubis {label} {name}"),
                    parent,
                    ..SymbolOptions::default()
                }),
        )?;
        // An event handler belongs to the state, not the preceding behavior
        // node, so later references are the state's own.
        if label == "event" {
            state.behavior = None;
        }
    }
    Ok(())
}

fn scan_anubis_references(
    builder: &mut CustomBuilder<'_, '_>,
    state: &AnubisScanState,
    line: AnubisLine<'_>,
) -> Result<(), ExtractError> {
    let owner = state
        .behavior
        .as_ref()
        .map(|(id, _)| id.clone())
        .or_else(|| state.root.as_ref().map(|(id, _)| id.clone()));
    for (offset, call) in function_like_names(line.text) {
        if anubis_call_skip(call) {
            continue;
        }
        let raw_offset = line.raw.find(line.text).unwrap_or(0) + offset;
        builder.add_reference(
            CustomReferenceInput::new(owner.clone(), call, ReferenceKind::Calls).at(
                line.start + raw_offset,
                line.start + raw_offset + call.len(),
            ),
        )?;
    }
    for (offset, reference) in dotted_references(line.text) {
        let raw_offset = line.raw.find(line.text).unwrap_or(0) + offset;
        builder.add_reference(
            CustomReferenceInput::new(owner.clone(), reference, ReferenceKind::References).at(
                line.start + raw_offset,
                line.start + raw_offset + reference.len(),
            ),
        )?;
    }
    scan_anubis_string_references(builder, owner.as_ref(), line)
}

/// Quoted and `[[long-bracket]]` strings naming BG3 resources or events
/// (`Entity("S_Trigger_<uuid>")`, `SetEntityEvent(me, "RaiseAlarm")`).
fn scan_anubis_string_references(
    builder: &mut CustomBuilder<'_, '_>,
    owner: Option<&SymbolId>,
    line: AnubisLine<'_>,
) -> Result<(), ExtractError> {
    let text_offset = line.start + line.raw.find(line.text).unwrap_or(0);
    let mut strings = quoted_values(line.text);
    strings.extend(bg3_tokens::long_bracket_values(line.text));
    strings.sort_unstable_by_key(|(offset, _)| *offset);
    for (offset, value) in strings {
        for (relative, token) in bg3_tokens::script_string_references(value) {
            let start = text_offset + offset + relative;
            builder.add_reference(
                CustomReferenceInput::new(owner.cloned(), token, ReferenceKind::References)
                    .at(start, start + token.len()),
            )?;
        }
    }
    Ok(())
}

fn anubis_root(line: &str) -> Option<(usize, &str, SymbolKind)> {
    for (prefix, kind) in [
        ("game.states.", SymbolKind::Module),
        ("game.configs.", SymbolKind::Resource),
    ] {
        let Some(prefix_start) = line.find(prefix) else {
            continue;
        };
        let start = prefix_start + prefix.len();
        let Some((_, name)) = first_identifier(&line[start..]) else {
            continue;
        };
        let after = &line[start + name.len()..];
        if after.contains("State") || after.contains("Config") {
            return Some((start, name, kind));
        }
    }
    None
}

fn anubis_behavior(line: &str) -> Option<(usize, &str, &str)> {
    let nodes = line.find("nodes")?;
    let after_nodes = &line[nodes + "nodes".len()..];
    let name = after_nodes
        .strip_prefix('.')
        .and_then(leading_dotted_identifier)
        .unwrap_or("nodes");
    let name_start = if name == "nodes" {
        nodes
    } else {
        line[nodes..].find(name)? + nodes
    };
    let shape = ["Action", "Selector", "Proxy"]
        .into_iter()
        .find(|shape| line.contains(shape))?;
    Some((name_start, name, shape))
}

fn anubis_handler(line: &str) -> Option<(usize, &str, &str)> {
    if let Some(events) = line.find("events.") {
        let start = events + "events.".len();
        let (_, name) = first_identifier(&line[start..])?;
        return line.contains("function").then_some((start, name, "event"));
    }
    for callback in [
        "CanEnter",
        "Valid",
        "OnFinished",
        "OnLeave",
        "OnEnter",
        "OnUpdate",
        "OnFailed",
    ] {
        if let Some(start) = line.find(callback)
            && line[start + callback.len()..].contains("function")
        {
            return Some((start, callback, "callback"));
        }
    }
    None
}

/// `A.B.C` at the start of `value` (a nested behavior-node path).
fn leading_dotted_identifier(value: &str) -> Option<&str> {
    let bytes = value.as_bytes();
    let mut end = 0;
    loop {
        if !bytes.get(end).copied().is_some_and(is_identifier_start) {
            break;
        }
        end += 1;
        while bytes.get(end).copied().is_some_and(is_identifier_body) {
            end += 1;
        }
        if bytes.get(end) != Some(&b'.')
            || !bytes.get(end + 1).copied().is_some_and(is_identifier_start)
        {
            return Some(&value[..end]);
        }
        end += 1;
    }
    None
}

const ANUBIS_CALL_SKIP_NAMES: &[&str] = &[
    "if",
    "function",
    "State",
    "Config",
    "Action",
    "Selector",
    "Proxy",
    "CanEnter",
    "Valid",
    "OnFinished",
    "OnLeave",
    "OnEnter",
    "OnUpdate",
    "OnFailed",
];

fn anubis_call_skip(name: &str) -> bool {
    ANUBIS_CALL_SKIP_NAMES.contains(&name)
}

fn dotted_references(line: &str) -> Vec<(usize, &str)> {
    if specifier_may_carry_credential(line) {
        return Vec::new();
    }
    let bytes = line.as_bytes();
    let mut output = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if !is_identifier_start(bytes[cursor]) {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        let mut dots = 0;
        while cursor < bytes.len()
            && (is_identifier_body(bytes[cursor]) || matches!(bytes[cursor], b'.' | b':' | b'-'))
        {
            if bytes[cursor] == b'.' {
                dots += 1;
            }
            cursor += 1;
        }
        if dots > 0 {
            output.push((start, &line[start..cursor]));
        }
    }
    output
}

fn extract_bg3_stats(builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
    let source = builder.source();
    let mut current = None;
    for (line_start, line) in physical_lines(source) {
        builder.check_cancelled()?;
        let words = identifiers(line);
        let parsed = Bg3ParsedLine {
            source: SourceLine {
                start: line_start,
                text: line,
            },
            words: &words,
        };
        if let Some(id) = add_bg3_declaration(builder, parsed)? {
            current = Some(id);
            continue;
        }
        let Some(owner) = current.clone() else {
            continue;
        };
        extract_bg3_entry_line(builder, &owner, parsed)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct SourceLine<'a> {
    start: usize,
    text: &'a str,
}

#[derive(Clone, Copy)]
struct Bg3ParsedLine<'a> {
    source: SourceLine<'a>,
    words: &'a [(usize, &'a str)],
}

fn add_bg3_declaration(
    builder: &mut CustomBuilder<'_, '_>,
    input: Bg3ParsedLine<'_>,
) -> Result<Option<SymbolId>, ExtractError> {
    let Bg3ParsedLine {
        source: SourceLine {
            start: line_start,
            text: line,
        },
        words,
    } = input;
    let Some((_, command)) = words.first() else {
        return Ok(None);
    };
    let Some((_, shape)) = words.get(1) else {
        return Ok(None);
    };
    if !command.eq_ignore_ascii_case("new") || !bg3_declaration_shape(shape) {
        return Ok(None);
    }
    let Some((offset, name)) = quoted_values(line).into_iter().next() else {
        return Ok(None);
    };
    if specifier_may_carry_credential(name) {
        return Ok(None);
    }
    builder
        .add_symbol(
            CustomSymbolInput::new(
                SymbolKind::Resource,
                name,
                format!("{}::{name}", builder.path()),
            )
            .at(line_start + offset, line_start + offset + name.len())
            .with_options(SymbolOptions {
                body_search_text: format!("bg3 {shape} {name}"),
                export: SymbolExportFlags::named(true),
                visibility: Some(Visibility::Public),
                ..SymbolOptions::default()
            }),
        )
        .map(Some)
}

fn bg3_declaration_shape(shape: &str) -> bool {
    matches!(
        shape.to_ascii_lowercase().as_str(),
        "entry" | "spellset" | "equipment" | "treasuretable"
    )
}

fn extract_bg3_entry_line(
    builder: &mut CustomBuilder<'_, '_>,
    owner: &SymbolId,
    input: Bg3ParsedLine<'_>,
) -> Result<(), ExtractError> {
    let Bg3ParsedLine { source, words } = input;
    let command = words.first().map(|(_, word)| word.to_ascii_lowercase());
    match command.as_deref() {
        Some("using" | "add") => add_bg3_command_reference(
            builder,
            Bg3Command {
                owner,
                source,
                command: command.as_deref(),
            },
        ),
        Some("data") => add_bg3_data_references(builder, owner, source),
        Some("object") if is_bg3_object_category(words) => add_bg3_command_reference(
            builder,
            Bg3Command {
                owner,
                source,
                command: command.as_deref(),
            },
        ),
        _ => Ok(()),
    }
}

/// Treasure-table `object category "Item",...` rows reference the item.
fn is_bg3_object_category(words: &[(usize, &str)]) -> bool {
    words
        .get(1)
        .is_some_and(|(_, word)| word.eq_ignore_ascii_case("category"))
}

#[derive(Clone, Copy)]
struct Bg3Command<'a> {
    owner: &'a SymbolId,
    source: SourceLine<'a>,
    command: Option<&'a str>,
}

fn add_bg3_command_reference(
    builder: &mut CustomBuilder<'_, '_>,
    input: Bg3Command<'_>,
) -> Result<(), ExtractError> {
    let Bg3Command {
        owner,
        source: SourceLine {
            start: line_start,
            text: line,
        },
        command,
    } = input;
    let Some((offset, value)) = quoted_values(line).into_iter().next() else {
        return Ok(());
    };
    if !is_qualified_name(value) {
        return Ok(());
    }
    let kind = if command == Some("using") {
        ReferenceKind::Extends
    } else {
        ReferenceKind::References
    };
    builder.add_reference(
        CustomReferenceInput::new(Some(owner.clone()), value, kind)
            .at(line_start + offset, line_start + offset + value.len()),
    )
}

fn add_bg3_data_references(
    builder: &mut CustomBuilder<'_, '_>,
    owner: &SymbolId,
    source: SourceLine<'_>,
) -> Result<(), ExtractError> {
    let SourceLine {
        start: line_start,
        text: line,
    } = source;
    let values = quoted_values(line);
    let (Some((_, field)), Some((offset, value))) =
        (values.first().copied(), values.get(1).copied())
    else {
        return Ok(());
    };
    for (relative, token, kind) in bg3::field_references(field, value) {
        let start = line_start + offset + relative;
        builder.add_reference(
            CustomReferenceInput::new(Some(owner.clone()), token, kind)
                .at(start, start + token.len()),
        )?;
    }
    Ok(())
}

fn bg3_reference_tokens(value: &str) -> Vec<&str> {
    if specifier_may_carry_credential(value) {
        return Vec::new();
    }
    value
        .split(|character: char| {
            !(character.is_ascii_alphanumeric()
                || matches!(character, '_' | '-' | '.' | ':' | '/' | '#'))
        })
        .filter(|token| {
            token.len() >= 3
                && is_qualified_name(token)
                && (token.contains(['_', '-', '.', ':', '/'])
                    || token.bytes().any(|byte| byte.is_ascii_digit()))
        })
        .collect()
}

fn extract_osiris(builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
    if builder.source().is_empty() {
        return Ok(());
    }
    let goal_name = basename_stem(builder.path()).to_owned();
    let goal = builder.add_symbol(
        CustomSymbolInput::new(SymbolKind::Module, &goal_name, goal_name.clone())
            .at(0, builder.source().len().min(goal_name.len().max(1)))
            .with_options(SymbolOptions {
                body_search_text: format!("osiris goal {goal_name}"),
                export: SymbolExportFlags::named(true),
                visibility: Some(Visibility::Public),
                ..SymbolOptions::default()
            }),
    )?;
    let source = builder.source();
    let mut state = OsirisScanState {
        goal,
        goal_name,
        section: None,
        current_rule: None,
        pending_rule: None,
        db_nodes: BTreeMap::new(),
    };
    for (index, (line_start, raw_line)) in physical_lines(source).enumerate() {
        builder.check_cancelled()?;
        let line = strip_line_comment(raw_line, "//").trim();
        if line.is_empty() {
            continue;
        }
        scan_osiris_line(
            builder,
            &mut state,
            OsirisLine {
                start: line_start,
                number: index.saturating_add(1),
                raw: raw_line,
                text: line,
            },
        )?;
    }
    Ok(())
}

struct OsirisScanState {
    goal: SymbolId,
    goal_name: String,
    section: Option<SymbolId>,
    current_rule: Option<SymbolId>,
    /// The open block's control keyword and the byte offset it starts at.
    pending_rule: Option<(&'static str, usize)>,
    db_nodes: BTreeMap<String, SymbolId>,
}

#[derive(Clone, Copy)]
struct OsirisLine<'source> {
    start: usize,
    /// One-based physical line number.
    number: usize,
    raw: &'source str,
    text: &'source str,
}

#[derive(Clone, Copy)]
struct OsirisPredicate<'source> {
    name: &'source str,
    raw_offset: usize,
    line_start: usize,
    /// One-based line the predicate is written on.
    line_number: usize,
}

fn scan_osiris_line(
    builder: &mut CustomBuilder<'_, '_>,
    state: &mut OsirisScanState,
    line: OsirisLine<'_>,
) -> Result<(), ExtractError> {
    if scan_osiris_header(builder, state, line)? {
        return Ok(());
    }
    if extract_osiris_declaration(
        builder,
        OsirisDeclarationInput {
            goal: state.goal.clone(),
            line: line.text,
            raw_line: line.raw,
            line_start: line.start,
        },
    )? {
        return Ok(());
    }
    scan_osiris_predicates(builder, state, line)?;
    scan_osiris_string_references(builder, state, line)
}

fn scan_osiris_header(
    builder: &mut CustomBuilder<'_, '_>,
    state: &mut OsirisScanState,
    line: OsirisLine<'_>,
) -> Result<bool, ExtractError> {
    if let Some(name) = osiris_section(line.text) {
        let offset = line.raw.find(name).unwrap_or(0);
        state.section = Some(
            builder.add_symbol(
                CustomSymbolInput::new(
                    SymbolKind::Namespace,
                    name,
                    format!("{}::{name}", state.goal_name),
                )
                .at(line.start + offset, line.start + offset + name.len())
                .with_options(SymbolOptions {
                    body_search_text: format!("osiris section {name}"),
                    parent: Some(state.goal.clone()),
                    ..SymbolOptions::default()
                }),
            )?,
        );
        state.current_rule = None;
        state.pending_rule = None;
        return Ok(true);
    }
    if let Some(control) = ["IF", "PROC", "QRY"].into_iter().find(|control| {
        word_after(line.text, control).is_some() || line.text.eq_ignore_ascii_case(control)
    }) {
        let control_start = line.start + line.raw.find(line.text).unwrap_or(0);
        state.pending_rule = Some((control, control_start));
        state.current_rule = None;
        return Ok(true);
    }
    Ok(false)
}

fn scan_osiris_predicates(
    builder: &mut CustomBuilder<'_, '_>,
    state: &mut OsirisScanState,
    line: OsirisLine<'_>,
) -> Result<(), ExtractError> {
    for (relative, predicate) in function_like_names(line.text) {
        if matches_ignore_ascii_case(predicate, &["IF", "AND", "NOT"]) {
            continue;
        }
        let predicate = OsirisPredicate {
            name: predicate,
            raw_offset: line.raw.find(line.text).unwrap_or(0) + relative,
            line_start: line.start,
            line_number: line.number,
        };
        if begin_pending_osiris_rule(builder, state, predicate)? {
            continue;
        }
        record_osiris_predicate(builder, state, predicate)?;
    }
    Ok(())
}

fn begin_pending_osiris_rule(
    builder: &mut CustomBuilder<'_, '_>,
    state: &mut OsirisScanState,
    predicate: OsirisPredicate<'_>,
) -> Result<bool, ExtractError> {
    if state.current_rule.is_some() {
        return Ok(false);
    }
    let Some((control, declaration_start)) = state.pending_rule.take() else {
        return Ok(false);
    };
    let label = match control {
        "QRY" => "query",
        "PROC" => "proc",
        _ => "rule",
    };
    let owner = state.section.clone().unwrap_or_else(|| state.goal.clone());
    // As in v1, the block starts at its control keyword and is disambiguated
    // by the head predicate's line. v1 recorded no end (it defaulted to the
    // start line); the span here runs through the head predicate's name.
    state.current_rule = Some(
        builder.add_symbol(
            CustomSymbolInput::new(
                SymbolKind::Method,
                &format!("{label}:{}", predicate.name),
                format!("{}::{label}:{}", state.goal_name, predicate.line_number),
            )
            .at(
                declaration_start,
                predicate.line_start + predicate.raw_offset + predicate.name.len(),
            )
            .with_options(SymbolOptions {
                body_search_text: format!("osiris {label} {}", predicate.name),
                parent: Some(owner),
                ..SymbolOptions::default()
            }),
        )?,
    );
    Ok(control != "IF")
}

fn record_osiris_predicate(
    builder: &mut CustomBuilder<'_, '_>,
    state: &mut OsirisScanState,
    predicate: OsirisPredicate<'_>,
) -> Result<(), ExtractError> {
    let owner = state
        .current_rule
        .clone()
        .unwrap_or_else(|| state.goal.clone());
    if predicate.name.starts_with("DB_") {
        ensure_osiris_db_symbol(builder, state, predicate)?;
        builder.add_reference(
            CustomReferenceInput::new(Some(owner), predicate.name, ReferenceKind::References).at(
                predicate.line_start + predicate.raw_offset,
                predicate.line_start + predicate.raw_offset + predicate.name.len(),
            ),
        )?;
    } else {
        builder.add_reference(
            CustomReferenceInput::new(Some(owner), predicate.name, ReferenceKind::Calls).at(
                predicate.line_start + predicate.raw_offset,
                predicate.line_start + predicate.raw_offset + predicate.name.len(),
            ),
        )?;
    }
    Ok(())
}

fn ensure_osiris_db_symbol(
    builder: &mut CustomBuilder<'_, '_>,
    state: &mut OsirisScanState,
    predicate: OsirisPredicate<'_>,
) -> Result<(), ExtractError> {
    if state.db_nodes.contains_key(predicate.name) {
        return Ok(());
    }
    let parent = state.section.clone().unwrap_or_else(|| state.goal.clone());
    let id = builder.add_symbol(
        CustomSymbolInput::new(SymbolKind::Table, predicate.name, predicate.name.to_owned())
            .at(
                predicate.line_start + predicate.raw_offset,
                predicate.line_start + predicate.raw_offset + predicate.name.len(),
            )
            .with_options(SymbolOptions {
                body_search_text: format!("osiris db {}", predicate.name),
                parent: Some(parent),
                ..SymbolOptions::default()
            }),
    )?;
    state.db_nodes.insert(predicate.name.to_owned(), id);
    Ok(())
}

fn scan_osiris_string_references(
    builder: &mut CustomBuilder<'_, '_>,
    state: &OsirisScanState,
    line: OsirisLine<'_>,
) -> Result<(), ExtractError> {
    let owner = state
        .current_rule
        .clone()
        .unwrap_or_else(|| state.goal.clone());
    for (offset, value) in quoted_values(line.text) {
        // As in v1, a string that is one identifier names it whole
        // (`SysCompleteGoal("Init")`); qualified tokens inside longer
        // strings are references too.
        let mut tokens = bg3_tokens::script_string_references(value);
        let mut known = tokens
            .iter()
            .map(|(_, token)| *token)
            .collect::<BTreeSet<_>>();
        for token in bg3_reference_tokens(value) {
            if known.insert(token) {
                tokens.push((value.find(token).unwrap_or(0), token));
            }
        }
        for (relative, token) in tokens {
            let raw_offset = line.raw.find(line.text).unwrap_or(0) + offset + relative;
            builder.add_reference(
                CustomReferenceInput::new(Some(owner.clone()), token, ReferenceKind::References)
                    .at(
                        line.start + raw_offset,
                        line.start + raw_offset + token.len(),
                    ),
            )?;
        }
    }
    Ok(())
}

fn osiris_section(line: &str) -> Option<&str> {
    let normalized = line.trim_end_matches(':').trim();
    [
        "INIT",
        "INITSECTION",
        "KB",
        "KBSECTION",
        "EXIT",
        "EXITSECTION",
    ]
    .into_iter()
    .find(|candidate| normalized.eq_ignore_ascii_case(candidate))
    .map(|section| {
        if section.starts_with("INIT") {
            "INIT"
        } else if section.starts_with("KB") {
            "KB"
        } else {
            "EXIT"
        }
    })
}

fn extract_osiris_declaration(
    builder: &mut CustomBuilder<'_, '_>,
    input: OsirisDeclarationInput<'_>,
) -> Result<bool, ExtractError> {
    if let Some(payload) = brace_payload(input.line, "alias_type") {
        if let Some((relative, name)) = first_identifier(payload) {
            let offset = input.raw_line.find(payload).unwrap_or(0) + relative;
            builder.add_symbol(
                CustomSymbolInput::new(SymbolKind::TypeAlias, name, name.to_owned())
                    .at(
                        input.line_start + offset,
                        input.line_start + offset + name.len(),
                    )
                    .with_options(SymbolOptions {
                        body_search_text: format!("osiris alias {name}"),
                        parent: Some(input.goal),
                        ..SymbolOptions::default()
                    }),
            )?;
        }
        return Ok(true);
    }
    if let Some(payload) = brace_payload(input.line, "enum_type") {
        let mut fields = payload.split(',').map(str::trim);
        if let Some(name) = fields.next().filter(|name| is_qualified_name(name)) {
            let offset = input.raw_line.find(name).unwrap_or(0);
            let enum_id = builder.add_symbol(
                CustomSymbolInput::new(SymbolKind::Enum, name, name.to_owned())
                    .at(
                        input.line_start + offset,
                        input.line_start + offset + name.len(),
                    )
                    .with_options(SymbolOptions {
                        body_search_text: format!("osiris enum {name}"),
                        parent: Some(input.goal),
                        ..SymbolOptions::default()
                    }),
            )?;
            for member in fields.skip(2) {
                let member_name = member.split('=').next().unwrap_or_default().trim();
                if !is_qualified_name(member_name) {
                    continue;
                }
                let member_offset = input.raw_line.find(member_name).unwrap_or(offset);
                builder.add_symbol(
                    CustomSymbolInput::new(
                        SymbolKind::EnumMember,
                        member_name,
                        format!("{name}.{member_name}"),
                    )
                    .at(
                        input.line_start + member_offset,
                        input.line_start + member_offset + member_name.len(),
                    )
                    .with_options(SymbolOptions {
                        body_search_text: format!("enum member {member_name}"),
                        parent: Some(enum_id.clone()),
                        ..SymbolOptions::default()
                    }),
                )?;
            }
        }
        return Ok(true);
    }
    let Some((_, command)) = first_identifier(input.line) else {
        return Ok(false);
    };
    if !matches_ignore_ascii_case(command, &["syscall", "sysquery", "call", "query", "event"]) {
        return Ok(false);
    }
    let Some((offset, name)) = word_after(input.line, command) else {
        return Ok(false);
    };
    let raw_offset = input.raw_line.find(input.line).unwrap_or(0) + offset;
    builder.add_symbol(
        CustomSymbolInput::new(SymbolKind::Function, name, name.to_owned())
            .at(
                input.line_start + raw_offset,
                input.line_start + raw_offset + name.len(),
            )
            .with_options(SymbolOptions {
                body_search_text: format!("osiris api {name}"),
                export: SymbolExportFlags::named(true),
                visibility: Some(Visibility::Public),
                parent: Some(input.goal),
                ..SymbolOptions::default()
            }),
    )?;
    Ok(true)
}

fn brace_payload<'source>(line: &'source str, command: &str) -> Option<&'source str> {
    let (_, found) = first_identifier(line)?;
    if !found.eq_ignore_ascii_case(command) {
        return None;
    }
    let open = line.find('{')?;
    let close = line.rfind('}')?;
    (close > open).then_some(&line[open + 1..close])
}

fn strip_line_comment<'source>(line: &'source str, marker: &str) -> &'source str {
    line.find(marker).map_or(line, |index| &line[..index])
}

fn extract_bg3_resource(builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
    // Binary (`.lsf`/`.lsb`) payloads are not text resources: stop instead of
    // emitting byte noise, so the file is recorded as degraded.
    if builder.source().contains('\0') {
        return Err(ExtractError::ParserStopped);
    }
    let trimmed = builder.source().trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(value) = serde_json::from_str::<Value>(builder.source())
    {
        return extract_bg3_json(builder, &value, None);
    }
    bg3::extract_markup(builder)
}

fn extract_bg3_json(
    builder: &mut CustomBuilder<'_, '_>,
    value: &Value,
    parent: Option<SymbolId>,
) -> Result<(), ExtractError> {
    builder.check_cancelled()?;
    match value {
        Value::Array(items) => {
            for item in items {
                extract_bg3_json(builder, item, parent.clone())?;
            }
        }
        Value::Object(fields) => extract_bg3_object(builder, fields, parent)?,
        Value::String(raw) => add_bg3_references(builder, raw, parent.as_ref())?,
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn extract_bg3_object(
    builder: &mut CustomBuilder<'_, '_>,
    fields: &serde_json::Map<String, Value>,
    parent: Option<SymbolId>,
) -> Result<(), ExtractError> {
    let symbol_count = builder.symbols.len();
    let next_parent = bg3_object_parent(builder, fields, parent)?;
    let declares_resource = builder.symbols.len() > symbol_count;
    let name_key = bg3_json_field_key(fields, &BG3_JSON_NAME_KEYS);
    let identity_key = bg3_json_field_key(fields, &BG3_JSON_IDENTITY_KEYS).filter(|_| {
        bg3_json_field(fields, &BG3_JSON_IDENTITY_KEYS)
            .and_then(bg3_tokens::global_identity)
            .is_some()
    });
    for (key, child) in fields {
        if declares_resource
            && (name_key == Some(key.as_str()) || identity_key == Some(key.as_str()))
        {
            continue;
        }
        if let Some(raw) = child.as_str()
            && !is_bg3_name_key(key)
        {
            add_bg3_references(builder, raw, next_parent.as_ref())?;
            continue;
        }
        extract_bg3_json(builder, child, next_parent.clone())?;
    }
    Ok(())
}

fn bg3_object_parent(
    builder: &mut CustomBuilder<'_, '_>,
    fields: &serde_json::Map<String, Value>,
    parent: Option<SymbolId>,
) -> Result<Option<SymbolId>, ExtractError> {
    let Some(name) =
        bg3_json_field(fields, &BG3_JSON_NAME_KEYS).filter(|name| !looks_sensitive(name))
    else {
        return Ok(parent);
    };
    let offset = builder.source().find(name).unwrap_or(0);
    if offset >= builder.source().len() {
        return Ok(parent);
    }
    // A UUID/Guid/id is the game-global identity other files reference.
    let uuid =
        bg3_json_field(fields, &BG3_JSON_IDENTITY_KEYS).and_then(bg3_tokens::global_identity);
    let addressable = parent.is_none() || uuid.is_some();
    let qualified_name = uuid.map_or_else(|| format!("{}::{name}", builder.path()), str::to_owned);
    builder
        .add_symbol(
            CustomSymbolInput::new(SymbolKind::Resource, name, qualified_name)
                .at(offset, (offset + name.len()).min(builder.source().len()))
                .with_options(SymbolOptions {
                    body_search_text: format!("bg3 resource {name}"),
                    export: SymbolExportFlags::named(addressable),
                    visibility: addressable.then_some(Visibility::Public),
                    parent,
                    ..SymbolOptions::default()
                }),
        )
        .map(Some)
}

/// LSJ name fields in v1 precedence order.
const BG3_JSON_NAME_KEYS: [&str; 6] = ["Name", "NameFS", "name", "UUID", "Guid", "id"];
/// LSJ fields that carry a game-global identity.
const BG3_JSON_IDENTITY_KEYS: [&str; 3] = ["UUID", "Guid", "id"];

/// The first non-empty (trimmed) string among `keys`, in order.
fn bg3_json_field<'value>(
    fields: &'value serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<&'value str> {
    keys.iter()
        .filter_map(|key| fields.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|value| !value.is_empty())
}

/// The scalar field selected by the declaration's first non-empty rule.
fn bg3_json_field_key<'key>(
    fields: &serde_json::Map<String, Value>,
    keys: &[&'key str],
) -> Option<&'key str> {
    keys.iter().copied().find(|key| {
        fields
            .get(*key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

fn add_bg3_references(
    builder: &mut CustomBuilder<'_, '_>,
    raw: &str,
    parent: Option<&SymbolId>,
) -> Result<(), ExtractError> {
    let base = builder.source().find(raw).unwrap_or(0);
    for (relative, token) in bg3_tokens::reference_tokens(raw) {
        if base + relative + token.len() <= builder.source().len() {
            builder.add_reference(
                CustomReferenceInput::new(parent.cloned(), token, ReferenceKind::References)
                    .at(base + relative, base + relative + token.len()),
            )?;
        }
    }
    Ok(())
}

fn is_bg3_name_key(key: &str) -> bool {
    matches!(key, "NameFS" | "Name" | "name" | "UUID" | "Guid" | "id")
}

type Bg3Region = (Option<SymbolId>, String);

fn scan_bg3_region(
    builder: &mut CustomBuilder<'_, '_>,
    regions: &mut Vec<Bg3Region>,
    tag: MarkupTag<'_>,
) -> Result<bool, ExtractError> {
    if !tag_name_eq(tag, "region") {
        return Ok(false);
    }
    if tag.closing {
        if regions.len() > 1 {
            regions.pop();
        }
        return Ok(true);
    }
    let unsafe_scope = regions.last().is_some_and(|(_, name)| name.is_empty())
        || tag_attribute(tag, "id").is_some_and(|(_, name)| specifier_may_carry_credential(name));
    if unsafe_scope {
        if !tag.self_closing {
            regions.push((None, String::new()));
        }
        return Ok(true);
    }
    if let Some((offset, name)) = tag_attribute(tag, "id") {
        let parent = regions.last().and_then(|(id, _)| id.clone());
        let prefix = regions.last().map_or(builder.path(), |(_, name)| name);
        let qualified = format!("{prefix}::{name}");
        let id = builder.add_symbol(
            CustomSymbolInput::new(SymbolKind::Namespace, name, qualified.clone())
                .at(offset, offset + name.len())
                .with_options(SymbolOptions {
                    body_search_text: format!("bg3 region {name}"),
                    parent,
                    ..SymbolOptions::default()
                }),
        )?;
        if !tag.self_closing {
            regions.push((Some(id), qualified));
        }
    }
    Ok(true)
}

fn scan_bg3_content(
    builder: &mut CustomBuilder<'_, '_>,
    regions: &[Bg3Region],
    tag: MarkupTag<'_>,
) -> Result<bool, ExtractError> {
    if !tag_name_eq(tag, "content") || tag.closing {
        return Ok(false);
    }
    let Some((offset, handle)) = tag_attribute(tag, "contentuid") else {
        return Ok(false);
    };
    if specifier_may_carry_credential(handle) {
        return Ok(true);
    }
    builder.add_symbol(
        CustomSymbolInput::new(SymbolKind::Resource, handle, handle.to_owned())
            .at(offset, offset + handle.len())
            .with_options(SymbolOptions {
                body_search_text: format!("localized content {handle}"),
                export: SymbolExportFlags::named(true),
                visibility: Some(Visibility::Public),
                parent: regions.last().and_then(|(id, _)| id.clone()),
                ..SymbolOptions::default()
            }),
    )?;
    Ok(true)
}
