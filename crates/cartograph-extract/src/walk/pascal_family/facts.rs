//! Stateless Pascal fact emitters: imports, heritage, type references,
//! signatures, names, and routine-local scopes.

use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::{
    ReferenceKind, SourceSpan, SymbolId, SymbolKind, Visibility, callable_signature_is_literal_free,
};
use tree_sitter::Node;

use super::names::{
    extras_free_text, identifier_text, is_builtin_type, name_parts, type_base_parts,
};
use crate::{
    ExtractError, ExtractedImportBinding, ExtractedReference, ImportBindingKind, SymbolExportFlags,
    walk::{
        ExtractionBuilder, PendingSymbol,
        syntax::{descendants_including_root, has_child_kind, named_children, span_for},
        with_root_scope,
    },
};

/// Longest retained callable signature.
const MAXIMUM_SIGNATURE_BYTES: usize = 512;
/// Syntax nodes inspected for literal tokens before a value is rejected.
const MAXIMUM_LITERAL_SCAN_NODES: usize = 4_096;
/// Bound on the nesting of anonymous type constructors scanned for type names.
const MAXIMUM_TYPE_NESTING: usize = 8;

/// Type names referenced from one declaration. `generics` holds the
/// generic parameters in scope, which name no project type.
#[derive(Clone, Copy)]
pub(super) struct TypeReferences<'tree, 'owner> {
    pub(super) root: Node<'tree>,
    pub(super) owner: &'owner SymbolId,
    pub(super) kind: ReferenceKind,
    pub(super) generics: &'owner GenericScope,
}

/// The generic parameters of the enclosing generic types and routines, as a
/// stack (entered and left with [`GenericScope::mark`] and
/// [`GenericScope::truncate`]) with an indexed, case-insensitive membership
/// test, so neither entering a declaration nor a lookup copies or scans it.
#[derive(Default)]
pub(super) struct GenericScope {
    stack: Vec<String>,
    names: BTreeMap<String, usize>,
}

impl GenericScope {
    /// The current depth, to return to with [`GenericScope::truncate`].
    pub(super) const fn mark(&self) -> usize {
        self.stack.len()
    }

    /// Bring lower-case parameter names into scope.
    pub(super) fn extend(&mut self, parameters: Vec<String>) {
        for parameter in parameters {
            let count = self.names.entry(parameter.clone()).or_default();
            *count = count.saturating_add(1);
            self.stack.push(parameter);
        }
    }

    /// Leave every parameter brought into scope after `mark`.
    pub(super) fn truncate(&mut self, mark: usize) {
        while self.stack.len() > mark {
            let Some(parameter) = self.stack.pop() else {
                return;
            };
            if let Some(count) = self.names.get_mut(&parameter) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.names.remove(&parameter);
                }
            }
        }
    }

    /// Whether a type name is a generic parameter in scope.
    fn contains(&self, name: &str) -> bool {
        !self.names.is_empty() && self.names.contains_key(&name.to_ascii_lowercase())
    }
}

/// One named type inside a declared type, collected once so a declaration
/// naming many symbols (`A, B, C: T`) scans its type a single time.
pub(super) struct TypeName {
    name: String,
    span: SourceSpan,
}

