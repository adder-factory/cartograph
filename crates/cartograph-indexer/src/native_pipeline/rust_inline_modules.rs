//! Relative module anchors require the reference owner's actual source scope.
//! A nominal declaration elsewhere cannot establish the scope of an impl body.

use super::{
    FileId, ResolutionCandidate, ResolutionIndex, ResolutionRequest, StageItemFailure, SymbolId,
    SymbolKind,
};

#[derive(Clone, Copy)]
pub(super) struct ModuleScope<'a> {
    pub(super) file: &'a FileId,
    pub(super) inline: &'a str,
    pub(super) module: Option<&'a SymbolId>,
}

pub(super) fn enclosing<'a, Cancel>(
    index: &'a ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ModuleScope<'a>>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some((file, _)) = index.modules.files.get_key_value(request.file_id) else {
        return Ok(None);
    };
    let root = ModuleScope {
        file,
        inline: "",
        module: None,
    };
    if root_import(index, request) {
        return Ok(Some(root));
    }
    let mut owner = request.owner;
    let mut nearest = None;
    let mut child_module = None;
    for _ in 0..=index.parents.len().saturating_add(1) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(id) = owner else {
            return Ok(None);
        };
        if index.file_symbols.get(request.file_id) == Some(id) {
            return Ok(child_module.is_none().then_some(nearest.unwrap_or(root)));
        }
        let Some((id, evidence)) = index.qualtype.owners.get_key_value(id) else {
            return Ok(None);
        };
        if !valid_owner(evidence, (request, child_module)) {
            return Ok(None);
        }
        if root_impl_method(index, (request, evidence)) {
            return Ok(Some(root));
        }
        if evidence.kind == SymbolKind::Module {
            nearest.get_or_insert(ModuleScope {
                file,
                inline: &evidence.name,
                module: Some(id),
            });
            child_module = evidence.name.rsplit_once("::").map(|(parent, _)| parent);
        }
        owner = index.parents.get(id);
        if owner.is_none() {
            return Ok((!evidence.name.contains("::")).then_some(nearest.unwrap_or(root)));
        }
    }
    Ok(None)
}

/// A file-level `use` declaration belongs to the file's root module.
fn root_import(index: &ResolutionIndex, request: &ResolutionRequest<'_>) -> bool {
    request.owner.is_none()
        && request.kind == super::ReferenceKind::Imports
        && super::rust_local_types::root_declaration(index, (request.file_id, request.span))
}

/// The AST marker proves a root impl's scope independently of the nominal
/// declaration, whose span does not contain its method bodies.
fn root_impl_method(
    index: &ResolutionIndex,
    (request, owner): (&ResolutionRequest<'_>, &super::qualtype_resolution::Owner),
) -> bool {
    owner.kind == SymbolKind::Method && super::rust_use_bindings::plain_impl(index, request)
}

fn valid_owner(
    owner: &super::qualtype_resolution::Owner,
    query: (&ResolutionRequest<'_>, Option<&str>),
) -> bool {
    let (request, enclosing_module) = query;
    owner
        .source_scope
        .as_ref()
        .is_some_and(|(file, start, end)| {
            file == request.file_id
                && *start <= request.span.start_byte()
                && *end >= request.span.end_byte()
        })
        && enclosing_module
            .is_none_or(|name| owner.kind == SymbolKind::Module && name == owner.name)
}

pub(super) fn owns(scope: ModuleScope<'_>, candidate: &ResolutionCandidate) -> bool {
    candidate.parent_symbol_id.as_ref() == scope.module
}

pub(super) fn contains_owner(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, ModuleScope<'_>),
) -> bool {
    let (request, scope) = query;
    scope
        .module
        .and_then(|id| index.qualtype.owners.get(id))
        .is_some_and(|owner| valid_owner(owner, (request, None)))
}

pub(super) fn qualified_name(prefix: &str, name: &str) -> Result<Option<String>, StageItemFailure> {
    let separator = usize::from(!prefix.is_empty()) * "::".len();
    let length = prefix
        .len()
        .saturating_add(separator)
        .saturating_add(name.len());
    if length > 1_024 {
        return Ok(None);
    }
    let mut qualified = String::new();
    qualified
        .try_reserve_exact(length)
        .map_err(|_| StageItemFailure)?;
    qualified.push_str(prefix);
    if separator > 0 {
        qualified.push_str("::");
    }
    qualified.push_str(name);
    Ok(Some(qualified))
}
