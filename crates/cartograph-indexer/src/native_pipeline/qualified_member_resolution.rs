//! Nominal, directly declared member calls. No inherited-member or type inference.

use std::collections::HashMap;

use cartograph_domain::{EdgeKind, FileId, ReferenceKind, SymbolId, SymbolKind, Visibility};
use cartograph_extract::ImportBindingKind;

use super::{
    DYNAMIC_DISPATCH_CONFIDENCE, DYNAMIC_DISPATCH_PROVENANCE, NativeFileFacts, NativeSymbolFacts,
    RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceDispatch, ReferenceResolution, ResolutionCandidate,
    ResolutionIndex, ResolutionRequest, ResolveBudget, ResolvedTarget, StageItemFailure,
    UNRESOLVED_IMPORT_PROVENANCE, binding_matches_reference_name, javascript_family_name,
    javascript_member_resolution, jvm_resolution, nominal_scope_resolution,
    resolution_candidates_for_file, resolve_reference, select_candidate,
    unresolved_reference_provenance, usize_to_u64,
};

const MAX_OWNER_HOPS: usize = 64;

#[derive(Default)]
pub(super) struct TypeIndex {
    types: HashMap<SymbolId, TypeIdentity>,
}

struct TypeIdentity {
    file_id: FileId,
    kind: SymbolKind,
    visibility: Option<Visibility>,
    has_ancestors: bool,
}

impl TypeIndex {
    pub(super) fn kind(&self, id: &SymbolId) -> Option<SymbolKind> {
        self.types.get(id).map(|identity| identity.kind)
    }

    pub(super) fn visibility(&self, id: &SymbolId) -> Option<(Option<Visibility>, &FileId)> {
        self.types
            .get(id)
            .map(|identity| (identity.visibility, &identity.file_id))
    }
}

pub(super) fn index_type(
    types: &mut TypeIndex,
    symbol: &NativeSymbolFacts,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if !receiver_kind(symbol.kind) {
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(SymbolId, TypeIdentity)>()))
            .saturating_add(usize_to_u64(symbol.input.symbol_id.as_str().len()))
            .saturating_add(usize_to_u64(symbol.input.file_id.as_str().len())),
    )?;
    types.types.try_reserve(1).map_err(|_| StageItemFailure)?;
    types.types.insert(
        symbol.input.symbol_id.clone(),
        TypeIdentity {
            file_id: symbol.input.file_id.clone(),
            kind: symbol.kind,
            visibility: symbol.visibility,
            has_ancestors: false,
        },
    );
    Ok(())
}

pub(super) fn index_ancestors<Cancel>(
    index: &mut ResolutionIndex,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for reference in &file.references {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if matches!(
            reference.kind,
            ReferenceKind::Inherits | ReferenceKind::Extends | ReferenceKind::Implements
        ) && let Some(owner) = reference.owner.as_ref()
            && let Some(identity) = index.types.types.get_mut(owner)
        {
            identity.has_ancestors = true;
        }
    }
    Ok(())
}

fn uncertain_ancestor_scope<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut owner = request.owner;
    for _ in 0..MAX_OWNER_HOPS {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(id) = owner else {
            return Ok(false);
        };
        if index
            .types
            .types
            .get(id)
            .is_some_and(|identity| identity.has_ancestors)
        {
            return Ok(true);
        }
        owner = index.parents.get(id);
    }
    Ok(true)
}

fn receiver_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Interface
            | SymbolKind::Enum
            | SymbolKind::Protocol
            | SymbolKind::Trait
    )
}

pub(super) fn inheritance_edge_kind(
    target: SymbolKind,
    source: Option<SymbolKind>,
) -> Option<EdgeKind> {
    match target {
        SymbolKind::Interface | SymbolKind::Trait | SymbolKind::Protocol => Some(
            if matches!(
                source,
                Some(SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum)
            ) {
                EdgeKind::Implements
            } else {
                EdgeKind::Extends
            },
        ),
        SymbolKind::Class | SymbolKind::Struct => Some(EdgeKind::Extends),
        _ => None,
    }
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    // JS owns receiver guards and import provenance; this tier only contributes
    // positive nominal targets after the receiver evidence permits them.
    let javascript = javascript_family_name(request.language);
    if javascript && javascript_member_resolution::shadowed_receiver(index, request) {
        return Ok(None);
    }
    let resolution = resolve_nominal(index, request, cancelled)?;
    Ok(resolution.filter(|resolution| !javascript || resolution.target.is_some()))
}

