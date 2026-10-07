//! JSX `Context.Provider`/`Consumer` denotes the context value, not an arbitrary
//! member guessed from its property name. Detection and exact buckets run first.
use super::super::{
    FRAMEWORK_CONVENTION_CONFIDENCE, ImportReferenceSite, ImportResolution,
    ImportResolutionRequest, JSX_CONTEXT_UNBOUND_RESOLUTION_PREFIX, ResolutionIndexFileInput,
    resolve_import, resolve_lexical,
};
use super::{
    FileId, HashSet, ResolutionIndex, ResolutionRequest, ResolvedTarget, SourceSpan,
    StageItemFailure, SymbolKind, charge_entry,
};

pub(super) const PROVENANCE: &str = "native-react-context-member";

pub(super) fn index_sites<Cancel>(
    input: &mut ResolutionIndexFileInput<'_, '_, '_, '_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let file = &input.file.file.file_id;
    let sites = &mut input.index.javascript_frameworks.unbound_context_sites;
    for reference in &input.file.references {
        if (input.cancelled)() {
            return Err(StageItemFailure);
        }
        if !reference.resolution_name.as_deref().is_some_and(|name| {
            name.strip_prefix(JSX_CONTEXT_UNBOUND_RESOLUTION_PREFIX)
                .is_some()
        }) {
            continue;
        }
        if !sites.contains_key(file) {
            charge_entry::<(FileId, HashSet<SourceSpan>)>(input.budget, file.as_str().len())?;
            sites.try_reserve(1).map_err(|_| StageItemFailure)?;
            sites.insert(file.clone(), HashSet::new());
        }
        let spans = sites.get_mut(file).ok_or(StageItemFailure)?;
        charge_entry::<SourceSpan>(input.budget, 0)?;
        spans.try_reserve(1).map_err(|_| StageItemFailure)?;
        spans.insert(reference.span);
    }
    Ok(())
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.kind != super::super::ReferenceKind::References
        || request.import_bindings.fallback_blocked
        || !matches!(request.language, "jsx" | "tsx")
        || index
            .javascript_frameworks
            .unbound_context_sites
            .get(request.file_id)
            .is_some_and(|sites| sites.contains(&request.span))
    {
        return Ok(None);
    }
    // Native JSX capture annotates unproven bindings; the framework builder
    // also excludes newly added dotted references such as config/registry
    // literals. Ordinary member reads have FieldAccess kind.
    let Some(context) = request
        .name
        .strip_suffix(".Provider")
        .or_else(|| request.name.strip_suffix(".Consumer"))
        .filter(|context| context.ends_with("Context") && !context.contains(['.', ':']))
    else {
        return Ok(None);
    };
    let receiver = ResolutionRequest {
        name: context,
        ..*request
    };
    let target = match resolve_lexical(index, &receiver, cancelled)? {
        Some(target) => Some(target),
        None => match resolve_import(
            index,
            ImportResolutionRequest {
                reference: &receiver,
                site: ImportReferenceSite::Usage,
            },
            cancelled,
        )? {
            ImportResolution::Resolved(target) => Some(target),
            ImportResolution::NotBound | ImportResolution::Unresolved => None,
        },
    };
    Ok(target
        .filter(|target| matches!(target.kind, SymbolKind::Constant | SymbolKind::Variable))
        .map(|mut target| {
            target.confidence = target.confidence.min(FRAMEWORK_CONVENTION_CONFIDENCE);
            target.provenance = PROVENANCE;
            target
        }))
}
