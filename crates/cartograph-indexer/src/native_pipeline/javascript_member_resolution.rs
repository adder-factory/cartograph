//! JS member fallbacks keep receiver-backed calls separate from name guesses.
//! Qualified companion facts retain receiver scope evidence. Builtin names
//! without receiver backing abstain; implicit-public methods use the existing
//! project dispatch eligibility and dynamic provenance.

use std::{collections::HashMap, mem::size_of};

use super::{
    DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE, ExtractedReference, FileId, ImportBindingKind,
    ImportBindingMatch, ImportReferenceSite, ImportResolution, ImportResolutionRequest,
    ImportScope, JAVASCRIPT_INTRINSIC_UNRESOLVED_PROVENANCE, JavascriptMemberCallContext,
    JavascriptMemberReceiver, LEXICAL_SCOPE_RESOLUTION_PREFIX, LexicalScopeQuery,
    MAX_SYMBOL_QUALIFIED_NAME_BYTES, NativeSymbolFacts, ProjectCandidateInput,
    ProjectResolutionCandidates, ProjectResolutionQuery, RESOLUTION_MAP_NODE_ALLOWANCE,
    ReferenceDispatch, ReferenceKind, ReferenceResolution, ResolutionCandidate, ResolutionIndex,
    ResolutionIndexFileInput, ResolutionRequest, ResolveBudget, ResolvedTarget, StageItemFailure,
    SymbolId, SymbolKind, UNRESOLVED_IMPORT_PROVENANCE, Visibility, framework_resolution_alias,
    import_binding_is_project_local, is_project_candidate, javascript_family_name,
    javascript_intrinsic_reference, matched_import_binding, native_bridge_target_language,
    project_resolved_target, project_source_context, reference_import_scope,
    reference_kind_candidate, resolution_candidates_for_file, resolve_import, resolve_lexical,
    resolve_lexical_scope, select_candidate, try_clone_text, usize_to_u64,
};

#[derive(Default)]
pub(super) struct JavascriptMemberIndex {
    classes: HashMap<SymbolId, ClassInfo>,
    static_members: HashMap<SymbolId, HashMap<String, Option<SymbolId>>>,
    calls: HashMap<FileId, HashMap<u64, Option<Companion>>>,
}

struct ClassInfo {
    file_id: FileId,
    qualified_name: String,
}

#[derive(PartialEq, Eq)]
enum Companion {
    Receiver {
        name: String,
        binding: ReceiverBinding,
    },
    Constructor(String),
    Value(NamedValue),
}

#[derive(PartialEq, Eq)]
enum ReceiverBinding {
    Module,
    Shadowed,
    LocalImport,
}

#[derive(PartialEq, Eq)]
struct NamedValue {
    name: String,
    owner: Option<SymbolId>,
    start_byte: u64,
    lexical_scope: bool,
}

impl Companion {
    fn name(&self) -> &str {
        match self {
            Self::Receiver { name, .. } | Self::Constructor(name) => name,
            Self::Value(value) => &value.name,
        }
    }
}

pub(super) fn index_file<Cancel>(
    input: &mut ResolutionIndexFileInput<'_, '_, '_, '_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !javascript_family_name(&input.file.file.language) {
        return Ok(());
    }
    index_classes(input)?;
    index_calls(input)
}

fn index_classes<Cancel>(
    input: &mut ResolutionIndexFileInput<'_, '_, '_, '_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let members = &mut input.index.javascript_members;
    for symbol in &input.file.symbols {
        if (input.cancelled)() {
            return Err(StageItemFailure);
        }
        if symbol.kind != SymbolKind::Class {
            continue;
        }
        charge_entry::<(SymbolId, ClassInfo)>(
            input.budget,
            symbol
                .input
                .symbol_id
                .as_str()
                .len()
                .saturating_add(symbol.input.file_id.as_str().len())
                .saturating_add(symbol.input.qualified_name.len()),
        )?;
        members
            .classes
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        members.classes.insert(
            symbol.input.symbol_id.clone(),
            ClassInfo {
                file_id: symbol.input.file_id.clone(),
                qualified_name: try_clone_text(&symbol.input.qualified_name)?,
            },
        );
    }
    Ok(())
}

