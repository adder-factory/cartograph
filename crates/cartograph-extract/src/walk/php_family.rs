//! PHP structural extraction.
//!
//! The family emits PHP declarations with namespace-qualified names
//! (`App\Models::User::find`), one import fact and named binding per `use`
//! clause, file imports only for pure string-literal include targets, and
//! extends/implements/trait-use, call, construction, and type references.
//! Statically named references also carry an exact compile-time lookup (see
//! the `names` module) that the indexer resolves by exact qualified name and
//! symbol space only. Dynamic receivers, variable class names, and members a class
//! does not itself declare stay unresolved rather than guessed.

mod names;

use cartograph_domain::{
    ReferenceKind, SymbolId, SymbolKind, Visibility, callable_signature_is_literal_free,
};
use tree_sitter::Node;

use crate::{
    ExtractError, ExtractedImportBinding, ExtractedReference, ImportBindingKind, SymbolExportFlags,
};

pub(super) use self::names::PhpScope;
use self::names::{
    ClassContext, FactoryCall, ImportKind, KEY_SEPARATOR, MAX_NAME_BYTES, bounded_concat,
    exact_lookup, php_key, php_name, returned_member_lookup, valid_fqn, valid_segment,
};
use super::{
    ExtractionBuilder, PendingSymbol,
    specifier_safety::specifier_may_carry_credential,
    syntax::{descendants, named_children, span_for, unwrap_parentheses},
    with_root_scope,
};

/// Longest retained signature, matching the other structural families.
const MAX_SIGNATURE_BYTES: usize = 512;
/// Longest declaration source span a signature is built from; comments and
/// attributes inside it are removed before the signature bound applies.
const MAX_SIGNATURE_SOURCE_BYTES: usize = 4 * MAX_SIGNATURE_BYTES;
/// Deepest receiver chain rendered into a member-call name.
const MAX_RECEIVER_DEPTH: usize = 4;
/// Deepest nested parentheses unwrapped around an include target.
const MAX_PARENTHESES_DEPTH: usize = 4;
/// Deepest nested union/intersection type walked for type references.
const MAX_TYPE_DEPTH: usize = 16;
/// Most leading comments skipped while looking for a first-class-callable
/// `...` argument; past it the call is treated as possibly first-class.
const MAX_LEADING_ARGUMENT_COMMENTS: usize = 8;
/// Module specifier of a `use` clause that imports from the global namespace.
const ROOT_NAMESPACE: &str = "\\";
/// Placeholder for a receiver expression that cannot be rendered literal-free.
const OPAQUE_RECEIVER: &str = "(...)";

/// File-inclusion expressions whose literal target is a file import.
const INCLUDE_KINDS: [&str; 4] = [
    "include_expression",
    "include_once_expression",
    "require_expression",
    "require_once_expression",
];
/// Grammar nodes that spell a (possibly qualified) PHP name.
const CLASS_NAME_KINDS: [&str; 3] = ["name", "qualified_name", "relative_name"];
/// Type names that never denote a project declaration.
const BUILTIN_TYPE_NAMES: [&str; 21] = [
    "array", "bool", "boolean", "callable", "double", "false", "float", "int", "integer",
    "iterable", "mixed", "never", "null", "object", "parent", "resource", "self", "static",
    "string", "true", "void",
];

/// Emit the declaration facts of one PHP node; returns whether the node was
/// handled, so the walker does not also visit it as usage.
pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "namespace_definition" => visit_namespace(builder, node, depth)?,
        "namespace_use_declaration" => visit_use_declaration(builder, node)?,
        "class_declaration"
        | "interface_declaration"
        | "trait_declaration"
        | "enum_declaration" => visit_class_like(builder, node, depth)?,
        "anonymous_class" => visit_anonymous_class(builder, node, depth)?,
        "method_declaration" | "function_definition" => visit_callable(builder, node, depth)?,
        "property_declaration" => visit_properties(builder, node, depth)?,
        "const_declaration" => visit_constants(builder, node)?,
        "enum_case" => visit_enum_case(builder, node)?,
        "use_declaration" => capture_trait_uses(builder, node)?,
        kind if INCLUDE_KINDS.contains(&kind) => {
            capture_include(builder, node)?;
            builder.visit_named_children(node, depth)?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Emit the call and construction references of one PHP expression node.
pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match node.kind() {
        "function_call_expression" => capture_function_call(builder, node),
        "member_call_expression" | "nullsafe_member_call_expression" => {
            capture_member_call(builder, node)
        }
        "scoped_call_expression" => capture_scoped_call(builder, node),
        "object_creation_expression" => capture_instantiation(builder, node),
        _ => Ok(()),
    }
}