/// A symbol with every optional fact cleared.
pub(super) fn pending(kind: SymbolKind, name: String, node: Node<'_>) -> PendingSymbol<'_> {
    PendingSymbol {
        kind,
        name,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: SymbolExportFlags::default(),
        async_symbol: false,
        static_member: false,
        visibility: None,
    }
}

/// `uses A, B.C;` emits one import symbol, one `Imports` reference, and one
/// namespace binding whose local name is the unit itself.
pub(super) fn emit_uses(
    builder: &mut ExtractionBuilder<'_, '_>,
    clause: Node<'_>,
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    for unit in named_children(clause).filter(|child| child.kind() == "moduleName") {
        let Some(parts) = name_parts(source, unit) else {
            continue;
        };
        let name = builder.context.copy_text(&parts.join("."))?;
        let symbol_name = name.clone();
        with_root_scope(builder, |builder| {
            builder.emit_symbol(pending(SymbolKind::Import, symbol_name, unit))
        })?;
        let span = span_for(unit)?;
        builder.emit_import_binding(ExtractedImportBinding {
            kind: ImportBindingKind::Namespace,
            module_specifier: name.clone(),
            imported_name: "*".to_owned(),
            local_name: name.clone(),
            span,
        })?;
        builder.emit_reference(ExtractedReference {
            owner: None,
            name,
            resolution_name: None,
            kind: ReferenceKind::Imports,
            span,
        })?;
    }
    Ok(())
}

/// Parent types of a class or interface. A class's first parent is its
/// ancestor and the rest are implemented interfaces; every parent of an
/// interface is an ancestor interface.
pub(super) fn emit_heritage(
    builder: &mut ExtractionBuilder<'_, '_>,
    body: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    let interface = body.kind() == "declIntf";
    let mut cursor = body.walk();
    let parents = body
        .children_by_field_name("parent", &mut cursor)
        .filter(|parent| parent.kind() == "typeref")
        .collect::<Vec<_>>();
    for (position, parent) in parents.into_iter().enumerate() {
        let Some(parts) = type_base_parts(source, parent) else {
            continue;
        };
        let kind = if interface || position == 0 {
            ReferenceKind::Extends
        } else {
            ReferenceKind::Implements
        };
        builder.emit_reference(ExtractedReference {
            owner: Some(owner.clone()),
            name: builder.context.copy_text(&parts.join("."))?,
            resolution_name: None,
            kind,
            span: span_for(parent)?,
        })?;
    }
    Ok(())
}

/// Named types inside a declared type (`TFoo`, `array of TFoo`, `^TFoo`,
/// `class of TFoo`, `set of TFoo`), skipping scalar builtins and generic
/// parameters in scope.
pub(super) fn emit_type_references(
    builder: &mut ExtractionBuilder<'_, '_>,
    references: TypeReferences<'_, '_>,
) -> Result<(), ExtractError> {
    let names = collect_type_names(builder, references.root, references.generics)?;
    emit_type_names(
        builder,
        &names,
        TypeNameOwner {
            owner: references.owner,
            kind: references.kind,
        },
    )
}

/// The owner and kind of the references emitted for collected type names.
#[derive(Clone, Copy)]
pub(super) struct TypeNameOwner<'owner> {
    pub(super) owner: &'owner SymbolId,
    pub(super) kind: ReferenceKind,
}

/// Collect the named types under `root` once, charging each retained name to
/// the extraction's working-memory budget.
pub(super) fn collect_type_names(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
    generics: &GenericScope,
) -> Result<Vec<TypeName>, ExtractError> {
    let source = builder.context.snapshot.source();
    let mut names = Vec::new();
    let mut stack = vec![(root, 0_usize)];
    while let Some((node, depth)) = stack.pop() {
        builder.context.ensure_active()?;
        match node.kind() {
            "typeref" => {
                if let Some(name) = type_name(source, node, generics) {
                    builder.context.budget.reserve_additional_string(&name)?;
                    names.push(TypeName {
                        name,
                        span: span_for(node)?,
                    });
                }
            }
            "type" | "declArray" | "declSet" | "declMetaClass" | "declFile"
                if depth < MAXIMUM_TYPE_NESTING =>
            {
                let children = named_children(node).collect::<Vec<_>>();
                stack.extend(
                    children
                        .into_iter()
                        .rev()
                        .map(|child| (child, depth.saturating_add(1))),
                );
            }
            _ => {}
        }
    }
    Ok(names)
}

/// Emit one reference per collected type name for `owner`.
pub(super) fn emit_type_names(
    builder: &mut ExtractionBuilder<'_, '_>,
    names: &[TypeName],
    owner: TypeNameOwner<'_>,
) -> Result<(), ExtractError> {
    for type_name in names {
        builder.emit_reference(ExtractedReference {
            owner: Some(owner.owner.clone()),
            name: builder.context.copy_text(&type_name.name)?,
            resolution_name: None,
            kind: owner.kind,
            span: type_name.span,
        })?;
    }
    Ok(())
}

/// The dotted name of one `typeref`, unless it is a scalar builtin or a
/// generic parameter in scope.
fn type_name(source: &str, typeref: Node<'_>, generics: &GenericScope) -> Option<String> {
    let parts = type_base_parts(source, typeref)?;
    if let [only] = parts.as_slice()
        && (is_builtin_type(only) || generics.contains(only))
    {
        return None;
    }
    Some(parts.join("."))
}

/// The routine header whose parameter and result types are referenced, and
/// the generic parameters in scope for it.
#[derive(Clone, Copy)]
pub(super) struct RoutineTypes<'tree, 'owner> {
    pub(super) routine: Node<'tree>,
    pub(super) owner: &'owner SymbolId,
    pub(super) generics: &'owner GenericScope,
}

/// Parameter types (`TypeOf`) and the result type (`Returns`) of a routine
/// declaration or implementation header.
pub(super) fn emit_routine_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    types: RoutineTypes<'_, '_>,
) -> Result<(), ExtractError> {
    let routine = types.routine;
    if let Some(arguments) = routine.child_by_field_name("args") {
        for argument in named_children(arguments).filter(|child| child.kind() == "declArg") {
            if let Some(declared_type) = argument.child_by_field_name("type") {
                emit_type_references(
                    builder,
                    TypeReferences {
                        root: declared_type,
                        owner: types.owner,
                        kind: ReferenceKind::TypeOf,
                        generics: types.generics,
                    },
                )?;
            }
        }
    }
    if let Some(result) = routine.child_by_field_name("type") {
        emit_type_references(
            builder,
            TypeReferences {
                root: result,
                owner: types.owner,
                kind: ReferenceKind::Returns,
                generics: types.generics,
            },
        )?;
    }
    Ok(())
}