pub(super) fn candidate_visibility(
    index: &ResolutionIndex,
    symbol: &NativeSymbolFacts,
) -> Option<Visibility> {
    if symbol.visibility.is_none()
        && symbol.kind == SymbolKind::Method
        && !symbol.name.starts_with('#')
        && index
            .parents
            .get(&symbol.input.symbol_id)
            .is_some_and(|parent| index.javascript_members.classes.contains_key(parent))
    {
        Some(Visibility::Public)
    } else {
        symbol.visibility
    }
}

pub(super) fn index_static_members<Cancel>(
    input: &mut ResolutionIndexFileInput<'_, '_, '_, '_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !javascript_family_name(&input.file.file.language) {
        return Ok(());
    }
    for symbol in &input.file.symbols {
        if (input.cancelled)() {
            return Err(StageItemFailure);
        }
        if symbol.kind != SymbolKind::Method
            || !symbol.execution.static_member
            || matches!(
                symbol.visibility,
                Some(Visibility::Private | Visibility::Protected)
            )
            || symbol.name.starts_with('#')
        {
            continue;
        }
        let Some(parent) = input.index.parents.get(&symbol.input.symbol_id) else {
            continue;
        };
        if input.index.javascript_members.classes.contains_key(parent) {
            insert_static_member(
                &mut input.index.javascript_members.static_members,
                (parent, symbol),
                input.budget,
            )?;
        }
    }
    Ok(())
}

fn insert_static_member(
    members: &mut HashMap<SymbolId, HashMap<String, Option<SymbolId>>>,
    (parent, symbol): (&SymbolId, &NativeSymbolFacts),
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if !members.contains_key(parent) {
        charge_entry::<(SymbolId, HashMap<String, Option<SymbolId>>)>(
            budget,
            parent.as_str().len(),
        )?;
        members.try_reserve(1).map_err(|_| StageItemFailure)?;
        members.insert(parent.clone(), HashMap::new());
    }
    let names = members.get_mut(parent).ok_or(StageItemFailure)?;
    if let Some(existing) = names.get_mut(&symbol.name) {
        if existing.as_ref() != Some(&symbol.input.symbol_id) {
            *existing = None;
        }
        return Ok(());
    }
    charge_entry::<(String, Option<SymbolId>)>(
        budget,
        symbol
            .name
            .len()
            .saturating_add(symbol.input.symbol_id.as_str().len()),
    )?;
    names.try_reserve(1).map_err(|_| StageItemFailure)?;
    names.insert(
        try_clone_text(&symbol.name)?,
        Some(symbol.input.symbol_id.clone()),
    );
    Ok(())
}

pub(super) fn shadowed_receiver(index: &ResolutionIndex, request: &ResolutionRequest<'_>) -> bool {
    matches!(
        companion(index, request),
        Some(Companion::Receiver { binding, .. }) if *binding != ReceiverBinding::Module
    )
}

fn index_calls<Cancel>(
    input: &mut ResolutionIndexFileInput<'_, '_, '_, '_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let members = &mut input.index.javascript_members;
    let mut calls = HashMap::new();
    for reference in &input.file.references {
        if (input.cancelled)() {
            return Err(StageItemFailure);
        }
        if receiver_companion(reference) || value_companion(reference) {
            insert_call(&mut calls, reference, input.budget)?;
        }
    }
    for context in &input.file.javascript_member_calls {
        if (input.cancelled)() {
            return Err(StageItemFailure);
        }
        apply_call_context(&mut calls, context, input.budget)?;
    }
    if !calls.is_empty() {
        charge_entry::<(FileId, HashMap<u64, Option<Companion>>)>(
            input.budget,
            input.file.file.file_id.as_str().len(),
        )?;
        members.calls.try_reserve(1).map_err(|_| StageItemFailure)?;
        members.calls.insert(input.file.file.file_id.clone(), calls);
    }
    Ok(())
}

fn receiver_companion(reference: &ExtractedReference) -> bool {
    reference.kind == ReferenceKind::Calls
        && reference.resolution_name.is_none()
        && reference.name.contains('.')
}