/// Enter a `namespace` statement or block; an unbraced namespace stays
/// open until the next one.
fn visit_namespace(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    if node
        .parent()
        .is_none_or(|parent| parent.kind() != "program")
    {
        return builder.visit_named_children(node, depth);
    }
    if builder.php.unbraced_namespace() {
        close_namespace_scope(builder);
    }
    let name_node = node.child_by_field_name("name");
    let name = match name_node {
        Some(name_node) => php_name_text(builder, name_node)?.filter(|name| valid_fqn(name)),
        None => None,
    };
    let body = node.child_by_field_name("body");
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: crate::PHP_NAMESPACE_SCOPE_MODULE.to_owned(),
        imported_name: "namespace".to_owned(),
        local_name: "namespace".to_owned(),
        span: span_for(node)?,
    })?;
    builder.php.enter_namespace(name.clone(), body.is_none());
    if name_node.is_some() && name.is_none() {
        builder.php.mark_aliases_incomplete();
    }
    if let Some(name) = name {
        let id = builder.emit_symbol(PendingSymbol::namespace(node, name.clone()))?;
        push_owner(
            builder,
            OwnerScope {
                id,
                kind: SymbolKind::Namespace,
                name,
            },
        );
    }
    let Some(body) = body else {
        return Ok(());
    };
    let result = builder.visit(body, depth.saturating_add(1));
    close_namespace_scope(builder);
    result
}

/// Leave the current namespace block and drop its `use` aliases.
fn close_namespace_scope(builder: &mut ExtractionBuilder<'_, '_>) {
    if builder.php.namespace().is_some()
        && builder.native_owner_kinds.last() == Some(&SymbolKind::Namespace)
    {
        pop_owner(builder);
    }
    builder.php.enter_namespace(None, false);
}

/// One declaration that becomes the owner and qualifier of nested facts.
struct OwnerScope {
    id: SymbolId,
    kind: SymbolKind,
    name: String,
}

/// Make a declaration the owner and qualifier of the facts nested in it.
fn push_owner(builder: &mut ExtractionBuilder<'_, '_>, scope: OwnerScope) {
    builder.owners.push(scope.id);
    builder.native_owner_kinds.push(scope.kind);
    builder.qualifiers.push(scope.name);
}

/// Run `visit` with only the namespace qualifier when the declaration sits
/// inside a function, method, or property-hook body: PHP functions and
/// classes declared there are namespace-level declarations that the body
/// merely contains.
fn with_declaration_qualifiers<Output>(
    builder: &mut ExtractionBuilder<'_, '_>,
    visit: impl FnOnce(&mut ExtractionBuilder<'_, '_>) -> Output,
) -> Output {
    let nested = builder.native_owner_kinds.iter().any(|kind| {
        matches!(
            kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Field
        )
    });
    if !nested {
        return visit(builder);
    }
    let namespace = builder.php.namespace().map(str::to_owned);
    let saved = std::mem::replace(&mut builder.qualifiers, namespace.into_iter().collect());
    let output = visit(builder);
    builder.qualifiers = saved;
    output
}

/// Undo the most recent `push_owner`.
fn pop_owner(builder: &mut ExtractionBuilder<'_, '_>) {
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
}

/// Emit every clause of one `use` statement, expanding a group prefix.
fn visit_use_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    let prefix = named_children(node)
        .find(|child| child.kind() == "namespace_name")
        .map(|child| node_text(source, child).trim());
    let statement = UseStatement {
        prefix,
        kind: import_kind(node).unwrap_or(ImportKind::Class),
        signature: import_signature(source, node),
    };
    let clauses = node.child_by_field_name("body").unwrap_or(node);
    for clause in named_children(clauses).filter(|child| child.kind() == "namespace_use_clause") {
        builder.context.ensure_active()?;
        visit_use_clause(builder, clause, &statement)?;
    }
    Ok(())
}

/// Facts shared by every clause of one `use` statement.
struct UseStatement<'source> {
    prefix: Option<&'source str>,
    kind: ImportKind,
    signature: Option<String>,
}

/// The symbol space a `use` statement or clause names with `function` or
/// `const`; a clause-level modifier overrides the statement's.
fn import_kind(node: Node<'_>) -> Option<ImportKind> {
    match node.child_by_field_name("type")?.kind() {
        "function" => Some(ImportKind::Function),
        "const" => Some(ImportKind::Constant),
        _ => None,
    }
}

/// The bounded statement text retained as an import signature, without
/// comments and with whitespace collapsed.
fn import_signature(source: &str, node: Node<'_>) -> Option<String> {
    let mut signature = String::new();
    (push_signature_text(&mut signature, source, node) && !signature.is_empty())
        .then_some(signature)
}

/// Emit and register one `use` clause; an import the alias tables cannot
/// represent makes the block abstain from namespace-relative resolution.
fn visit_use_clause(
    builder: &mut ExtractionBuilder<'_, '_>,
    clause: Node<'_>,
    statement: &UseStatement<'_>,
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    let alias = clause.child_by_field_name("alias");
    let Some(name_node) = named_children(clause)
        .find(|child| Some(*child) != alias && matches!(child.kind(), "name" | "qualified_name"))
    else {
        builder.php.mark_aliases_incomplete();
        return Ok(());
    };
    let imported_text = node_text(source, name_node).trim();
    let fqn = match statement.prefix {
        Some(prefix) => bounded_concat(&[prefix, "\\", imported_text], MAX_NAME_BYTES),
        None => bounded_concat(&[imported_text.trim_start_matches('\\')], MAX_NAME_BYTES),
    };
    let Some(fqn) = fqn.filter(|fqn| valid_fqn(fqn)) else {
        builder.php.mark_aliases_incomplete();
        return Ok(());
    };
    let (module, imported) = fqn.rsplit_once('\\').unwrap_or((ROOT_NAMESPACE, &fqn));
    let local = alias.map_or(imported, |alias| node_text(source, alias).trim());
    if !valid_segment(local) {
        builder.php.mark_aliases_incomplete();
        return Ok(());
    }
    let kind = import_kind(clause).unwrap_or(statement.kind);
    emit_use_clause(
        builder,
        UseClause {
            clause,
            name_node,
            names: ImportedNames {
                fqn: &fqn,
                module,
                imported,
                local,
            },
            kind,
            signature: statement.signature.as_deref(),
        },
    )?;
    builder.php.register_alias(kind, local, &fqn);
    Ok(())
}

