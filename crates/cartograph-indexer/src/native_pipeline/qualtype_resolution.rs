//! Small, evidence-bound nominal and enum lookups. The owner catalog is shared
//! by the memory and spilled index builders; it never scans the project per site.

use super::{
    FileId, HashMap, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceKind,
    ReferenceResolution, ResolutionCandidate, ResolutionIndex, ResolutionIndexContext,
    ResolutionIndexTarget, ResolutionRequest, ResolvedTarget, StageItemFailure, SymbolId,
    SymbolKind, size_of, try_clone_text, usize_to_u64,
};

#[derive(Default)]
pub(super) struct TypeIndex {
    pub(super) owners: HashMap<SymbolId, Owner>,
    pub(super) csharp: super::namespace_types::NamespaceImports,
    pub(super) rescript: super::rescript_resolution::Modules,
    pub(super) python: super::python_type_variables::TypeVariables,
    pub(super) python_class_members: super::python_class_members::Members,
    pub(super) enums: super::enum_resolution::Receivers,
    pub(super) rust: super::rust_local_types::RootImports,
    pub(super) generics: super::qualtype_generics::Scopes,
    pub(super) constructors: super::csharp_constructors::Calls,
}

pub(super) struct Owner {
    pub(super) name: String,
    pub(super) kind: SymbolKind,
    pub(super) static_member: bool,
    pub(super) parameter_count: u16,
    pub(super) source_scope: Option<(FileId, u64, u64)>,
}

pub(super) fn index_syntax<Cancel>(
    index: &mut ResolutionIndex,
    file: &NativeFileFacts,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    super::qualtype_generics::index_file(index, file, context)?;
    super::javascript_alias_exports::index_syntax(index, file, context)?;
    super::enum_resolution::index_file(index, file, context)?;
    super::python_class_members::index_file(index, file, context)?;
    super::python_type_variables::index_file(index, file, context)
}

pub(super) fn index_file<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    super::rescript_resolution::index_file(target, file)?;
    super::namespace_types::index_file(target, file, cancelled)?;
    super::python_type_variables::index_module(target, file);
    super::rust_local_types::index_file(target, file, cancelled)?;
    if !matches!(file.file.language.as_str(), "csharp" | "php" | "rust") {
        return Ok(());
    }
    let rust_source = file.file.language == "rust";
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let name = &symbol.input.qualified_name;
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(SymbolId, Owner)>()))
                .saturating_add(usize_to_u64(symbol.input.symbol_id.as_str().len()))
                .saturating_add(usize_to_u64(name.len()))
                .saturating_add(if rust_source {
                    usize_to_u64(file.file.file_id.as_str().len())
                } else {
                    0
                }),
        )?;
        target
            .index
            .qualtype
            .owners
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        target.index.qualtype.owners.insert(
            symbol.input.symbol_id.clone(),
            Owner {
                name: try_clone_text(name)?,
                kind: symbol.kind,
                static_member: symbol.execution.static_member,
                parameter_count: symbol.health.parameter_count,
                source_scope: rust_source.then(|| {
                    (
                        file.file.file_id.clone(),
                        symbol.input.start_byte,
                        symbol.input.end_byte,
                    )
                }),
            },
        );
    }
    Ok(())
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    match request.language {
        "rescript" => super::rescript_resolution::resolve(index, request, cancelled),
        "csharp" => super::namespace_types::resolve(index, request, cancelled),
        "rust" => super::rust_local_types::resolve(index, request, cancelled),
        "python" => super::python_type_variables::resolve(index, request, cancelled),
        _ => Ok(None),
    }
}

pub(super) fn nominal(kind: ReferenceKind) -> bool {
    matches!(
        kind,
        ReferenceKind::TypeOf
            | ReferenceKind::Returns
            | ReferenceKind::Instantiates
            | ReferenceKind::Extends
            | ReferenceKind::Implements
            | ReferenceKind::Inherits
    )
}

pub(super) fn nominal_candidate(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Union
            | SymbolKind::Interface
            | SymbolKind::Trait
            | SymbolKind::Protocol
            | SymbolKind::Enum
            | SymbolKind::TypeAlias
    )
}

#[derive(Default)]
pub(super) struct Selection<'a> {
    pub(super) candidate: Option<&'a ResolutionCandidate>,
    pub(super) ambiguous: bool,
}

impl<'a> Selection<'a> {
    pub(super) fn retain(&mut self, candidate: &'a ResolutionCandidate) {
        if self
            .candidate
            .is_some_and(|prior| prior.symbol_id != candidate.symbol_id)
        {
            self.ambiguous = true;
        }
        self.candidate = Some(candidate);
    }

    pub(super) fn resolution(
        self,
        provenance: &'static str,
        confidence: f32,
    ) -> Option<ReferenceResolution> {
        self.candidate.filter(|_| !self.ambiguous).map(|candidate| {
            ReferenceResolution::resolved(ResolvedTarget {
                symbol_id: candidate.symbol_id.clone(),
                kind: candidate.kind,
                confidence,
                provenance,
            })
        })
    }
}