/// `(params): Result`, retained only when it is bounded and literal-free
/// (default parameter values are literals and drop the whole signature).
pub(super) fn routine_signature(
    builder: &ExtractionBuilder<'_, '_>,
    routine: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let arguments = routine.child_by_field_name("args");
    let result = routine.child_by_field_name("type");
    if arguments.is_none() && result.is_none()
        || arguments.into_iter().chain(result).any(contains_literal)
    {
        return Ok(None);
    }
    let source = builder.context.snapshot.source();
    let mut signature = String::new();
    if let Some(arguments) = arguments {
        push_collapsed(&mut signature, &extras_free_text(source, arguments));
    }
    if let Some(result) = result {
        signature.push_str(": ");
        push_collapsed(&mut signature, &extras_free_text(source, result));
    }
    if signature.len() > MAXIMUM_SIGNATURE_BYTES || !callable_signature_is_literal_free(&signature)
    {
        return Ok(None);
    }
    builder.context.copy_text(&signature).map(Some)
}

/// Append `text` with every whitespace run collapsed to one space, stopping
/// once the signature bound is exceeded.
fn push_collapsed(signature: &mut String, text: &str) {
    for (position, word) in text.split_whitespace().enumerate() {
        if signature.len() > MAXIMUM_SIGNATURE_BYTES {
            return;
        }
        if position > 0 {
            signature.push(' ');
        }
        signature.push_str(word);
    }
}

