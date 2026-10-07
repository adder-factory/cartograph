//! Exact Java/Kotlin package and import lookup over extracted facts.
//!
//! Explicit imports (including Kotlin aliases) precede same-package names,
//! which precede wildcard packages. An ambiguous or missing explicit target
//! never widens to a namesake. No path suffix or capitalized-variable heuristic
//! participates: package declarations and nominal type references are required.

mod wildcard;

use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};

use cartograph_domain::{FileId, ReferenceKind, SourceSpan, SymbolId, SymbolKind, Visibility};
use cartograph_extract::{DeclarationSyntax, ImportBindingKind};

use super::{
    EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE, FileResolutionContext, ImportBindingScratch,
    ImportBindingSelection, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceLookup,
    ReferenceResolution, ResolutionCandidate, ResolutionCandidateInsertion, ResolutionIndex,
    ResolutionIndexTarget, ResolutionRequest, ResolveBudget, ResolvedTarget, StageItemFailure,
    UNRESOLVED_IMPORT_PROVENANCE, UNRESOLVED_PROVENANCE, jvm_nested_resolution,
    nominal_scope_resolution, try_clone_text, usize_to_u64,
};

const MAX_KEY_BYTES: usize = 1_536;
const MAX_OWNER_HOPS: usize = 64;
const IMPORT_PROVENANCE: &str = "native-jvm-explicit-import";
const PACKAGE_PROVENANCE: &str = "native-jvm-package";
const WILDCARD_PROVENANCE: &str = "native-jvm-wildcard-import";
pub(super) const QUALIFIED_PROVENANCE: &str = "native-jvm-qualified-type";

#[derive(Default)]
pub(super) struct JvmResolutionIndex {
    files: HashMap<FileId, ImportHints>,
    declared_types: HashMap<SymbolId, Option<String>>,
    constructor_abstentions: HashSet<SymbolId>,
    packages: HashMap<String, HashMap<String, Option<String>>>,
}

#[derive(Default)]
struct ImportHints {
    package: Option<String>,
    package_owner: Option<SymbolId>,
    explicit: HashMap<String, Option<String>>,
    wildcards: HashSet<String>,
    wildcard_types: HashMap<String, Option<String>>,
    abstentions: HashSet<(u64, u64)>,
    local_type_blocks: HashMap<(u64, u64), (u64, u64)>,
}

pub(super) fn language(language: &str) -> bool {
    matches!(language, "java" | "kotlin")
}

fn indexed_type_request(language_name: &str, kind: ReferenceKind) -> bool {
    language(language_name)
        && matches!(
            kind,
            ReferenceKind::TypeOf
                | ReferenceKind::Returns
                | ReferenceKind::Instantiates
                | ReferenceKind::Inherits
                | ReferenceKind::Extends
                | ReferenceKind::Implements
                | ReferenceKind::Decorates
                | ReferenceKind::Imports
        )
}

pub(super) fn select_import_bindings<'context>(
    input: (
        &'context FileResolutionContext<'_>,
        &ReferenceLookup<'_>,
        ReferenceKind,
        &str,
    ),
    scratch: &'context mut ImportBindingScratch,
) -> ImportBindingSelection<'context> {
    let (context, lookup, kind, binding_name) = input;
    if indexed_type_request(&context.identity.language, lookup.request_kind(kind)) {
        return ImportBindingSelection::empty();
    }
    scratch.select(context.import_bindings, binding_name)
}

pub(super) fn syntax_abstention(name: &str) -> bool {
    name.starts_with("native-jvm-type-parameter::")
}

pub(super) fn reference_abstains(index: &ResolutionIndex, input: (&FileId, SourceSpan)) -> bool {
    let (file, span) = input;
    index.languages.jvm.files.get(file).is_some_and(|hints| {
        hints
            .abstentions
            .contains(&(span.start_byte(), span.end_byte()))
    })
}

pub(super) fn nominal_type(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Interface
            | SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Trait
            | SymbolKind::TypeAlias
    )
}

pub(super) fn value_binding_candidate(
    request: &ResolutionRequest<'_>,
    candidate: &ResolutionCandidate,
) -> bool {
    !(request.language == "kotlin"
        && request.kind == ReferenceKind::FieldAccess
        && candidate.declaration_syntax == DeclarationSyntax::KotlinConstructor)
}