/// The names one `use` clause imports and binds.
#[derive(Clone, Copy)]
struct ImportedNames<'text> {
    fqn: &'text str,
    module: &'text str,
    imported: &'text str,
    local: &'text str,
}

/// One expanded `use` clause ready for emission.
#[derive(Clone, Copy)]
struct UseClause<'tree, 'text> {
    clause: Node<'tree>,
    name_node: Node<'tree>,
    names: ImportedNames<'text>,
    kind: ImportKind,
    signature: Option<&'text str>,
}

/// Emit the import symbol, file import, named binding, and declaration
/// reference of one `use` clause.
fn emit_use_clause(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: UseClause<'_, '_>,
) -> Result<(), ExtractError> {
    let names = input.names;
    emit_import_symbol(
        builder,
        ImportSymbol {
            node: input.clause,
            name: names.fqn,
            signature: input.signature,
        },
    )?;
    builder.emit_reference(ExtractedReference {
        owner: None,
        name: builder.context.copy_text(names.fqn)?,
        resolution_name: None,
        kind: ReferenceKind::Imports,
        span: span_for(input.clause)?,
    })?;
    let span = span_for(input.name_node)?;
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Named,
        module_specifier: builder.context.copy_text(names.module)?,
        imported_name: builder.context.copy_text(names.imported)?,
        local_name: builder.context.copy_text(names.local)?,
        span,
    })?;
    let resolution_name =
        php_key(names.fqn).and_then(|key| exact_lookup(input.kind.intent(), &[&key]));
    builder.emit_reference(ExtractedReference {
        owner: None,
        name: builder.context.copy_text(names.imported)?,
        resolution_name,
        kind: ReferenceKind::References,
        span,
    })
}

/// A file-scope import symbol.
#[derive(Clone, Copy)]
struct ImportSymbol<'tree, 'text> {
    node: Node<'tree>,
    name: &'text str,
    signature: Option<&'text str>,
}

/// Emit one import symbol at file scope.
fn emit_import_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ImportSymbol<'_, '_>,
) -> Result<(), ExtractError> {
    let name = builder.context.copy_text(input.name)?;
    let signature = input
        .signature
        .map(|signature| builder.context.copy_text(signature))
        .transpose()?;
    with_root_scope(builder, |builder| {
        builder.emit_symbol(PendingSymbol {
            kind: SymbolKind::Import,
            name,
            span_node: input.node,
            structural_node: input.node,
            doc_anchor: input.node,
            body_node: None,
            declaration_only: false,
            signature,
            export: SymbolExportFlags::default(),
            async_symbol: false,
            static_member: false,
            visibility: None,
        })
    })?;
    Ok(())
}

/// Emit a file import for an include/require whose target is a literal that
/// carries no credential.
fn capture_include(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    let Some(literal) = named_children(node)
        .next()
        .and_then(|target| unwrap_parentheses(target, MAX_PARENTHESES_DEPTH))
    else {
        return Ok(());
    };
    let Some(path) = static_string_literal(source, literal) else {
        return Ok(());
    };
    // A remote include can embed credentials; such a target leaves no fact.
    if specifier_may_carry_credential(path) {
        return Ok(());
    }
    // The include statement is a string literal, so the Import keeps only its
    // path as the name and retains no statement text.
    emit_import_symbol(
        builder,
        ImportSymbol {
            node,
            name: path,
            signature: None,
        },
    )?;
    let span = span_for(literal)?;
    builder.emit_reference(ExtractedReference {
        owner: None,
        name: builder.context.copy_text(path)?,
        resolution_name: None,
        kind: ReferenceKind::Imports,
        span,
    })?;
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::IncludeQuoted,
        module_specifier: builder.context.copy_text(path)?,
        imported_name: "*".to_owned(),
        local_name: "*".to_owned(),
        span,
    })
}

/// The text of a quoted string made only of literal content. Interpolated,
/// escaped, concatenated, empty, and control-character targets are dynamic
/// or ambiguous and are not file imports.
fn static_string_literal<'source>(source: &'source str, node: Node<'_>) -> Option<&'source str> {
    if !matches!(node.kind(), "string" | "encapsed_string") {
        return None;
    }
    let mut parts = named_children(node);
    let content = parts.next()?;
    if content.kind() != "string_content" || parts.next().is_some() {
        return None;
    }
    let value = node_text(source, content);
    (!value.is_empty()
        && value.len() <= MAX_NAME_BYTES
        && !value.bytes().any(|byte| byte.is_ascii_control()))
    .then_some(value)
}