fn apply_call_context(
    calls: &mut HashMap<u64, Option<Companion>>,
    context: &JavascriptMemberCallContext,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if let JavascriptMemberReceiver::Constructor(name) = &context.receiver {
        charge_entry::<(u64, Option<Companion>)>(budget, name.len())?;
        return merge_call(
            calls,
            (
                context.end_byte,
                Some(Companion::Constructor(try_clone_text(name)?)),
            ),
        );
    }
    if let Some(Some(Companion::Receiver { binding, .. })) = calls.get_mut(&context.end_byte) {
        *binding = match context.receiver {
            JavascriptMemberReceiver::LocalImport => ReceiverBinding::LocalImport,
            _ => ReceiverBinding::Shadowed,
        };
    }
    Ok(())
}

fn value_companion(reference: &ExtractedReference) -> bool {
    reference.kind == ReferenceKind::References
        && !reference.name.contains('.')
        && reference
            .resolution_name
            .as_deref()
            .is_none_or(|name| name.starts_with(LEXICAL_SCOPE_RESOLUTION_PREFIX))
}

pub(super) fn charge_entry<Entry>(
    budget: &mut ResolveBudget,
    text_bytes: usize,
) -> Result<(), StageItemFailure> {
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<Entry>()))
            .saturating_add(usize_to_u64(text_bytes)),
    )
}

fn insert_call(
    calls: &mut HashMap<u64, Option<Companion>>,
    reference: &ExtractedReference,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    let end = reference.span.end_byte();
    let text_bytes = reference.name.len().saturating_add(
        reference
            .owner
            .as_ref()
            .map_or(0, |owner| owner.as_str().len()),
    );
    charge_entry::<(u64, Option<Companion>)>(budget, text_bytes)?;
    let companion = if receiver_companion(reference) {
        normalized_receiver(&reference.name)?.map(|name| Companion::Receiver {
            name,
            binding: ReceiverBinding::Module,
        })
    } else {
        Some(Companion::Value(NamedValue {
            name: try_clone_text(&reference.name)?,
            owner: reference.owner.clone(),
            start_byte: reference.span.start_byte(),
            lexical_scope: reference.resolution_name.is_some(),
        }))
    };
    merge_call(calls, (end, companion))
}

fn merge_call(
    calls: &mut HashMap<u64, Option<Companion>>,
    (end, companion): (u64, Option<Companion>),
) -> Result<(), StageItemFailure> {
    if let Some(existing) = calls.get_mut(&end) {
        if existing.as_ref() != companion.as_ref() {
            *existing = None;
        }
        return Ok(());
    }
    calls.try_reserve(1).map_err(|_| StageItemFailure)?;
    calls.insert(end, companion);
    Ok(())
}

fn normalized_receiver(name: &str) -> Result<Option<String>, StageItemFailure> {
    let mut normalized = String::new();
    normalized
        .try_reserve_exact(name.len())
        .map_err(|_| StageItemFailure)?;
    let mut remaining = name;
    loop {
        let Some(source) = receiver_trivia(remaining) else {
            return Ok(None);
        };
        let Some((identifier, source)) = receiver_identifier(source) else {
            return Ok(None);
        };
        normalized.push_str(identifier);
        let Some(source) = receiver_trivia(source) else {
            return Ok(None);
        };
        if source.is_empty() {
            return Ok(normalized.contains('.').then_some(normalized));
        }
        let Some(source) = source
            .strip_prefix("?.")
            .or_else(|| source.strip_prefix('.'))
        else {
            return Ok(None);
        };
        normalized.push('.');
        remaining = source;
    }
}

fn receiver_trivia(mut source: &str) -> Option<&str> {
    loop {
        source = source.trim_start();
        if let Some(comment) = source.strip_prefix("/*") {
            source = comment.get(comment.find("*/")?.saturating_add(2)..)?;
        } else if let Some(comment) = source.strip_prefix("//") {
            source = comment.get(comment.find(['\r', '\n'])?..)?;
        } else {
            return Some(source);
        }
    }
}

fn receiver_identifier(source: &str) -> Option<(&str, &str)> {
    let mut characters = source.char_indices();
    let (_, first) = characters.next()?;
    if !first.is_alphabetic() && !matches!(first, '_' | '$') {
        return None;
    }
    let end = characters
        .find(|(_, character)| !character.is_alphanumeric() && !matches!(character, '_' | '$'))
        .map_or(source.len(), |(offset, _)| offset);
    Some(source.split_at(end))
}