/// Index JVM declarations under their dotted FQN as well as their native name.
pub(super) fn candidate_alias(
    insertion: ResolutionCandidateInsertion<'_>,
) -> Result<Option<String>, StageItemFailure> {
    if !language(insertion.language) {
        return Ok(None);
    }
    let qualified = &insertion.symbol.input.qualified_name;
    if !qualified.contains("::") || qualified.len() > MAX_KEY_BYTES {
        return Ok(None);
    }
    let mut alias = String::new();
    alias
        .try_reserve_exact(qualified.len())
        .map_err(|_| StageItemFailure)?;
    for segment in qualified.split("::") {
        if !alias.is_empty() {
            alias.push('.');
        }
        alias.push_str(segment);
    }
    Ok(Some(alias))
}

pub(super) fn index_file<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !language(&file.file.language) && file.resolution_abstentions.is_empty() {
        return Ok(());
    }
    let mut hints = ImportHints::default();
    index_abstentions(&mut hints, (file, target.budget), cancelled)?;
    index_local_blocks(&mut hints, (file, target.budget), cancelled)?;
    index_declarations(target, (file, &mut hints), cancelled)?;
    index_imports(&mut hints, (file, target.budget), cancelled)?;
    wildcard::index_package(target, (file, &hints), cancelled)?;
    for reference in &file.references {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if reference.kind == ReferenceKind::TypeOf {
            record_declared_type(target, reference)?;
        }
    }
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(FileId, ImportHints)>()))
            .saturating_add(usize_to_u64(file.file.file_id.as_str().len())),
    )?;
    target
        .index
        .languages
        .jvm
        .files
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    target
        .index
        .languages
        .jvm
        .files
        .insert(file.file.file_id.clone(), hints);
    Ok(())
}

fn index_declarations<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    input: (&NativeFileFacts, &mut ImportHints),
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file, hints) = input;
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if file.file.language == "kotlin"
            && hints
                .abstentions
                .contains(&(symbol.input.start_byte, symbol.input.end_byte))
        {
            block_construction(
                &mut target.index.languages.jvm,
                &symbol.input.symbol_id,
                target.budget,
            )?;
        }
        if symbol.kind == SymbolKind::Namespace && symbol.input.qualified_name == symbol.name {
            target.budget.charge(
                usize_to_u64(symbol.name.len())
                    .saturating_add(usize_to_u64(symbol.input.symbol_id.as_str().len())),
            )?;
            hints.package = Some(try_clone_text(&symbol.name)?);
            hints.package_owner = Some(symbol.input.symbol_id.clone());
        }
    }
    Ok(())
}

fn record_declared_type(
    target: &mut ResolutionIndexTarget<'_>,
    reference: &cartograph_extract::ExtractedReference,
) -> Result<(), StageItemFailure> {
    let Some(owner) = &reference.owner else {
        return Ok(());
    };
    record_unique(
        &mut target.index.languages.jvm.declared_types,
        UniqueName {
            key: owner.clone(),
            key_bytes: usize_to_u64(owner.as_str().len()),
            value: Some(try_clone_text(
                reference
                    .resolution_name
                    .as_deref()
                    .unwrap_or(&reference.name),
            )?),
        },
        target.budget,
    )
}

fn block_construction(
    jvm: &mut JvmResolutionIndex,
    id: &SymbolId,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if jvm.constructor_abstentions.contains(id) {
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<SymbolId>()))
            .saturating_add(usize_to_u64(id.as_str().len())),
    )?;
    jvm.constructor_abstentions
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    jvm.constructor_abstentions.insert(id.clone());
    Ok(())
}

pub(super) fn constructor_target_abstains(
    index: &ResolutionIndex,
    input: (&ResolutionRequest<'_>, &SymbolId, SymbolKind),
) -> bool {
    let (request, id, kind) = input;
    request.language == "kotlin"
        && request.kind == ReferenceKind::Instantiates
        && (kind == SymbolKind::TypeAlias
            || index.languages.jvm.constructor_abstentions.contains(id))
}

fn index_abstentions<Cancel>(
    hints: &mut ImportHints,
    input: (&NativeFileFacts, &mut ResolveBudget),
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file, budget) = input;
    for span in &file.resolution_abstentions {
        if cancelled() {
            return Err(StageItemFailure);
        }
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE.saturating_add(usize_to_u64(size_of::<(u64, u64)>())),
        )?;
        hints
            .abstentions
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        hints
            .abstentions
            .insert((span.start_byte(), span.end_byte()));
    }
    Ok(())
}