fn resolve_nominal<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.kind != ReferenceKind::Calls || !member_language(request.language) {
        return Ok(None);
    }
    let Some((receiver, member)) = split_call(request.name) else {
        return Ok(None);
    };
    match import_receiver_route((request, receiver), cancelled)? {
        ImportReceiverRoute::Base => return Ok(None),
        ImportReceiverRoute::Blocked => {
            return Ok(Some(ReferenceResolution::unresolved(
                UNRESOLVED_IMPORT_PROVENANCE,
            )));
        }
        ImportReceiverRoute::Member => {}
    }
    let receiver_request = ResolutionRequest {
        name: receiver,
        kind: ReferenceKind::FieldAccess,
        dispatch: ReferenceDispatch::Static,
        ..*request
    };
    let receiver = match resolve_receiver(index, &receiver_request, cancelled)? {
        ReceiverLookup::Resolved(receiver) => receiver,
        ReceiverLookup::Absent if !lexical_binding_exists(index, &receiver_request, cancelled)? => {
            return Ok(None);
        }
        ReceiverLookup::Absent | ReceiverLookup::Blocked => {
            return Ok(Some(ReferenceResolution::unresolved(
                unresolved_reference_provenance(index, request, cancelled)?,
            )));
        }
    };
    let query = MemberQuery {
        index,
        request,
        receiver,
        member,
    };
    if let Some(target) = resolve_member(&query, cancelled)? {
        return Ok(Some(ReferenceResolution::resolved(target)));
    }
    Ok(Some(ReferenceResolution::unresolved(
        unresolved_reference_provenance(index, request, cancelled)?,
    )))
}

enum ImportReceiverRoute {
    Base,
    Blocked,
    Member,
}

fn import_receiver_route<Cancel>(
    input: (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<ImportReceiverRoute, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, receiver) = input;
    if jvm_resolution::language(request.language) {
        return Ok(ImportReceiverRoute::Member);
    }
    for binding in request.import_bindings.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.kind == ImportBindingKind::Namespace && binding.local_name == receiver {
            return Ok(ImportReceiverRoute::Base);
        }
        if javascript_family_name(request.language)
            && binding.kind == ImportBindingKind::Named
            && binding.local_name == receiver
        {
            return Ok(ImportReceiverRoute::Base);
        }
        if matches!(
            binding.kind,
            ImportBindingKind::Named | ImportBindingKind::Default
        ) && binding.local_name != receiver
            && binding_matches_reference_name(binding, receiver)
        {
            return Ok(ImportReceiverRoute::Blocked);
        }
    }
    Ok(ImportReceiverRoute::Member)
}

fn member_language(language: &str) -> bool {
    jvm_resolution::language(language)
        || javascript_family_name(language)
        || matches!(language, "csharp" | "ruby")
}

fn split_call(name: &str) -> Option<(&str, &str)> {
    let (receiver, member) = name.rsplit_once('.').or_else(|| name.rsplit_once("::"))?;
    if receiver.is_empty()
        || member.is_empty()
        || matches!(receiver, "this" | "self" | "super")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b':' | b'_' | b'$'))
    {
        return None;
    }
    Some((receiver, member))
}

struct Receiver {
    target: ResolvedTarget,
    instance: bool,
}

enum ReceiverLookup {
    Absent,
    Blocked,
    Resolved(Receiver),
}

fn nominal_receiver<Cancel>(
    index: &ResolutionIndex,
    input: (&ResolutionRequest<'_>, ResolvedTarget),
    cancelled: &mut Cancel,
) -> Result<ReceiverLookup, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, target) = input;
    if uncertain_ancestor_scope(index, request, cancelled)? {
        return Ok(ReceiverLookup::Blocked);
    }
    Ok(ReceiverLookup::Resolved(Receiver {
        target,
        instance: false,
    }))
}

fn resolve_receiver<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ReceiverLookup, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if let Some(resolution) = nominal_scope_resolution::resolve(index, request, cancelled)? {
        let Some(target) = resolution.target else {
            return Ok(ReceiverLookup::Blocked);
        };
        return if receiver_kind(target.kind) {
            nominal_receiver(index, (request, target), cancelled)
        } else {
            declared_receiver(index, (request, &target.symbol_id), cancelled)
                .map(|receiver| receiver.map_or(ReceiverLookup::Blocked, ReceiverLookup::Resolved))
        };
    }
    if qualified_head_blocks_lookup(index, request, cancelled)? {
        return Ok(ReceiverLookup::Blocked);
    }
    let type_request = ResolutionRequest {
        kind: ReferenceKind::Instantiates,
        ..*request
    };
    let resolution = if jvm_resolution::language(request.language) {
        jvm_resolution::resolve_type(index, &type_request, cancelled)?
    } else {
        resolve_reference(index, &type_request, cancelled)?
    };
    match resolution
        .target
        .filter(|target| receiver_kind(target.kind))
    {
        Some(target) => nominal_receiver(index, (request, target), cancelled),
        None => Ok(ReceiverLookup::Absent),
    }
}

fn qualified_head_blocks_lookup<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !lexical_binding_exists(index, request, cancelled)? {
        return Ok(false);
    }
    let Some((head, _)) = request.name.split_once('.') else {
        return Ok(true);
    };
    if !jvm_resolution::language(request.language) {
        return Ok(true);
    }
    let head_request = ResolutionRequest {
        name: head,
        ..*request
    };
    Ok(
        nominal_scope_resolution::resolve(index, &head_request, cancelled)?.is_none_or(
            |resolution| {
                resolution
                    .target
                    .is_none_or(|target| !receiver_kind(target.kind))
            },
        ),
    )
}

