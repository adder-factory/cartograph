//! Rust 2018 path heads name current-module declarations before external crates.
use super::{
    ResolutionIndex, ResolutionRequest, StageItemFailure, resolution_candidates_for_file,
    rust_inline_modules::{self, ModuleScope},
};

pub(super) enum Anchor {
    Unchanged,
    Local(String),
    Unproven,
}

impl Anchor {
    pub(super) fn path<'a>(&'a self, original: &'a str) -> Option<&'a str> {
        match self {
            Self::Unchanged => Some(original),
            Self::Local(path) => Some(path),
            Self::Unproven => None,
        }
    }
}

pub(super) fn anchor<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<Anchor, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, path) = query;
    let head = path.split("::").next().unwrap_or(path);
    if path.starts_with("::") || matches!(head, "crate" | "self" | "super") {
        return Ok(Anchor::Unchanged);
    }
    let Some(scope) = rust_inline_modules::enclosing(index, request, cancelled)? else {
        return Ok(Anchor::Unproven);
    };
    if !declared_head(index, (scope, path), cancelled)? {
        return Ok(Anchor::Unchanged);
    }
    // The existing use proof includes name-aware generics, nearer items,
    // statement/item macros, and the exact root-impl source scope.
    if !super::rust_local_types::use_scope_proven(index, request, cancelled)?
        || !super::rust_use_bindings::unshadowed(index, (request, Some(scope)), cancelled)?
    {
        return Ok(Anchor::Unproven);
    }
    Ok(rust_inline_modules::qualified_name("self", path)?.map_or(Anchor::Unproven, Anchor::Local))
}

pub(super) fn declared_head<Cancel>(
    index: &ResolutionIndex,
    query: (ModuleScope<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (scope, path) = query;
    let head = path.split("::").next().unwrap_or(path);
    let Some(name) = rust_inline_modules::qualified_name(scope.inline, head)? else {
        return Ok(false);
    };
    if super::rust_path_guards::module_declared(index, (scope.file, &name)).is_some() {
        return Ok(true);
    }
    if index
        .rust_paths
        .modules
        .get(scope.file)
        .is_some_and(|edges| edges.contains_key(&name))
    {
        // A duplicate or missing child still shadows a namesake dependency.
        return Ok(true);
    }
    for candidate in resolution_candidates_for_file(index, &name, scope.file) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.qualified_name == name
            && rust_inline_modules::owns(scope, candidate)
            && (!path.contains("::")
                || candidate.kind == super::SymbolKind::Module
                || super::qualtype_resolution::nominal_candidate(candidate.kind))
        {
            return Ok(true);
        }
    }
    Ok(false)
}