fn index_local_blocks<Cancel>(
    hints: &mut ImportHints,
    input: (&NativeFileFacts, &mut ResolveBudget),
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file, budget) = input;
    for (declaration, block) in &file.local_type_scopes {
        if cancelled() {
            return Err(StageItemFailure);
        }
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<((u64, u64), (u64, u64))>())),
        )?;
        hints
            .local_type_blocks
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        hints.local_type_blocks.insert(
            (declaration.start_byte(), declaration.end_byte()),
            (block.start_byte(), block.end_byte()),
        );
    }
    Ok(())
}

pub(super) fn lexical_type_visible(
    index: &ResolutionIndex,
    input: (&ResolutionRequest<'_>, &ResolutionCandidate),
) -> bool {
    let (request, candidate) = input;
    if request.language != "java" || !nominal_type(candidate.kind) {
        return true;
    }
    let Some(hints) = index.languages.jvm.files.get(request.file_id) else {
        return false;
    };
    if let Some(block) = hints.local_type_blocks.get(&candidate.declaration_span) {
        return candidate.declaration_span.0 < request.span.start_byte()
            && block.0 <= request.span.start_byte()
            && request.span.end_byte() <= block.1;
    }
    candidate.parent_symbol_id.as_ref().is_none_or(|parent| {
        index.types.kind(parent).is_some() || hints.package_owner.as_ref() == Some(parent)
    })
}

fn index_imports<Cancel>(
    hints: &mut ImportHints,
    input: (&NativeFileFacts, &mut ResolveBudget),
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file, budget) = input;
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.kind == ImportBindingKind::Named {
            record_unique(
                &mut hints.explicit,
                UniqueName {
                    key: try_clone_text(&binding.local_name)?,
                    key_bytes: usize_to_u64(binding.local_name.len()),
                    value: joined_name(&binding.module_specifier, &binding.imported_name),
                },
                budget,
            )?;
        } else if binding.kind == ImportBindingKind::Namespace
            && binding.local_name == "*"
            && !hints.wildcards.contains(&binding.module_specifier)
        {
            budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE
                    .saturating_add(usize_to_u64(size_of::<String>()))
                    .saturating_add(usize_to_u64(binding.module_specifier.len())),
            )?;
            hints
                .wildcards
                .try_reserve(1)
                .map_err(|_| StageItemFailure)?;
            hints
                .wildcards
                .insert(try_clone_text(&binding.module_specifier)?);
        }
    }
    Ok(())
}

struct UniqueName<Key> {
    key: Key,
    key_bytes: u64,
    value: Option<String>,
}

fn record_unique<Key: Eq + Hash>(
    values: &mut HashMap<Key, Option<String>>,
    entry: UniqueName<Key>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if let Some(existing) = values.get_mut(&entry.key) {
        if *existing != entry.value {
            *existing = None;
        }
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(Key, Option<String>)>()))
            .saturating_add(entry.key_bytes)
            .saturating_add(
                entry
                    .value
                    .as_ref()
                    .map_or(0, |value| usize_to_u64(value.len())),
            ),
    )?;
    values.try_reserve(1).map_err(|_| StageItemFailure)?;
    values.insert(entry.key, entry.value);
    Ok(())
}

pub(super) fn declared_type<'index>(
    index: &'index ResolutionIndex,
    id: &SymbolId,
) -> Option<&'index str> {
    index.languages.jvm.declared_types.get(id)?.as_deref()
}

pub(super) fn same_package(index: &ResolutionIndex, files: [&FileId; 2]) -> bool {
    let [source, target] = files.map(|id| index.languages.jvm.files.get(id));
    matches!((source, target), (Some(source), Some(target)) if source.package == target.package)
}

pub(super) fn resolve_reference<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !indexed_type_request(request.language, request.kind) {
        return Ok(None);
    }
    resolve_type(index, request, cancelled).map(Some)
}