/// Emit a class, interface, trait, or enum and visit its body.
fn visit_class_like(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    with_declaration_qualifiers(builder, |builder| emit_class_like(builder, node, depth))
}

/// Emit one named class-like declaration with its heritage, attributes,
/// and class context.
fn emit_class_like(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name) = node
        .child_by_field_name("name")
        .map(|name| php_identifier_text(builder, name))
        .transpose()?
        .flatten()
    else {
        return builder.visit_named_children(node, depth);
    };
    let kind = class_like_kind(node.kind());
    let body = node.child_by_field_name("body");
    let parent = (kind == SymbolKind::Class)
        .then(|| class_parent_key(builder, node))
        .flatten();
    let id = builder.emit_symbol(PendingSymbol {
        kind,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: body,
        declaration_only: false,
        signature: None,
        export: SymbolExportFlags::named(true),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })?;
    let key = builder
        .facts
        .symbols
        .last()
        .map(|symbol| symbol.qualified_name.clone());
    capture_heritage(builder, node, &id)?;
    capture_attributes(builder, node, &id)?;
    push_owner(builder, OwnerScope { id, kind, name });
    builder.php.push_class(ClassContext {
        key,
        parent,
        trait_body: kind == SymbolKind::Trait,
    });
    let result = body.map_or(Ok(()), |body| builder.visit(body, depth.saturating_add(1)));
    builder.php.pop_class();
    pop_owner(builder);
    result
}

/// The symbol kind of a class-like declaration node.
fn class_like_kind(node_kind: &str) -> SymbolKind {
    match node_kind {
        "interface_declaration" => SymbolKind::Interface,
        "trait_declaration" => SymbolKind::Trait,
        "enum_declaration" => SymbolKind::Enum,
        _ => SymbolKind::Class,
    }
}

/// The candidate key of the class a class declaration extends.
fn class_parent_key(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> Option<String> {
    let source = builder.context.snapshot.source();
    let parent = named_children(node)
        .find(|child| child.kind() == "base_clause")
        .and_then(|clause| named_children(clause).find(is_class_name_node))?;
    let raw = php_name(node_text(source, parent))?;
    php_key(&builder.php.class_fqn(raw)?)
}

/// Visit an anonymous class: its constructor arguments run in the
/// enclosing context, its body in an unnamed class context.
fn visit_anonymous_class(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    if let Some(arguments) = named_children(node).find(|child| child.kind() == "arguments") {
        builder.visit(arguments, depth.saturating_add(1))?;
    }
    let parent = class_parent_key(builder, node);
    builder.php.push_class(ClassContext {
        key: None,
        parent,
        trait_body: false,
    });
    let result = node
        .child_by_field_name("body")
        .map_or(Ok(()), |body| builder.visit(body, depth.saturating_add(1)));
    builder.php.pop_class();
    result
}

/// Emit one `Decorates` reference per attribute on a declaration, resolved
/// like any other class name.
fn capture_attributes(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    let Some(list) = node.child_by_field_name("attributes") else {
        return Ok(());
    };
    for group in named_children(list).filter(|child| child.kind() == "attribute_group") {
        for name in named_children(group)
            .filter(|child| child.kind() == "attribute")
            .filter_map(|attribute| named_children(attribute).find(is_class_name_node))
        {
            emit_class_reference(
                builder,
                ClassReference {
                    owner: Some(owner.clone()),
                    node: name,
                    kind: ReferenceKind::Decorates,
                },
            )?;
        }
    }
    Ok(())
}

/// Emit `extends` and `implements` references for a class-like declaration.
fn capture_heritage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    for clause in named_children(node) {
        let kind = match clause.kind() {
            "base_clause" => ReferenceKind::Extends,
            "class_interface_clause" => ReferenceKind::Implements,
            _ => continue,
        };
        for name in named_children(clause).filter(is_class_name_node) {
            emit_class_reference(
                builder,
                ClassReference {
                    owner: Some(owner.clone()),
                    node: name,
                    kind,
                },
            )?;
        }
    }
    Ok(())
}

/// Emit one `implements` reference per trait a class body uses. A `use`
/// with an adaptation block (`as`, `insteadof`) can add or rename the
/// methods the traits supply, so its references are marked adapted.
fn capture_trait_uses(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if !matches!(
        builder.native_owner_kinds.last(),
        Some(SymbolKind::Class | SymbolKind::Trait | SymbolKind::Enum)
    ) {
        return Ok(());
    }
    let owner = builder.owners.last().cloned();
    let adapted = named_children(node).any(|child| child.kind() == "use_list");
    for name_node in named_children(node).filter(is_class_name_node) {
        let Some(name) = php_name_text(builder, name_node)? else {
            continue;
        };
        let resolution_name = builder.php.trait_use_lookup(&name, adapted);
        builder.emit_reference(ExtractedReference {
            owner: owner.clone(),
            name,
            resolution_name,
            kind: ReferenceKind::Implements,
            span: span_for(name_node)?,
        })?;
    }
    Ok(())
}

/// Whether a node spells a PHP name.
fn is_class_name_node(node: &Node<'_>) -> bool {
    CLASS_NAME_KINDS.contains(&node.kind())
}