fn declared_receiver<Cancel>(
    index: &ResolutionIndex,
    input: (&ResolutionRequest<'_>, &SymbolId),
    cancelled: &mut Cancel,
) -> Result<Option<Receiver>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, id) = input;
    if !jvm_resolution::language(request.language) {
        return Ok(None);
    }
    let Some(name) = jvm_resolution::declared_type(index, id) else {
        return Ok(None);
    };
    if !simple_declared_receiver(index, (request, id), cancelled)? {
        return Ok(None);
    }
    let resolution = jvm_resolution::resolve_type(
        index,
        &ResolutionRequest {
            name,
            kind: ReferenceKind::TypeOf,
            owner: Some(id),
            ..*request
        },
        cancelled,
    )?;
    Ok(resolution
        .target
        .filter(|target| receiver_kind(target.kind))
        .map(|target| Receiver {
            target,
            instance: true,
        }))
}

fn lexical_binding_exists<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let head = request.name.split('.').next().unwrap_or(request.name);
    let candidates = resolution_candidates_for_file(index, head, request.file_id);
    let mut scope = request.owner;
    for _ in 0..MAX_OWNER_HOPS {
        for candidate in candidates {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if candidate.parent_symbol_id.as_ref() == scope {
                return Ok(true);
            }
        }
        let Some(id) = scope else {
            return Ok(false);
        };
        scope = index.parents.get(id);
    }
    Ok(true)
}

/// Array, generic, nullable, and compound types cannot be followed as a
/// single nominal receiver merely because they emit one type reference.
fn simple_declared_receiver<Cancel>(
    index: &ResolutionIndex,
    input: (&ResolutionRequest<'_>, &SymbolId),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, id) = input;
    for candidate in resolution_candidates_for_file(index, request.name, request.file_id) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if &candidate.symbol_id == id {
            return Ok(!candidate.signature.is_empty()
                && !candidate
                    .signature
                    .contains(['<', '>', '[', ']', '(', ')', '|', '&', '?']));
        }
    }
    Ok(false)
}

struct MemberQuery<'index, 'request> {
    index: &'index ResolutionIndex,
    request: &'index ResolutionRequest<'request>,
    receiver: Receiver,
    member: &'index str,
}

fn resolve_member<Cancel>(
    query: &MemberQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(identity) = query
        .index
        .types
        .types
        .get(&query.receiver.target.symbol_id)
    else {
        return Ok(None);
    };
    let inside = inside_receiver(
        query.index,
        (&query.receiver.target.symbol_id, query.request.owner),
        cancelled,
    )?;
    let candidates = resolution_candidates_for_file(query.index, query.member, &identity.file_id);
    let mut eligible_count = 0_usize;
    let candidate = select_candidate(
        candidates,
        |candidate| {
            let eligible = candidate.parent_symbol_id.as_ref()
                == Some(&query.receiver.target.symbol_id)
                && candidate.kind == SymbolKind::Method
                && (query.receiver.instance || candidate.static_member)
                && member_visible(query, candidate, inside);
            if eligible {
                eligible_count = eligible_count.saturating_add(1);
            }
            eligible
        },
        cancelled,
    )?;
    // Java/Kotlin/C# declarations can be distinct overloads without bodies.
    // Only JavaScript-family overload signatures share one implementation.
    if eligible_count != 1 && !javascript_family_name(query.request.language) {
        return Ok(None);
    }
    Ok(candidate.map(|candidate| ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: if query.receiver.instance {
            DYNAMIC_DISPATCH_CONFIDENCE
        } else {
            0.95
        },
        provenance: if query.receiver.instance {
            DYNAMIC_DISPATCH_PROVENANCE
        } else {
            "native-qualified-member"
        },
    }))
}

fn member_visible(
    query: &MemberQuery<'_, '_>,
    candidate: &ResolutionCandidate,
    inside: bool,
) -> bool {
    if inside || candidate.visibility == Some(Visibility::Public) {
        return true;
    }
    if matches!(
        candidate.visibility,
        Some(Visibility::Private | Visibility::Protected | Visibility::Internal)
    ) {
        return false;
    }
    !jvm_resolution::language(query.request.language)
        || jvm_resolution::same_package(query.index, [query.request.file_id, &candidate.file_id])
}

fn inside_receiver<Cancel>(
    index: &ResolutionIndex,
    input: (&SymbolId, Option<&SymbolId>),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (receiver, mut owner) = input;
    for _ in 0..MAX_OWNER_HOPS {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(id) = owner else {
            return Ok(false);
        };
        if id == receiver {
            return Ok(true);
        }
        owner = index.parents.get(id);
    }
    Ok(false)
}
