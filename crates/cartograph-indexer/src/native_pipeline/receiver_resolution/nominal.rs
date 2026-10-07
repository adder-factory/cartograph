//! Class and mixin inheritance reuse the receiver owner's bounded graph.

use super::{
    Class, Member, MemberSearch, ParentDeclaration, ReferenceKind, ResolutionIndex,
    ResolutionRequest, ResolvedTarget, StageItemFailure, direct_member, inherited_member,
    member_target,
};

pub(super) fn language(language: &str) -> bool {
    matches!(language, "apex" | "dart" | "ruby" | "groovy" | "scala")
}

pub(super) fn record_direct_base(
    target: &mut super::ResolutionIndexTarget<'_>,
    (owner, parent, kind): (&super::SymbolId, &Option<String>, ReferenceKind),
) -> Result<(), StageItemFailure> {
    if kind != ReferenceKind::Extends {
        return Ok(());
    }
    let Some(class) = target.index.receivers.classes.get_mut(owner.as_str()) else {
        return Ok(());
    };
    target.budget.charge(
        parent
            .as_ref()
            .map_or(0, |name| super::usize_to_u64(name.len())),
    )?;
    class.direct_base = parent
        .as_deref()
        .map(super::super::try_clone_text)
        .transpose()?;
    Ok(())
}

pub(in super::super) fn constructor_redirect<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<super::ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language != "dart" {
        return Ok(None);
    }
    let Some(name) = request.name.strip_prefix("dart-constructor-redirect:") else {
        return Ok(None);
    };
    redirect(index, (request, name), cancelled)
        .map(|target| target.map(super::ReferenceResolution::resolved))
}

pub(in super::super) fn redirect<Cancel>(
    index: &ResolutionIndex,
    (request, name): (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if cancelled() {
        return Err(StageItemFailure);
    }
    let Some(class) = request
        .owner
        .and_then(|owner| index.parents.get(owner))
        .and_then(|id| index.receivers.classes.get(id.as_str()))
    else {
        return Ok(None);
    };
    let (receiver, member) = name
        .split_once('.')
        .map_or((name, None), |(receiver, member)| (receiver, Some(member)));
    let class = match receiver {
        "this" => Some(class),
        "super" => class
            .direct_base
            .as_ref()
            .and_then(|base| index.receivers.classes.get(base)),
        _ => None,
    };
    let Some(class) = class else { return Ok(None) };
    let name = member.unwrap_or(&class.name);
    let Some(super::IndexedMember::Unique(target)) = class.members.get(name) else {
        return Ok(None);
    };
    if !target.constructor
        || (target.visibility == Some(super::Visibility::Private)
            && &class.file_id != request.file_id)
    {
        return Ok(None);
    }
    Ok(Some(ResolvedTarget {
        symbol_id: target.symbol_id.clone(),
        kind: target.kind,
        confidence: 1.0,
        provenance: "native-dart-constructor-redirect",
    }))
}

pub(super) fn parent_class<'index, Cancel>(
    index: &'index ResolutionIndex,
    (declaration, request): (&ParentDeclaration, &ResolutionRequest<'_>),
    cancelled: &mut Cancel,
) -> Result<Option<&'index Class>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if declaration.blocked {
        return Ok(None);
    }
    let request = ResolutionRequest {
        owner: index.parents.get(&declaration.owner),
        kind: if request.language == "ruby" {
            ReferenceKind::FieldAccess
        } else {
            ReferenceKind::TypeOf
        },
        ..*request
    };
    let resolution = super::super::resolve_reference(index, &request, cancelled)?;
    Ok(resolution
        .target
        .filter(|target| target.confidence >= super::super::EXACT_PROJECT_CONFIDENCE)
        .and_then(|target| index.receivers.classes.get(target.symbol_id.as_str())))
}

pub(super) fn current_member<Cancel>(
    index: &ResolutionIndex,
    reference: &super::ExtractedReference,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if reference.kind != ReferenceKind::Calls || super::lexical::shadowed(index, reference) {
        return Ok(None);
    }
    let Some(class) = super::super::generic_resolution::current_instance_class(index, reference)
        .and_then(|owner| index.receivers.classes.get(owner.as_str()))
    else {
        return Ok(None);
    };
    let name = reference
        .name
        .strip_prefix("this.")
        .unwrap_or(&reference.name);
    let mut search = MemberSearch {
        index,
        class,
        receiver: class,
        name,
        kind: reference.kind,
    };
    Ok(match direct_member(&mut search, cancelled)? {
        Member::Missing => {
            inherited_member(search, cancelled)?.map(|candidate| member_target(candidate, true))
        }
        Member::Unique(_) | Member::Ambiguous => None,
    })
}