/// A reference whose target is a class-like name in source.
struct ClassReference<'tree> {
    owner: Option<SymbolId>,
    node: Node<'tree>,
    kind: ReferenceKind,
}

/// Emit one reference to a class-like name with its exact lookup.
fn emit_class_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ClassReference<'_>,
) -> Result<(), ExtractError> {
    let Some(name) = php_name_text(builder, input.node)? else {
        return Ok(());
    };
    let resolution_name = builder.php.class_lookup(&name, None);
    builder.emit_reference(ExtractedReference {
        owner: input.owner,
        name,
        resolution_name,
        kind: input.kind,
        span: span_for(input.node)?,
    })
}

/// Emit a function or method. A named function is compiled without class
/// scope even when declared inside a method, so `self`, `parent`, and
/// `$this` name no class there, and a function declared in a callable body
/// is a namespace-level declaration.
fn visit_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    if node.kind() != "function_definition" {
        return emit_callable(builder, node, depth);
    }
    builder.php.push_class(ClassContext::OUTSIDE);
    let result =
        with_declaration_qualifiers(builder, |builder| emit_callable(builder, node, depth));
    builder.php.pop_class();
    result
}

/// Emit one named callable with its signature, types, and attributes, then
/// visit its parameters and body.
fn emit_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name) = node
        .child_by_field_name("name")
        .map(|name| php_identifier_text(builder, name))
        .transpose()?
        .flatten()
    else {
        return builder.visit_named_children(node, depth);
    };
    let method = node.kind() == "method_declaration";
    let modifiers = Modifiers::of(builder, node);
    let visibility = method.then(|| modifiers.visibility.unwrap_or(Visibility::Public));
    let body = node.child_by_field_name("body");
    let kind = if method {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    };
    let signature = callable_signature(builder, node)?;
    let id = builder.emit_symbol(PendingSymbol {
        kind,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: body,
        declaration_only: body.is_none(),
        signature,
        export: SymbolExportFlags::named(
            visibility.is_none_or(|value| value == Visibility::Public),
        ),
        async_symbol: false,
        static_member: modifiers.static_member,
        visibility,
    })?;
    capture_callable_types(builder, node, &id)?;
    capture_attributes(builder, node, &id)?;
    push_owner(builder, OwnerScope { id, kind, name });
    let result = ["parameters", "body"]
        .into_iter()
        .filter_map(|field| node.child_by_field_name(field))
        .try_for_each(|child| builder.visit(child, depth.saturating_add(1)));
    pop_owner(builder);
    result
}

/// Declaration modifiers that change visibility or static-ness.
struct Modifiers {
    visibility: Option<Visibility>,
    static_member: bool,
}

impl Modifiers {
    fn of(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> Self {
        let source = builder.context.snapshot.source();
        let mut modifiers = Self {
            visibility: None,
            static_member: false,
        };
        for child in named_children(node) {
            match child.kind() {
                "visibility_modifier" => {
                    modifiers.visibility = visibility_keyword(node_text(source, child));
                }
                "var_modifier" => modifiers.visibility = Some(Visibility::Public),
                "static_modifier" => modifiers.static_member = true,
                _ => {}
            }
        }
        modifiers
    }
}

/// The visibility a modifier keyword declares.
fn visibility_keyword(text: &str) -> Option<Visibility> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("public") {
        Some(Visibility::Public)
    } else if text.eq_ignore_ascii_case("protected") {
        Some(Visibility::Protected)
    } else if text.eq_ignore_ascii_case("private") {
        Some(Visibility::Private)
    } else {
        None
    }
}

/// `(params): return` with whitespace collapsed, retained only when it is
/// bounded and literal-free.
fn callable_signature(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let source = builder.context.snapshot.source();
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return Ok(None);
    };
    let mut signature = String::new();
    if !push_signature_text(&mut signature, source, parameters) {
        return Ok(None);
    }
    if let Some(return_type) = node.child_by_field_name("return_type") {
        signature.push_str(": ");
        if !push_signature_text(&mut signature, source, return_type) {
            return Ok(None);
        }
    }
    let signature = signature.replace("( ", "(").replace(" )", ")");
    if signature.len() > MAX_SIGNATURE_BYTES || !callable_signature_is_literal_free(&signature) {
        return Ok(None);
    }
    builder.context.copy_text(&signature).map(Some)
}

/// Append `node`'s source without comments or attributes, whitespace
/// collapsed. Comments and attribute arguments can carry arbitrary text that
/// is not part of the declared signature.
fn push_signature_text(output: &mut String, source: &str, node: Node<'_>) -> bool {
    if node.end_byte().saturating_sub(node.start_byte()) > MAX_SIGNATURE_SOURCE_BYTES {
        return false;
    }
    let mut code = String::new();
    let mut cursor = node.start_byte();
    for excluded in
        descendants(node).filter(|child| matches!(child.kind(), "comment" | "attribute_list"))
    {
        if excluded.start_byte() < cursor {
            continue;
        }
        code.push_str(
            source
                .get(cursor..excluded.start_byte())
                .unwrap_or_default(),
        );
        code.push(' ');
        cursor = excluded.end_byte();
    }
    code.push_str(source.get(cursor..node.end_byte()).unwrap_or_default());
    push_collapsed(output, &code)
}

