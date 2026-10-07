//! Internal visibility proves at least the declaring module's parent subtree.
//! This is the common exact subset of `pub(crate)` and `pub(super)` evidence.
use super::{
    ResolutionCandidate, ResolutionIndex, ResolutionRequest, StageItemFailure, Visibility,
    resolution_candidates_for_file,
    rust_inline_modules::{self, ModuleScope},
    rust_root_ownership,
};

pub(super) fn visible<Cancel>(
    index: &ResolutionIndex,
    query: (
        &ResolutionRequest<'_>,
        ModuleScope<'_>,
        &str,
        &ResolutionCandidate,
    ),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, scope, name, candidate) = query;
    if !declaration_visible(index, (request, scope, candidate.visibility), cancelled)? {
        return Ok(false);
    }
    if rust_inline_modules::owns(scope, candidate) {
        return Ok(true);
    }
    let Some((parent_name, _)) = name.rsplit_once("::") else {
        return Ok(false);
    };
    let Some(parent_name) = rust_inline_modules::qualified_name(scope.inline, parent_name)? else {
        return Ok(false);
    };
    let mut selected = None;
    for parent in resolution_candidates_for_file(index, &parent_name, scope.file) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if parent.qualified_name != parent_name
            || !(parent.kind == super::SymbolKind::Module
                || super::qualtype_resolution::nominal_candidate(parent.kind))
            || !rust_inline_modules::owns(scope, parent)
        {
            continue;
        }
        if selected.is_some() {
            return Ok(false);
        }
        selected = Some(parent);
    }
    let Some(parent) = selected else {
        return Ok(false);
    };
    if !super::qualtype_resolution::nominal_candidate(parent.kind) {
        return Ok(false);
    }
    Ok(
        candidate.parent_symbol_id.as_ref() == Some(&parent.symbol_id)
            && declaration_visible(index, (request, scope, parent.visibility), cancelled)?,
    )
}

pub(super) fn declaration_visible<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, ModuleScope<'_>, Option<Visibility>),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, scope, visibility) = query;
    if visibility == Some(Visibility::Public)
        || rust_inline_modules::contains_owner(index, (request, scope))
    {
        return Ok(true);
    }
    let Some(root) = rust_root_ownership::root(index, scope.file) else {
        return Ok(false);
    };
    if rust_root_ownership::root(index, request.file_id) != Some(root) {
        return Ok(false);
    }
    let target = if visibility == Some(Visibility::Internal) {
        rust_root_ownership::parent_module(index, scope).unwrap_or(scope)
    } else {
        scope
    };
    let Some(mut source) = rust_inline_modules::enclosing(index, request, cancelled)? else {
        return Ok(false);
    };
    for _ in 0..=index
        .parents
        .len()
        .saturating_add(index.modules.files.len())
    {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if source.file == target.file && source.module == target.module {
            return Ok(true);
        }
        let Some(parent) = rust_root_ownership::parent_module(index, source) else {
            return Ok(false);
        };
        source = parent;
    }
    Ok(false)
}