pub(super) fn resolve_type<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if syntax_abstention(request.name) {
        return Ok(ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE));
    }
    if kotlin_value_shadows_type(index, request, cancelled)? {
        return Ok(ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE));
    }
    let type_request = ResolutionRequest {
        kind: ReferenceKind::TypeOf,
        ..*request
    };
    if let Some(resolution) = nominal_scope_resolution::resolve(index, &type_request, cancelled)? {
        if resolution.target.as_ref().is_some_and(|target| {
            constructor_target_abstains(index, (request, &target.symbol_id, target.kind))
        }) {
            return Ok(ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE));
        }
        return Ok(resolution);
    }
    if let Some(resolution) = jvm_nested_resolution::resolve(index, request, cancelled)? {
        return Ok(resolution);
    }
    resolve_imported_type(index, request, cancelled)
}

fn kotlin_value_shadows_type<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language != "kotlin" || request.kind != ReferenceKind::Instantiates {
        return Ok(false);
    }
    let head_request = ResolutionRequest {
        name: request.name.split('.').next().unwrap_or(request.name),
        kind: ReferenceKind::FieldAccess,
        ..*request
    };
    Ok(
        nominal_scope_resolution::resolve(index, &head_request, cancelled)?.is_some_and(
            |resolution| {
                resolution
                    .target
                    .is_none_or(|target| !nominal_type(target.kind))
            },
        ),
    )
}

pub(super) fn callable_competitor(request: &ResolutionRequest<'_>, kind: SymbolKind) -> bool {
    (request.kind == ReferenceKind::Imports
        || (request.language == "kotlin" && request.kind == ReferenceKind::Instantiates))
        && matches!(
            kind,
            SymbolKind::Function
                | SymbolKind::Method
                | SymbolKind::Variable
                | SymbolKind::Constant
                | SymbolKind::Property
                | SymbolKind::Field
        )
}

fn resolve_imported_type<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let hints = index
        .languages
        .jvm
        .files
        .get(request.file_id)
        .ok_or(StageItemFailure)?;
    let query = TypeQuery {
        index,
        request,
        key: request.name,
    };
    if let Some(explicit) = hints.explicit.get(request.name) {
        return match explicit.as_deref() {
            Some(key) => bind_type(TypeQuery { key, ..query }, IMPORT_PROVENANCE, cancelled),
            None => Ok(ReferenceResolution::unresolved(
                UNRESOLVED_IMPORT_PROVENANCE,
            )),
        };
    }
    if let Some((head, suffix)) = request.name.split_once('.') {
        if let Some(explicit) = hints.explicit.get(head) {
            let key = explicit
                .as_deref()
                .and_then(|head| joined_name(head, suffix));
            return match key.as_deref() {
                Some(key) => bind_type(TypeQuery { key, ..query }, IMPORT_PROVENANCE, cancelled),
                None => Ok(ReferenceResolution::unresolved(
                    UNRESOLVED_IMPORT_PROVENANCE,
                )),
            };
        }
        let choice = type_choice(query, cancelled)?;
        if !matches!(choice, TypeChoice::Absent) {
            return choice.resolution((query, QUALIFIED_PROVENANCE), cancelled);
        }
    }
    let key = joined_name(hints.package.as_deref().unwrap_or_default(), request.name);
    if let Some(key) = key.as_deref() {
        let choice = type_choice(TypeQuery { key, ..query }, cancelled)?;
        if !matches!(choice, TypeChoice::Absent) {
            return choice.resolution((query, PACKAGE_PROVENANCE), cancelled);
        }
    }
    wildcard::resolve(query, hints, cancelled)
}

#[derive(Clone, Copy)]
struct TypeQuery<'index, 'request, 'key> {
    index: &'index ResolutionIndex,
    request: &'index ResolutionRequest<'request>,
    key: &'key str,
}

enum TypeChoice<'index> {
    Absent,
    Ambiguous,
    Unique(&'index ResolutionCandidate),
}