/// The declared name of a type or routine: the last component of a dotted
/// or generic name.
pub(super) fn declared_name(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> Option<String> {
    let source = builder.context.snapshot.source();
    name_parts(source, node)
        .and_then(|parts| parts.last().copied())
        .map(str::to_owned)
}

/// An identifier's text when it is a plain Pascal identifier.
pub(super) fn owned_identifier(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let source = builder.context.snapshot.source();
    identifier_text(source, node)
        .map(|text| builder.context.copy_text(text))
        .transpose()
}

/// The unit/program/library name, or the file stem for `program;`.
pub(super) fn module_name(
    builder: &ExtractionBuilder<'_, '_>,
    module: Node<'_>,
) -> Result<String, ExtractError> {
    let source = builder.context.snapshot.source();
    let declared = named_children(module)
        .find(|child| child.kind() == "moduleName")
        .and_then(|name| name_parts(source, name))
        .map(|parts| parts.join("."));
    if let Some(name) = declared {
        return builder.context.copy_text(&name);
    }
    let path = builder.context.snapshot.path().as_str();
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    builder.context.copy_text(stem)
}

/// The initializer expression after `=` in a constant or variable, when it
/// holds no literal token. Pascal literals such as `$FF` pass textual
/// literal checks, so the syntax tree decides.
pub(super) fn binding_value(binding: Node<'_>) -> Option<Node<'_>> {
    if named_children(binding).any(|child| child.kind() == "ERROR") {
        // Error recovery can attach a later declaration's value to this name.
        return None;
    }
    let default_value = binding.child_by_field_name("defaultValue")?;
    named_children(default_value)
        .find(|child| child.kind() != "kEq")
        .filter(|value| !contains_literal(*value) && !contains_extras(*value))
}

/// Whether a subtree holds a comment or compiler directive, whose text must
/// never be copied into a retained signature.
fn contains_extras(node: Node<'_>) -> bool {
    descendants_including_root(node).any(|child| matches!(child.kind(), "comment" | "pp"))
}

/// Whether a subtree holds a literal token (numbers, strings, characters,
/// `True`/`False`/`nil`). Oversized subtrees count as literal-bearing.
fn contains_literal(node: Node<'_>) -> bool {
    descendants_including_root(node)
        .take(MAXIMUM_LITERAL_SCAN_NODES.saturating_add(1))
        .enumerate()
        .any(|(position, child)| {
            position >= MAXIMUM_LITERAL_SCAN_NODES
                || matches!(
                    child.kind(),
                    "literalNumber" | "literalString" | "literalChar" | "kTrue" | "kFalse" | "kNil"
                )
        })
}

/// Member visibility of a class section; `published` members are public.
pub(super) fn section_visibility(section: Node<'_>) -> Option<Visibility> {
    named_children(section).find_map(|child| match child.kind() {
        "kPublic" | "kPublished" => Some(Visibility::Public),
        "kProtected" => Some(Visibility::Protected),
        "kPrivate" => Some(Visibility::Private),
        _ => None,
    })
}

/// Lower-case names of the values one routine declares itself: parameters,
/// local variables and constants, and local types. An abbreviated
/// implementation header (`procedure TFoo.Run;`) takes its parameters from
/// the paired `declaration`. Each name is charged to the extraction budget;
/// nested routines are scoped separately as they are emitted.
pub(super) fn routine_locals(
    builder: &mut ExtractionBuilder<'_, '_>,
    implementation: Node<'_>,
    declaration: Option<Node<'_>>,
) -> Result<BTreeSet<String>, ExtractError> {
    let source = builder.context.snapshot.source();
    let mut declarations = Vec::new();
    let header = implementation.child_by_field_name("header");
    let arguments = header
        .and_then(|header| header.child_by_field_name("args"))
        .or_else(|| {
            header
                .filter(|header| header.child_by_field_name("type").is_none())
                .and(declaration)
                .and_then(|declaration| declaration.child_by_field_name("args"))
        });
    if let Some(arguments) = arguments {
        declarations.extend(named_children(arguments).filter(|child| child.kind() == "declArg"));
    }
    let mut cursor = implementation.walk();
    for local in implementation.children_by_field_name("local", &mut cursor) {
        if matches!(local.kind(), "declVars" | "declConsts" | "declTypes") {
            declarations.extend(named_children(local));
        }
    }
    let mut names = BTreeSet::new();
    for declaration in declarations {
        for name in declared_field_names(source, declaration) {
            builder.context.budget.reserve_additional_string(&name)?;
            names.insert(name);
        }
    }
    Ok(names)
}

/// Lower-case names of the routines one routine declares locally that no
/// name alone can pick: a name marked `overload`, or declared by more than
/// one `forward` declaration or more than one body (whatever its spelling).
/// Each name is charged as it is first admitted, and the scan polls for
/// cancellation.
pub(super) fn local_overloads(
    builder: &mut ExtractionBuilder<'_, '_>,
    implementation: Node<'_>,
) -> Result<BTreeSet<String>, ExtractError> {
    let source = builder.context.snapshot.source();
    let mut forward = BTreeMap::<String, usize>::new();
    let mut bodies = BTreeMap::<String, usize>::new();
    let mut overloaded = BTreeSet::new();
    let mut cursor = implementation.walk();
    for local in implementation.children_by_field_name("local", &mut cursor) {
        builder.context.ensure_active()?;
        let Some((counts, declaration)) = local_overload_counts(local, &mut forward, &mut bodies)
        else {
            continue;
        };
        let marked = is_marked_overload(declaration);
        for name in declared_field_names(source, declaration) {
            if !counts.contains_key(&name) {
                builder.context.budget.reserve_additional_string(&name)?;
            }
            let count = counts.entry(name.clone()).or_default();
            *count = count.saturating_add(1);
            if (marked || *count > 1) && !overloaded.contains(&name) {
                builder.context.budget.reserve_additional_string(&name)?;
                overloaded.insert(name);
            }
        }
    }
    Ok(overloaded)
}

/// Select the forward/body count map and declaration header of a local routine.
fn local_overload_counts<'tree, 'counts>(
    local: Node<'tree>,
    forward: &'counts mut BTreeMap<String, usize>,
    bodies: &'counts mut BTreeMap<String, usize>,
) -> Option<(&'counts mut BTreeMap<String, usize>, Node<'tree>)> {
    match local.kind() {
        "declProc" => Some((forward, local)),
        "defProc" => local
            .child_by_field_name("header")
            .map(|header| (bodies, header)),
        _ => None,
    }
}

/// Whether a routine header carries the `overload` directive.
fn is_marked_overload(declaration: Node<'_>) -> bool {
    let mut cursor = declaration.walk();
    declaration
        .children_by_field_name("attribute", &mut cursor)
        .any(|attribute| has_child_kind(attribute, "kOverload"))
}

/// The lower-case name a local `forward` (or external) routine declaration
/// introduces.
pub(super) fn local_declaration_name(
    builder: &ExtractionBuilder<'_, '_>,
    declaration: Node<'_>,
) -> Option<String> {
    declared_field_names(builder.context.snapshot.source(), declaration)
        .into_iter()
        .next()
}

/// Lower-case last components of every `name` field of a declaration.
fn declared_field_names(source: &str, declaration: Node<'_>) -> Vec<String> {
    let mut cursor = declaration.walk();
    declaration
        .children_by_field_name("name", &mut cursor)
        .filter_map(|name| name_parts(source, name))
        .filter_map(|parts| parts.last().map(|last| last.to_ascii_lowercase()))
        .collect()
}