/// Append `text` with every whitespace run collapsed to one space, refusing
/// to grow past the signature bound.
fn push_collapsed(output: &mut String, text: &str) -> bool {
    let mut previous_space = false;
    for character in text.trim().chars() {
        let space = character.is_whitespace();
        if space && previous_space {
            continue;
        }
        previous_space = space;
        if output.len().saturating_add(character.len_utf8()) > MAX_SIGNATURE_BYTES {
            return false;
        }
        output.push(if space { ' ' } else { character });
    }
    true
}

/// Emit parameter `type_of` and return-type `returns` references.
fn capture_callable_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    if let Some(parameters) = node.child_by_field_name("parameters") {
        for parameter in named_children(parameters) {
            builder.context.ensure_active()?;
            if let Some(type_node) = parameter.child_by_field_name("type") {
                capture_type(
                    builder,
                    TypeReference::new(type_node, owner, ReferenceKind::TypeOf),
                )?;
            }
        }
    }
    if let Some(return_type) = node.child_by_field_name("return_type") {
        capture_type(
            builder,
            TypeReference::new(return_type, owner, ReferenceKind::Returns),
        )?;
    }
    Ok(())
}

/// One type position whose named classes become references.
#[derive(Clone, Copy)]
struct TypeReference<'tree, 'owner> {
    node: Node<'tree>,
    owner: &'owner SymbolId,
    kind: ReferenceKind,
    depth: usize,
}

impl<'tree, 'owner> TypeReference<'tree, 'owner> {
    const fn new(node: Node<'tree>, owner: &'owner SymbolId, kind: ReferenceKind) -> Self {
        Self {
            node,
            owner,
            kind,
            depth: 0,
        }
    }
}

/// Emit references for every named class in one type position.
fn capture_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeReference<'_, '_>,
) -> Result<(), ExtractError> {
    if input.depth > MAX_TYPE_DEPTH {
        return Ok(());
    }
    if input.node.kind() == "named_type" {
        return capture_named_type(builder, input);
    }
    for child in named_children(input.node) {
        capture_type(
            builder,
            TypeReference {
                node: child,
                depth: input.depth.saturating_add(1),
                ..input
            },
        )?;
    }
    Ok(())
}

/// Emit a reference for one named type unless it is a builtin.
fn capture_named_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeReference<'_, '_>,
) -> Result<(), ExtractError> {
    let Some(name) = named_children(input.node).find(is_class_name_node) else {
        return Ok(());
    };
    let source = builder.context.snapshot.source();
    let text = node_text(source, name).trim();
    if BUILTIN_TYPE_NAMES
        .iter()
        .any(|builtin| text.eq_ignore_ascii_case(builtin))
    {
        return Ok(());
    }
    emit_class_reference(
        builder,
        ClassReference {
            owner: Some(input.owner.clone()),
            node: name,
            kind: input.kind,
        },
    )
}

/// Emit every property of one declaration and visit its hook bodies.
fn visit_properties(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let declaration = PropertyDeclaration {
        node,
        modifiers: Modifiers::of(builder, node),
        type_node: node.child_by_field_name("type"),
    };
    let mut last = None;
    for element in named_children(node).filter(|child| child.kind() == "property_element") {
        if let Some(scope) = emit_property(builder, &declaration, element)? {
            last = Some(scope);
        }
    }
    let hooks = named_children(node).find(|child| child.kind() == "property_hook_list");
    let (Some(scope), Some(hooks)) = (last, hooks) else {
        return Ok(());
    };
    push_owner(builder, scope);
    let result = builder.visit(hooks, depth.saturating_add(1));
    pop_owner(builder);
    result
}

/// The facts shared by every property of one declaration.
struct PropertyDeclaration<'tree> {
    node: Node<'tree>,
    modifiers: Modifiers,
    type_node: Option<Node<'tree>>,
}

/// Emit one property as a field, the kind v1.1.33 gave PHP properties;
/// returns its scope so property hooks can own their calls.
fn emit_property(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: &PropertyDeclaration<'_>,
    element: Node<'_>,
) -> Result<Option<OwnerScope>, ExtractError> {
    let Some(name_node) = element
        .child_by_field_name("name")
        .and_then(|variable| named_children(variable).find(|child| child.kind() == "name"))
    else {
        return Ok(None);
    };
    let Some(name) = php_identifier_text(builder, name_node)? else {
        return Ok(None);
    };
    let visibility = declaration
        .modifiers
        .visibility
        .unwrap_or(Visibility::Public);
    let signature = property_signature(builder, declaration.type_node, &name)?;
    let id = builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Field,
        name: name.clone(),
        span_node: element,
        structural_node: element,
        doc_anchor: declaration.node,
        body_node: None,
        declaration_only: false,
        signature,
        export: SymbolExportFlags::named(visibility == Visibility::Public),
        async_symbol: false,
        static_member: declaration.modifiers.static_member,
        visibility: Some(visibility),
    })?;
    if let Some(type_node) = declaration.type_node {
        capture_type(
            builder,
            TypeReference::new(type_node, &id, ReferenceKind::TypeOf),
        )?;
    }
    Ok(Some(OwnerScope {
        id,
        kind: SymbolKind::Field,
        name,
    }))
}