pub(super) fn binding_name<'index>(
    index: &'index ResolutionIndex,
    (file_id, reference): (&FileId, &ExtractedReference),
) -> Option<&'index str> {
    if reference.kind != ReferenceKind::Calls
        || reference
            .resolution_name
            .as_deref()
            .is_some_and(|name| !name.starts_with(super::DYNAMIC_DISPATCH_RESOLUTION_PREFIX))
    {
        return None;
    }
    index
        .javascript_members
        .calls
        .get(file_id)?
        .get(&reference.span.end_byte())?
        .as_ref()
        .map(Companion::name)
}

fn companion<'index>(
    index: &'index ResolutionIndex,
    request: &ResolutionRequest<'_>,
) -> Option<&'index Companion> {
    index
        .javascript_members
        .calls
        .get(request.file_id)?
        .get(&request.span.end_byte())?
        .as_ref()
}

pub(super) fn guard<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !javascript_family_name(request.language) || request.kind != ReferenceKind::Calls {
        return Ok(None);
    }
    if request.dispatch == ReferenceDispatch::Static {
        return guard_static_receiver(index, request, cancelled);
    }
    if request.dispatch != ReferenceDispatch::Dynamic {
        return Ok(None);
    }
    if let Some(Companion::Constructor(name)) = companion(index, request) {
        return resolve_constructor(index, (request, name), cancelled).map(Some);
    }
    if let Some(Companion::Value(value)) = companion(index, request) {
        return resolve_named_value(
            NamedValueQuery {
                index,
                request,
                value,
            },
            cancelled,
        )
        .map(Some);
    }
    let Some(Companion::Receiver { name, binding }) = companion(index, request) else {
        return Ok(Some(ReferenceResolution::unresolved(
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE,
        )));
    };
    if name.rsplit('.').next() != Some(request.name) {
        return Ok(Some(ReferenceResolution::unresolved(
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE,
        )));
    }
    let qualified = ResolutionRequest {
        name,
        dispatch: ReferenceDispatch::Static,
        ..*request
    };
    if *binding == ReceiverBinding::LocalImport {
        return Ok(Some(ReferenceResolution::unresolved(
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE,
        )));
    }
    if *binding == ReceiverBinding::Shadowed {
        return shadowed_call(&qualified, cancelled);
    }
    if let Some(target) = resolve_lexical(index, &qualified, cancelled)? {
        return Ok(Some(ReferenceResolution::resolved(target)));
    }
    if let Some(resolution) = receiver_import(
        ReceiverImportQuery {
            index,
            qualified: &qualified,
            member: request.name,
            dispatch: request.dispatch,
        },
        cancelled,
    )? {
        return Ok(Some(resolution));
    }
    if javascript_intrinsic_reference(&qualified) {
        return Ok(Some(ReferenceResolution::unresolved(
            JAVASCRIPT_INTRINSIC_UNRESOLVED_PROVENANCE,
        )));
    }
    Ok(builtin_member(request.name)
        .then(|| ReferenceResolution::unresolved(DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE)))
}

fn guard_static_receiver<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(Companion::Receiver { name, binding }) = companion(index, request) else {
        return Ok(None);
    };
    let qualified = ResolutionRequest { name, ..*request };
    if *binding == ReceiverBinding::LocalImport {
        return Ok(None);
    }
    if *binding == ReceiverBinding::Shadowed {
        return shadowed_call(&qualified, cancelled);
    }
    let project_import = matches!(
        matched_import_binding(&qualified, ImportReferenceSite::Usage, cancelled)?,
        ImportBindingMatch::Unique(binding, _) if import_binding_is_project_local(index, binding, &qualified)
    );
    if !project_import {
        return Ok(None);
    }
    if let Some(target) = resolve_lexical(index, &qualified, cancelled)? {
        return Ok(Some(ReferenceResolution::resolved(target)));
    }
    receiver_import(
        ReceiverImportQuery {
            index,
            qualified: &qualified,
            member: name.rsplit('.').next().ok_or(StageItemFailure)?,
            dispatch: request.dispatch,
        },
        cancelled,
    )
}

fn shadowed_call<Cancel>(
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let imported = !matches!(
        matched_import_binding(request, ImportReferenceSite::Usage, cancelled)?,
        ImportBindingMatch::NotBound
    );
    Ok(
        (imported || builtin_member(request.name.rsplit('.').next().unwrap_or(request.name)))
            .then(|| ReferenceResolution::unresolved(DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE)),
    )
}

