//! Declared project classes and bounded chains through declared class returns.

use super::super::{
    ExtractedImportBinding, FileId, NativeSymbolFacts, SymbolInput, resolve_reference,
};
use super::{
    Class, FileImportBindingIndex, ImportBindingScratch, IndexedMember, MemberDefinition,
    NativeFileFacts, ReferenceDispatch, ReferenceKind, ResolutionIndex, ResolutionIndexTarget,
    ResolutionRequest, ResolvedTarget, SourceSpan, StageItemFailure, SymbolId, SymbolKind,
    try_clone_text, usize_to_u64,
};

const CONSTRUCTOR_PREFIX: &str = "constructor::";
const UNSUPPORTED_CONSTRUCTOR: &str = "constructor::?";
use std::mem::{size_of, take};

const MAX_CHAIN_CALLS: usize = 3;
const MINIMUM_TYPE_CONFIDENCE: f32 = 0.9;
const RETURN_PROVENANCE: &str = "native-declared-return-receiver";

pub(super) fn index_class(
    target: &mut ResolutionIndexTarget<'_>,
    (file, symbol): (&NativeFileFacts, &NativeSymbolFacts),
) -> Result<(), StageItemFailure> {
    target.budget.charge(
        super::RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(String, Class)>()))
            .saturating_add(usize_to_u64(symbol.input.symbol_id.as_str().len()).saturating_mul(2))
            .saturating_add(usize_to_u64(file.file.file_id.as_str().len()))
            .saturating_add(usize_to_u64(symbol.name.len())),
    )?;
    target
        .index
        .receivers
        .classes
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    target.index.receivers.classes.insert(
        try_clone_text(symbol.input.symbol_id.as_str())?,
        Class {
            symbol_id: symbol.input.symbol_id.clone(),
            file_id: file.file.file_id.clone(),
            name: try_clone_text(&symbol.name)?,
            direct_base: None,
            members: std::collections::HashMap::new(),
            non_methods: std::collections::HashSet::new(),
            assigned_members: std::collections::HashSet::new(),
            fenced: false,
        },
    );
    Ok(())
}

pub(super) fn member_definition(
    file: &NativeFileFacts,
    symbol: &NativeSymbolFacts,
) -> IndexedMember {
    IndexedMember::Unique(MemberDefinition {
        symbol_id: symbol.input.symbol_id.clone(),
        kind: symbol.kind,
        visibility: symbol.visibility,
        instance_allowed: matches!(file.file.language.as_str(), "python" | "go")
            || !symbol.execution.static_member,
        constructor: symbol.declaration_syntax == super::super::DeclarationSyntax::DartConstructor,
        return_class: None,
    })
}

pub(in super::super) fn resolve_abstention<Cancel>(
    index: &ResolutionIndex,
    query: super::ReceiverQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<super::ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let receiver = super::resolve(index, query, cancelled)?;
    Ok(super::prefer_base(
        super::ReferenceResolution::unresolved(super::super::UNRESOLVED_PROVENANCE),
        receiver,
    ))
}

fn reserve_slot<T>(
    budget: &mut super::ResolveBudget,
    values: &mut Vec<T>,
) -> Result<(), StageItemFailure> {
    if values.len() < values.capacity() {
        return Ok(());
    }
    let additional = values.capacity().max(1);
    budget.charge(usize_to_u64(additional).saturating_mul(usize_to_u64(size_of::<T>())))?;
    values
        .try_reserve_exact(additional)
        .map_err(|_| StageItemFailure)
}

#[derive(Default)]
pub(super) struct DeclaredIndex {
    pending: Vec<ReturnFile>,
}

struct ReturnFile {
    file_id: FileId,
    imports: Vec<ExtractedImportBinding>,
    declarations: Vec<ReturnDeclaration>,
}

struct ReturnDeclaration {
    class_id: SymbolId,
    method_id: SymbolId,
    member: String,
    name: String,
    span: SourceSpan,
}