/// `Type $name`, the v1 property signature, without any default value.
fn property_signature(
    builder: &ExtractionBuilder<'_, '_>,
    type_node: Option<Node<'_>>,
    name: &str,
) -> Result<Option<String>, ExtractError> {
    let source = builder.context.snapshot.source();
    let mut signature = String::new();
    if let Some(type_node) = type_node {
        if !push_signature_text(&mut signature, source, type_node) {
            return Ok(None);
        }
        signature.push(' ');
    }
    signature.push('$');
    signature.push_str(name);
    if signature.len() > MAX_SIGNATURE_BYTES || !callable_signature_is_literal_free(&signature) {
        return Ok(None);
    }
    builder.context.copy_text(&signature).map(Some)
}

/// Emit one constant per element of a `const` declaration.
fn visit_constants(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let class_member = matches!(
        builder.native_owner_kinds.last(),
        Some(SymbolKind::Class | SymbolKind::Interface | SymbolKind::Trait | SymbolKind::Enum)
    );
    let modifiers = Modifiers::of(builder, node);
    let visibility = class_member.then(|| modifiers.visibility.unwrap_or(Visibility::Public));
    for element in named_children(node).filter(|child| child.kind() == "const_element") {
        let Some(name_node) = named_children(element).find(|child| child.kind() == "name") else {
            continue;
        };
        let Some(name) = php_identifier_text(builder, name_node)? else {
            continue;
        };
        builder.emit_symbol(PendingSymbol {
            kind: SymbolKind::Constant,
            name,
            span_node: element,
            structural_node: element,
            doc_anchor: node,
            body_node: None,
            declaration_only: false,
            signature: None,
            export: SymbolExportFlags::named(
                visibility.is_none_or(|value| value == Visibility::Public),
            ),
            async_symbol: false,
            static_member: false,
            visibility,
        })?;
    }
    Ok(())
}

/// Emit one enum case.
fn visit_enum_case(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name) = node
        .child_by_field_name("name")
        .map(|name| php_identifier_text(builder, name))
        .transpose()?
        .flatten()
    else {
        return Ok(());
    };
    builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::EnumMember,
        name,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: SymbolExportFlags::named(true),
        async_symbol: false,
        static_member: false,
        visibility: Some(Visibility::Public),
    })?;
    Ok(())
}

/// Emit a call to a statically named function.
fn capture_function_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(function) = node
        .child_by_field_name("function")
        .filter(is_class_name_node)
    else {
        return Ok(());
    };
    let Some(name) = php_name_text(builder, function)? else {
        return Ok(());
    };
    let resolution_name = builder.php.function_lookup(&name);
    builder.emit_reference(ExtractedReference {
        owner: builder.owners.last().cloned(),
        name,
        resolution_name,
        kind: ReferenceKind::Calls,
        span: span_for(function)?,
    })
}

/// Emit an instance (`->` or `?->`) method call.
fn capture_member_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = node
        .child_by_field_name("name")
        .filter(|name| name.kind() == "name")
    else {
        return Ok(());
    };
    let Some(method) = php_identifier_text(builder, name_node)? else {
        return Ok(());
    };
    let source = builder.context.snapshot.source();
    let object = node.child_by_field_name("object");
    let receiver = object
        .and_then(|object| render_receiver(source, object, 0))
        .unwrap_or_else(|| OPAQUE_RECEIVER.to_owned());
    let operator = if node.kind() == "nullsafe_member_call_expression" {
        "?->"
    } else {
        "->"
    };
    let Some(name) = bounded_concat(&[&receiver, operator, &method], MAX_NAME_BYTES) else {
        return Ok(());
    };
    let resolution_name = if receiver == "$this" {
        builder.php.this_member_lookup(&method)
    } else {
        object
            .and_then(|object| factory_call(builder, object))
            .and_then(|factory| returned_member_lookup(&factory, &method))
    };
    builder.emit_reference(ExtractedReference {
        owner: builder.owners.last().cloned(),
        name,
        resolution_name,
        kind: ReferenceKind::Calls,
        span: span_for(name_node)?,
    })
}

/// The factory method a receiver calls, when the receiver is itself a
/// statically named call: `Class::factory()`, `self::`/`static::`/
/// `parent::factory()`, or `$this->factory()`. Any other receiver, including
/// a longer chain, names no factory, and neither does a first-class callable
/// (`Class::factory(...)`, whose `...` is its only argument, comments aside), which evaluates
/// to a `Closure` rather than to the factory's return value.
fn factory_call(builder: &ExtractionBuilder<'_, '_>, call: Node<'_>) -> Option<FactoryCall> {
    let source = builder.context.snapshot.source();
    if call
        .child_by_field_name("arguments")
        .is_none_or(maybe_first_class_callable)
    {
        return None;
    }
    let factory = call
        .child_by_field_name("name")
        .filter(|name| name.kind() == "name")
        .map(|name| node_text(source, name).trim())
        .filter(|name| valid_segment(name))?;
    match call.kind() {
        "scoped_call_expression" => {
            let scope = call
                .child_by_field_name("scope")
                .filter(|scope| scope.kind() == "relative_scope" || is_class_name_node(scope))?;
            builder
                .php
                .static_factory(php_name(node_text(source, scope))?, factory)
        }
        "member_call_expression" | "nullsafe_member_call_expression" => {
            let object = call.child_by_field_name("object")?;
            (object.kind() == "variable_name" && node_text(source, object).trim() == "$this")
                .then(|| builder.php.this_factory(factory))
                .flatten()
        }
        _ => None,
    }
}