fn resolve_constructor<Cancel>(
    index: &ResolutionIndex,
    (request, constructor): (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let lookup = ResolutionRequest {
        name: constructor,
        kind: ReferenceKind::Instantiates,
        dispatch: ReferenceDispatch::Static,
        ..*request
    };
    let imported = resolve_import(
        index,
        ImportResolutionRequest {
            reference: &lookup,
            site: ImportReferenceSite::Usage,
        },
        cancelled,
    )?;
    let ImportResolution::Resolved(target) = imported else {
        return Ok(ReferenceResolution::unresolved(
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE,
        ));
    };
    let Some(class) = index.javascript_members.classes.get(&target.symbol_id) else {
        return Ok(ReferenceResolution::unresolved(
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE,
        ));
    };
    let mut buffer = [0_u8; MAX_SYMBOL_QUALIFIED_NAME_BYTES];
    let Some(name) = constructor_member_name((&class.qualified_name, request.name), &mut buffer)
    else {
        return Ok(ReferenceResolution::unresolved(
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE,
        ));
    };
    let source = project_source_context(index, request)?;
    let candidates = resolution_candidates_for_file(index, name, &class.file_id);
    let candidate = select_candidate(
        candidates,
        |candidate| {
            candidate.parent_symbol_id.as_ref() == Some(&target.symbol_id)
                && project_eligible(index, (request, source), candidate)
        },
        cancelled,
    )?;
    Ok(candidate.map_or(
        ReferenceResolution::unresolved(DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE),
        |candidate| ReferenceResolution::resolved(project_resolved_target(candidate)),
    ))
}

fn constructor_member_name<'buffer>(
    (class, member): (&str, &str),
    buffer: &'buffer mut [u8],
) -> Option<&'buffer str> {
    let length = class.len().checked_add(2)?.checked_add(member.len())?;
    let name = buffer.get_mut(..length)?;
    name.get_mut(..class.len())?
        .copy_from_slice(class.as_bytes());
    name.get_mut(class.len()..class.len().checked_add(2)?)?
        .copy_from_slice(b"::");
    name.get_mut(class.len().checked_add(2)?..)?
        .copy_from_slice(member.as_bytes());
    std::str::from_utf8(name).ok()
}