impl TypeChoice<'_> {
    fn resolution<Cancel>(
        self,
        input: (TypeQuery<'_, '_, '_>, &'static str),
        cancelled: &mut Cancel,
    ) -> Result<ReferenceResolution, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let (query, provenance) = input;
        Ok(match self {
            Self::Absent => {
                ReferenceResolution::unresolved(EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE)
            }
            Self::Unique(candidate) if type_visible(query, candidate, cancelled)? => {
                ReferenceResolution::resolved(ResolvedTarget {
                    symbol_id: candidate.symbol_id.clone(),
                    kind: candidate.kind,
                    confidence: 0.95,
                    provenance,
                })
            }
            Self::Unique(_) | Self::Ambiguous => {
                ReferenceResolution::unresolved(UNRESOLVED_IMPORT_PROVENANCE)
            }
        })
    }
}

fn type_visible<Cancel>(
    query: TypeQuery<'_, '_, '_>,
    candidate: &ResolutionCandidate,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !visibility_accessible(query, (candidate.visibility, &candidate.file_id)) {
        return Ok(false);
    }
    let mut parent = candidate.parent_symbol_id.as_ref();
    for _ in 0..MAX_OWNER_HOPS {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(id) = parent else {
            return Ok(true);
        };
        let Some(visibility) = query.index.types.visibility(id) else {
            return Ok(query
                .index
                .languages
                .jvm
                .files
                .get(&candidate.file_id)
                .is_some_and(|hints| hints.package_owner.as_ref() == Some(id)));
        };
        if !visibility_accessible(query, visibility) {
            return Ok(false);
        }
        parent = query.index.parents.get(id);
    }
    Ok(false)
}

fn visibility_accessible(
    query: TypeQuery<'_, '_, '_>,
    visibility: (Option<Visibility>, &FileId),
) -> bool {
    let (visibility, file) = visibility;
    visibility == Some(Visibility::Public)
        || (visibility != Some(Visibility::Private)
            && same_package(query.index, [query.request.file_id, file]))
}

fn type_choice<'index, Cancel>(
    query: TypeQuery<'index, '_, '_>,
    cancelled: &mut Cancel,
) -> Result<TypeChoice<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(bucket) = query.index.candidates.get(query.key) else {
        return Ok(TypeChoice::Absent);
    };
    let mut selected = None;
    for candidate in bucket.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !fqn_matches(&candidate.qualified_name, query.key)
            || !type_language_compatible(query, candidate)
        {
            continue;
        }
        if callable_competitor(query.request, candidate.kind)
            || constructor_target_abstains(
                query.index,
                (query.request, &candidate.symbol_id, candidate.kind),
            )
        {
            return Ok(TypeChoice::Ambiguous);
        }
        if !nominal_type(candidate.kind) {
            continue;
        }
        if selected.is_some() {
            return Ok(TypeChoice::Ambiguous);
        }
        selected = Some(candidate);
    }
    Ok(selected.map_or(TypeChoice::Absent, TypeChoice::Unique))
}

fn type_language_compatible(query: TypeQuery<'_, '_, '_>, candidate: &ResolutionCandidate) -> bool {
    let Some(file) = query.index.modules.files.get(&candidate.file_id) else {
        return false;
    };
    language(&file.language)
        && (query.request.language != "java"
            || file.language != "kotlin"
            || candidate.kind != SymbolKind::TypeAlias)
}

fn fqn_matches(qualified: &str, key: &str) -> bool {
    let mut segments = qualified.split("::");
    let Some(first) = segments.next() else {
        return false;
    };
    let Some(mut remaining) = key.strip_prefix(first) else {
        return false;
    };
    for segment in segments {
        let Some(suffix) = remaining
            .strip_prefix('.')
            .and_then(|suffix| suffix.strip_prefix(segment))
        else {
            return false;
        };
        remaining = suffix;
    }
    remaining.is_empty()
}

fn bind_type<Cancel>(
    query: TypeQuery<'_, '_, '_>,
    provenance: &'static str,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    type_choice(query, cancelled)?.resolution((query, provenance), cancelled)
}

pub(super) use wildcard::prepare as prepare_wildcards;

/// Temporary lookup strings are capped independently of project size.
fn joined_name(package: &str, name: &str) -> Option<String> {
    let bytes = package.len().checked_add(name.len())?.checked_add(1)?;
    if bytes > MAX_KEY_BYTES {
        return None;
    }
    let mut key = String::new();
    key.try_reserve_exact(bytes).ok()?;
    key.push_str(package);
    if !package.is_empty() {
        key.push('.');
    }
    key.push_str(name);
    Some(key)
}