pub(super) fn supported(language: &str) -> bool {
    matches!(
        language,
        "java"
            | "kotlin"
            | "csharp"
            | "swift"
            | "dart"
            | "cpp"
            | "scala"
            | "ruby"
            | "apex"
            | "solidity"
            | "ocaml"
            | "powershell"
            | "pascal"
            | "objc"
    )
}

pub(super) fn resolve_type<'index, Cancel>(
    index: &'index ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<&'index Class>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let resolution = resolve_reference(index, request, cancelled)?;
    Ok(resolution
        .target
        .filter(|target| {
            target.confidence >= MINIMUM_TYPE_CONFIDENCE && super::nominal(target.kind)
        })
        .and_then(|target| index.receivers.classes.get(target.symbol_id.as_str())))
}

pub(super) fn index_returns<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if matches!(file.file.language.as_str(), "python" | "go") {
        return Ok(());
    }
    let lookups = super::FileLookups::new(
        file.receiver_evidence
            .as_deref()
            .map_or(&[], |evidence| evidence.lookups.as_slice()),
        target.budget,
        cancelled,
    )?;
    let mut declarations = Vec::new();
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if let Some(declaration) = return_declaration(target, (&lookups, symbol))? {
            reserve_slot(target.budget, &mut declarations)?;
            declarations.push(declaration);
        }
    }
    if declarations.is_empty() {
        return Ok(());
    }
    let imports = clone_imports(target, file, cancelled)?;
    push_file(target, (file, imports, declarations))
}

fn return_declaration(
    target: &mut ResolutionIndexTarget<'_>,
    input: (&super::FileLookups<'_>, &NativeSymbolFacts),
) -> Result<Option<ReturnDeclaration>, StageItemFailure> {
    let (lookups, symbol) = input;
    let Some(class_id) = target
        .index
        .parents
        .get(&symbol.input.symbol_id)
        .filter(|owner| target.index.receivers.classes.contains_key(owner.as_str()))
    else {
        return Ok(None);
    };
    if !matches!(symbol.kind, SymbolKind::Method | SymbolKind::Function) {
        return Ok(None);
    }
    let Some(name) = return_marker(lookups, &symbol.input) else {
        return Ok(None);
    };
    target.budget.charge(usize_to_u64(
        symbol.name.len()
            + name.len()
            + class_id.as_str().len()
            + symbol.input.symbol_id.as_str().len(),
    ))?;
    Ok(Some(ReturnDeclaration {
        class_id: class_id.clone(),
        method_id: symbol.input.symbol_id.clone(),
        member: try_clone_text(&symbol.name)?,
        name: try_clone_text(name)?,
        span: declaration_span(&symbol.input)?,
    }))
}

fn return_marker<'evidence>(
    lookups: &super::FileLookups<'evidence>,
    symbol: &SymbolInput,
) -> Option<&'evidence str> {
    lookups
        .sites
        .get(&(symbol.start_byte, symbol.end_byte, ReferenceKind::Returns))
        .and_then(|marker| *marker)
        .and_then(|marker| marker.strip_prefix(super::EXPLICIT_RECEIVER_RESOLUTION_PREFIX))
        .and_then(|marker| marker.strip_suffix('#'))
}

fn declaration_span(symbol: &SymbolInput) -> Result<SourceSpan, StageItemFailure> {
    SourceSpan::new(
        cartograph_domain::SourcePosition::new(symbol.start_byte, symbol.start_line, 0)
            .map_err(|_| StageItemFailure)?,
        cartograph_domain::SourcePosition::new(symbol.end_byte, symbol.end_line, 0)
            .map_err(|_| StageItemFailure)?,
    )
    .map_err(|_| StageItemFailure)
}

fn push_file(
    target: &mut ResolutionIndexTarget<'_>,
    input: (
        &NativeFileFacts,
        Vec<ExtractedImportBinding>,
        Vec<ReturnDeclaration>,
    ),
) -> Result<(), StageItemFailure> {
    let (file, imports, declarations) = input;
    target
        .budget
        .charge(usize_to_u64(file.file.file_id.as_str().len()))?;
    let pending = &mut target.index.receivers.declared.pending;
    reserve_slot(target.budget, pending)?;
    pending.push(ReturnFile {
        file_id: file.file.file_id.clone(),
        imports,
        declarations,
    });
    Ok(())
}