fn project_eligible(
    index: &ResolutionIndex,
    (request, source): (&ResolutionRequest<'_>, &super::ResolutionFileContext),
    candidate: &ResolutionCandidate,
) -> bool {
    class_member(index, candidate)
        && reference_kind_candidate(request.kind, candidate)
        && is_project_candidate(ProjectCandidateInput {
            modules: &index.modules,
            source,
            source_file_id: request.file_id,
            reference_name: request.name,
            dynamic_dispatch: request.dispatch == ReferenceDispatch::Dynamic,
            rust_local_import: false,
            candidate,
        })
}

#[derive(Clone, Copy)]
struct NamedValueQuery<'index, 'request> {
    index: &'index ResolutionIndex,
    request: &'index ResolutionRequest<'request>,
    value: &'index NamedValue,
}

fn resolve_named_value<Cancel>(
    query: NamedValueQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let NamedValueQuery {
        index,
        request,
        value,
    } = query;
    if value.start_byte != request.span.start_byte() || value.name != request.name {
        return Ok(ReferenceResolution::unresolved(
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE,
        ));
    }
    let value_request = ResolutionRequest {
        name: &value.name,
        owner: value.owner.as_ref(),
        kind: ReferenceKind::Calls,
        dispatch: ReferenceDispatch::Static,
        ..*request
    };
    let lexical = if value.lexical_scope {
        resolve_lexical_scope(
            index,
            LexicalScopeQuery {
                request: &value_request,
                candidates: resolution_candidates_for_file(index, &value.name, request.file_id),
            },
            cancelled,
        )?
    } else {
        resolve_lexical(index, &value_request, cancelled)?
    };
    if let Some(target) = lexical {
        return Ok(named_value_resolution(target));
    }
    let imported = resolve_import(
        index,
        ImportResolutionRequest {
            reference: &value_request,
            site: ImportReferenceSite::Usage,
        },
        cancelled,
    )?;
    Ok(match imported {
        ImportResolution::Resolved(target) => named_value_resolution(target),
        ImportResolution::NotBound | ImportResolution::Unresolved => {
            ReferenceResolution::unresolved(DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE)
        }
    })
}

fn named_value_resolution(target: ResolvedTarget) -> ReferenceResolution {
    if matches!(target.kind, SymbolKind::Function | SymbolKind::Method) {
        ReferenceResolution::resolved(target)
    } else {
        ReferenceResolution::unresolved(DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE)
    }
}

#[derive(Clone, Copy)]
struct ReceiverImportQuery<'index, 'request> {
    index: &'index ResolutionIndex,
    qualified: &'index ResolutionRequest<'request>,
    member: &'index str,
    dispatch: ReferenceDispatch,
}

fn receiver_import<Cancel>(
    query: ReceiverImportQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if native_modules_import(&query, cancelled)? {
        return Ok(None);
    }
    let ReceiverImportQuery {
        index,
        qualified,
        member,
        dispatch,
    } = query;
    let resolution = resolve_import(
        index,
        ImportResolutionRequest {
            reference: qualified,
            site: ImportReferenceSite::Usage,
        },
        cancelled,
    )?;
    match resolution {
        ImportResolution::NotBound => Ok((reference_import_scope(index, qualified, cancelled)?
            == ImportScope::NonLocal)
            .then(|| ReferenceResolution::unresolved(DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE))),
        ImportResolution::Unresolved => Ok(Some(ReferenceResolution::unresolved(
            UNRESOLVED_IMPORT_PROVENANCE,
        ))),
        ImportResolution::Resolved(target) => {
            let namespace_member = matches!(
                matched_import_binding(qualified, ImportReferenceSite::Usage, cancelled)?,
                ImportBindingMatch::Unique(binding, imported_name)
                    if binding.kind == ImportBindingKind::Namespace && imported_name == member
            );
            if namespace_member {
                let resolution = if dispatch == ReferenceDispatch::Static {
                    ReferenceResolution::resolved(target)
                } else {
                    named_value_resolution(target)
                };
                return Ok(Some(resolution));
            }
            if !matches!(
                target.kind,
                SymbolKind::Class | SymbolKind::Function | SymbolKind::Method
            ) {
                // Opaque imported values keep their existing consumer edge;
                // refining their runtime properties would need type inference.
                return Ok((dispatch == ReferenceDispatch::Dynamic).then(|| {
                    ReferenceResolution::unresolved(DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE)
                }));
            }
            if cancelled() {
                return Err(StageItemFailure);
            }
            let member = imported_static_member(&query, &target);
            Ok(Some(member.map_or(
                ReferenceResolution::unresolved(DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE),
                |symbol_id| {
                    ReferenceResolution::resolved(ResolvedTarget {
                        symbol_id: symbol_id.clone(),
                        kind: SymbolKind::Method,
                        ..target
                    })
                },
            )))
        }
    }
}

fn native_modules_import<Cancel>(
    query: &ReceiverImportQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if query.dispatch != ReferenceDispatch::Dynamic
        || !native_bridge_lookup(query.index, query.member)
    {
        return Ok(false);
    }
    let ImportBindingMatch::Unique(binding, _) =
        matched_import_binding(query.qualified, ImportReferenceSite::Usage, cancelled)?
    else {
        return Ok(false);
    };
    if binding.kind != ImportBindingKind::Named
        || binding.module_specifier != "react-native"
        || binding.imported_name != "NativeModules"
        || import_binding_is_project_local(query.index, binding, query.qualified)
    {
        return Ok(false);
    }
    // This external import names the native registry, so the existing bridge
    // tiers own its dynamic member lookup after the receiver guards have run.
    let suffix = query
        .qualified
        .name
        .strip_prefix(&binding.local_name)
        .and_then(|suffix| suffix.strip_prefix('.'));
    Ok(suffix
        .and_then(|suffix| suffix.split_once('.'))
        .is_some_and(|(module, member)| !module.is_empty() && member == query.member))
}

fn imported_static_member<'index>(
    query: &ReceiverImportQuery<'index, '_>,
    target: &ResolvedTarget,
) -> Option<&'index SymbolId> {
    if target.kind != SymbolKind::Class
        || query
            .qualified
            .name
            .split_once('.')
            .is_none_or(|(_, suffix)| suffix != query.member)
    {
        return None;
    }
    query
        .index
        .javascript_members
        .static_members
        .get(&target.symbol_id)?
        .get(query.member)?
        .as_ref()
}