/// Whether a call's arguments are, or within the comment bound may be, the
/// sole `...` of a first-class callable.
fn maybe_first_class_callable(arguments: Node<'_>) -> bool {
    for (index, argument) in named_children(arguments).enumerate() {
        if argument.kind() != "comment" {
            return argument.kind() == "variadic_placeholder";
        }
        if index >= MAX_LEADING_ARGUMENT_COMMENTS {
            return true;
        }
    }
    false
}

/// Render a call receiver from names only. Literal-bearing or otherwise
/// unrenderable sub-expressions make the whole receiver unrenderable, and
/// the caller substitutes `OPAQUE_RECEIVER`.
fn render_receiver(source: &str, node: Node<'_>, depth: usize) -> Option<String> {
    if depth > MAX_RECEIVER_DEPTH {
        return None;
    }
    match node.kind() {
        "variable_name" | "name" | "qualified_name" | "relative_name" | "relative_scope" => {
            bounded_concat(&[php_name(node_text(source, node))?], MAX_NAME_BYTES)
        }
        "member_access_expression" | "nullsafe_member_access_expression" => {
            render_member(source, node, depth)
        }
        "function_call_expression" => {
            let function =
                render_receiver(source, node.child_by_field_name("function")?, depth + 1)?;
            bounded_concat(&[&function, "()"], MAX_NAME_BYTES)
        }
        "member_call_expression" | "nullsafe_member_call_expression" | "scoped_call_expression" => {
            bounded_concat(
                &[&render_member(source, node, depth)?, "()"],
                MAX_NAME_BYTES,
            )
        }
        _ => None,
    }
}

/// `receiver->name`, `receiver?->name`, or `scope::name` for a member access
/// or member call.
fn render_member(source: &str, node: Node<'_>, depth: usize) -> Option<String> {
    let name = node
        .child_by_field_name("name")
        .filter(|name| name.kind() == "name")?;
    let (receiver, operator) = match node.kind() {
        "scoped_call_expression" => (node.child_by_field_name("scope")?, "::"),
        "nullsafe_member_call_expression" | "nullsafe_member_access_expression" => {
            (node.child_by_field_name("object")?, "?->")
        }
        _ => (node.child_by_field_name("object")?, "->"),
    };
    let receiver = render_receiver(source, receiver, depth + 1)?;
    bounded_concat(
        &[&receiver, operator, node_text(source, name).trim()],
        MAX_NAME_BYTES,
    )
}

/// Emit a static or scoped (`::`) method call.
fn capture_scoped_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    let (Some(name_node), Some(scope)) = (
        node.child_by_field_name("name")
            .filter(|name| name.kind() == "name"),
        node.child_by_field_name("scope"),
    ) else {
        return Ok(());
    };
    let Some(method) = php_identifier_text(builder, name_node)? else {
        return Ok(());
    };
    let scope_text =
        render_receiver(source, scope, 0).unwrap_or_else(|| OPAQUE_RECEIVER.to_owned());
    let Some(name) = bounded_concat(&[&scope_text, KEY_SEPARATOR, &method], MAX_NAME_BYTES) else {
        return Ok(());
    };
    let class_scope = scope.kind() == "relative_scope" || is_class_name_node(&scope);
    let resolution_name = class_scope
        .then(|| builder.php.class_lookup(&scope_text, Some(&method)))
        .flatten();
    builder.emit_reference(ExtractedReference {
        owner: builder.owners.last().cloned(),
        name,
        resolution_name,
        kind: ReferenceKind::Calls,
        span: span_for(name_node)?,
    })
}

/// Emit an `instantiates` reference for `new` with a static class name.
fn capture_instantiation(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(class) = named_children(node).find(|child| child.kind() != "arguments") else {
        return Ok(());
    };
    if !is_class_name_node(&class) {
        return Ok(());
    }
    emit_class_reference(
        builder,
        ClassReference {
            owner: builder.owners.last().cloned(),
            node: class,
            kind: ReferenceKind::Instantiates,
        },
    )
}

/// The source text of one node.
fn node_text<'source>(source: &'source str, node: Node<'_>) -> &'source str {
    source
        .get(node.start_byte()..node.end_byte())
        .unwrap_or_default()
}

/// A bounded owned copy of a validated PHP (possibly qualified) name.
fn php_name_text(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let source = builder.context.snapshot.source();
    php_name(node_text(source, node))
        .map(|name| builder.context.copy_text(name))
        .transpose()
}

/// A bounded owned copy of a validated unqualified PHP identifier.
fn php_identifier_text(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let source = builder.context.snapshot.source();
    let text = node_text(source, node).trim();
    if !valid_segment(text) {
        return Ok(None);
    }
    builder.context.copy_text(text).map(Some)
}