pub(super) fn clone_imports<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<Vec<ExtractedImportBinding>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut imports = Vec::new();
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        target.budget.charge(usize_to_u64(
            binding.module_specifier.len() + binding.imported_name.len() + binding.local_name.len(),
        ))?;
        reserve_slot(target.budget, &mut imports)?;
        imports.push(ExtractedImportBinding {
            kind: binding.kind,
            span: binding.span,
            module_specifier: try_clone_text(&binding.module_specifier)?,
            imported_name: try_clone_text(&binding.imported_name)?,
            local_name: try_clone_text(&binding.local_name)?,
        });
    }
    Ok(imports)
}

pub(super) fn finish_returns<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for file in take(&mut target.index.receivers.declared.pending) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let metadata = target
            .index
            .modules
            .files
            .get(&file.file_id)
            .ok_or(StageItemFailure)?;
        let bindings =
            FileImportBindingIndex::new(&file.imports, target.budget, &metadata.language)?;
        let mut scratch = ImportBindingScratch::new(file.imports.len(), target.budget)?;
        for declaration in file.declarations {
            if cancelled() {
                return Err(StageItemFailure);
            }
            let name = declared_name(&declaration.name);
            let request = ResolutionRequest {
                file_id: &file.file_id,
                file_path: &metadata.path,
                language: &metadata.language,
                import_bindings: scratch.select(&bindings, name),
                owner: None,
                name,
                dispatch: ReferenceDispatch::Static,
                kind: ReferenceKind::TypeOf,
                span: declaration.span,
            };
            if let Some(id) = return_id(
                target.index,
                (&declaration, &request, target.budget),
                cancelled,
            )? {
                set_return_class(&mut target.index.receivers, (&declaration, id));
            }
        }
    }
    Ok(())
}

fn declared_name(marker: &str) -> &str {
    marker
        .strip_prefix("type::")
        .or_else(|| marker.strip_prefix(super::EXPLICIT_RECEIVER_IMPORT_PREFIX))
        .unwrap_or(marker)
}

fn return_id<Cancel>(
    index: &ResolutionIndex,
    input: (
        &ReturnDeclaration,
        &ResolutionRequest<'_>,
        &mut super::ResolveBudget,
    ),
    cancelled: &mut Cancel,
) -> Result<Option<SymbolId>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (declaration, request, budget) = input;
    let Some(class) = declared_class(index, (&declaration.name, request), cancelled)? else {
        return Ok(None);
    };
    budget.charge(usize_to_u64(class.symbol_id.as_str().len()))?;
    Ok(Some(class.symbol_id.clone()))
}

fn set_return_class(receivers: &mut super::ReceiverIndex, input: (&ReturnDeclaration, SymbolId)) {
    let (declaration, id) = input;
    if let Some(IndexedMember::Unique(member)) = receivers
        .classes
        .get_mut(declaration.class_id.as_str())
        .and_then(|class| class.members.get_mut(&declaration.member))
        && member.symbol_id == declaration.method_id
    {
        member.return_class = Some(id);
    }
}

fn declared_class<'index, Cancel>(
    index: &'index ResolutionIndex,
    input: (&str, &ResolutionRequest<'_>),
    cancelled: &mut Cancel,
) -> Result<Option<&'index Class>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (marker, request) = input;
    if let Some(id) = marker.strip_prefix('@') {
        return Ok(super::local_type(index, (id, request.file_id)));
    }
    if marker.starts_with(super::EXPLICIT_RECEIVER_IMPORT_PREFIX) {
        return super::imported_type(index, request, cancelled);
    }
    if marker.starts_with("type::") {
        return resolve_type(index, request, cancelled);
    }
    Ok(None)
}