pub(super) fn resolve_project<Cancel>(
    query: &mut ProjectResolutionQuery<'_, '_, Cancel>,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !javascript_family_name(query.request.language)
        || native_bridge_lookup(query.index, query.request.name)
    {
        return Ok(None);
    }
    if query.request.dispatch != ReferenceDispatch::Dynamic {
        return Ok((query.request.kind == ReferenceKind::Calls
            && builtin_member(query.request.name))
        .then(|| ReferenceResolution::unresolved(DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE)));
    }
    let candidate = select_candidate(
        ProjectResolutionCandidates::new(query.candidate_bucket, query.source, query.request.name)?,
        |candidate| project_eligible(query.index, (query.request, query.source), candidate),
        query.cancelled,
    )?;
    Ok(Some(candidate.map_or(
        ReferenceResolution::unresolved(DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE),
        |candidate| ReferenceResolution::resolved(project_resolved_target(candidate)),
    )))
}

pub(super) fn native_bridge_symbol(symbol: &NativeSymbolFacts, language: &str) -> bool {
    (javascript_family_name(language) || native_bridge_target_language(language))
        && framework_resolution_alias(symbol, language).is_some()
}

fn native_bridge_lookup(index: &ResolutionIndex, name: &str) -> bool {
    index
        .candidates
        .get(name)
        .is_some_and(|bucket| bucket.native_bridge_member)
}

pub(super) fn accessible_member(candidate: &ResolutionCandidate) -> bool {
    matches!(candidate.kind, SymbolKind::Method | SymbolKind::Function)
        && !matches!(
            candidate.visibility,
            Some(Visibility::Private | Visibility::Protected)
        )
        && !candidate
            .qualified_name
            .rsplit("::")
            .next()
            .is_some_and(|name| name.starts_with('#'))
}

pub(super) fn class_member(index: &ResolutionIndex, candidate: &ResolutionCandidate) -> bool {
    accessible_member(candidate)
        && candidate
            .parent_symbol_id
            .as_ref()
            .is_some_and(|parent| index.javascript_members.classes.contains_key(parent))
}

pub(super) fn unique_candidate<'candidate, Candidates, Eligible, Cancel>(
    candidates: Candidates,
    mut eligible: Eligible,
    cancelled: &mut Cancel,
) -> Result<Option<&'candidate ResolutionCandidate>, StageItemFailure>
where
    Candidates: IntoIterator<Item = &'candidate ResolutionCandidate>,
    Eligible: FnMut(&ResolutionCandidate) -> bool,
    Cancel: FnMut() -> bool,
{
    let mut selected = None;
    for candidate in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if eligible(candidate) {
            if selected.is_some() {
                return Ok(None);
            }
            selected = Some(candidate);
        }
    }
    Ok(selected)
}

fn builtin_member(name: &str) -> bool {
    matches!(
        name,
        "set"
            | "get"
            | "has"
            | "delete"
            | "add"
            | "clear"
            | "map"
            | "filter"
            | "reduce"
            | "reduceRight"
            | "forEach"
            | "find"
            | "findIndex"
            | "some"
            | "every"
            | "push"
            | "pop"
            | "shift"
            | "unshift"
            | "slice"
            | "splice"
            | "concat"
            | "join"
            | "flat"
            | "flatMap"
            | "fill"
            | "sort"
            | "reverse"
            | "indexOf"
            | "lastIndexOf"
            | "includes"
            | "keys"
            | "values"
            | "entries"
            | "next"
            | "split"
            | "replace"
            | "replaceAll"
            | "trim"
            | "trimStart"
            | "trimEnd"
            | "startsWith"
            | "endsWith"
            | "padStart"
            | "padEnd"
            | "repeat"
            | "substring"
            | "substr"
            | "charAt"
            | "charCodeAt"
            | "codePointAt"
            | "toLowerCase"
            | "toUpperCase"
            | "then"
            | "catch"
            | "finally"
            | "toString"
            | "valueOf"
            | "hasOwnProperty"
            | "size"
            | "length"
            | "exec"
            | "test"
            | "match"
            | "matchAll"
            | "search"
            | "normalize"
            | "localeCompare"
            | "isPrototypeOf"
            | "propertyIsEnumerable"
    )
}