pub(super) fn returned_class<'index, Cancel>(
    index: &'index ResolutionIndex,
    input: (&'index Class, std::str::Split<'_, char>, bool),
    cancelled: &mut Cancel,
) -> Result<Option<(&'index Class, bool)>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (mut class, members, static_required) = input;
    let mut count = 0;
    for member in members {
        if cancelled() {
            return Err(StageItemFailure);
        }
        count += 1;
        if count > MAX_CHAIN_CALLS {
            return Ok(None);
        }
        let Some(next) = next_class(index, (class, member, static_required && count == 1)) else {
            return Ok(None);
        };
        class = next;
    }
    Ok(Some((class, count > 0)))
}

fn next_class<'index>(
    index: &'index ResolutionIndex,
    input: (&'index Class, &str, bool),
) -> Option<&'index Class> {
    let (class, member, static_required) = input;
    if class.fenced
        || class.non_methods.contains(member)
        || class.assigned_members.contains(member)
        || index.receivers.unproven_assignments.contains(member)
    {
        return None;
    }
    let IndexedMember::Unique(definition) = class.members.get(member)? else {
        return None;
    };
    if definition.instance_allowed == static_required {
        return None;
    }
    definition
        .return_class
        .as_ref()
        .and_then(|id| index.receivers.classes.get(id.as_str()))
}

pub(super) fn target(candidate: &MemberDefinition, chained: bool) -> ResolvedTarget {
    if !chained {
        return super::member_target(candidate, false);
    }
    ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: MINIMUM_TYPE_CONFIDENCE,
        provenance: RETURN_PROVENANCE,
    }
}

pub(super) fn permits_constructor(
    index: &ResolutionIndex,
    input: (&super::FileResolutionContext<'_>, bool, &Class),
) -> bool {
    let (context, constructed, class) = input;
    if !constructed {
        return true;
    }
    let language = context.identity.language.as_str();
    if class.non_methods.contains(UNSUPPORTED_CONSTRUCTOR)
        || language == "kotlin" && class.file_id != context.identity.file_id
    {
        return false;
    }
    let member = match language {
        "ruby" => "new",
        "kotlin" => "invoke",
        _ => return true,
    };
    !(class.members.contains_key(member)
        || class.non_methods.contains(member)
        || class.assigned_members.contains(member)
        || index.receivers.unproven_assignments.contains(member))
}

pub(super) struct ReceiverPath<'lookup> {
    pub(super) receiver: &'lookup str,
    pub(super) member: &'lookup str,
    pub(super) members: std::str::Split<'lookup, char>,
    pub(super) constructed: bool,
    pub(super) static_required: bool,
}

pub(super) fn receiver_path<'lookup>(
    payload: &'lookup str,
    language: &str,
) -> Option<ReceiverPath<'lookup>> {
    let (receiver, member) = payload.split_once('#')?;
    if language == "python" && member.starts_with("__") && !member.ends_with("__") {
        return None;
    }
    let mut members = receiver.split('|');
    let receiver = members.next().unwrap_or(receiver);
    let constructed = receiver.starts_with(CONSTRUCTOR_PREFIX);
    let receiver = receiver
        .strip_prefix(CONSTRUCTOR_PREFIX)
        .unwrap_or(receiver);
    let static_required = receiver.starts_with("static::");
    let receiver = receiver.strip_prefix("static::").unwrap_or(receiver);
    Some(ReceiverPath {
        receiver,
        member,
        members,
        constructed,
        static_required,
    })
}

pub(super) fn receiver_class<'index, Cancel>(
    index: &'index ResolutionIndex,
    input: (
        &super::FileResolutionContext<'_>,
        &super::ExtractedReference,
        &mut super::ImportBindingScratch,
        &str,
    ),
    cancelled: &mut Cancel,
) -> Result<Option<&'index Class>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (context, reference, scratch, marker) = input;
    if let Some(id) = marker.strip_prefix('@') {
        return Ok(super::local_type(index, (id, &context.identity.file_id)));
    }
    let imported = marker.strip_prefix(super::EXPLICIT_RECEIVER_IMPORT_PREFIX);
    let Some(name) = imported.or_else(|| marker.strip_prefix("type::")) else {
        return Ok(None);
    };
    let request = super::type_request((context, reference, scratch, name));
    if imported.is_some() {
        super::imported_type(index, &request, cancelled)
    } else {
        resolve_type(index, &request, cancelled)
    }
}
