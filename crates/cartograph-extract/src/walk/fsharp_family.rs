//! F# structural extraction.
//!
//! F# declares namespaces and modules that scope everything after them, binds
//! functions and values with the same `let` form (told apart only by whether
//! the left side takes arguments), and calls functions by juxtaposition rather
//! than with a dedicated call node. This family keeps those distinctions,
//! records `open` declarations as namespace imports, and names a call from the
//! head of its outermost application. Declarations are public unless an access
//! modifier says otherwise.

use cartograph_domain::{
    ReferenceKind, SymbolKind, Visibility, callable_signature_is_literal_free,
};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind, SymbolExportFlags};

use super::{
    ExtractionBuilder, NamedDeclaration, NodeRange, PendingReference, PendingSymbol, SymbolScope,
    family_support::screened_name,
    in_symbol_scope, references, safe_assignment_signature,
    syntax::{has_child_kind, named_child_of_kind, named_children, span_for},
    widen_symbol_span, with_root_scope,
};

/// The imported and local name of an `open` declaration's namespace binding.
const WILDCARD: &str = "*";
/// Nested applications followed to reach a call's head.
const MAXIMUM_APPLICATION_DEPTH: usize = 64;
/// Applications of computation-expression builders and discard helpers that
/// v1 never recorded as calls.
const NON_CALL_HEADS: [&str; 4] = ["async", "seq", "ignore", "nameof"];
/// Owners whose `let` bindings and types are part of the public surface.
const MODULE_OWNER_KINDS: [SymbolKind; 2] = [SymbolKind::Namespace, SymbolKind::Module];

/// Extract one F# declaration, returning whether `node` was consumed.
pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    if visit_scope_declaration(builder, node, depth)? {
        return Ok(true);
    }
    visit_type_declaration(builder, node, depth)
}

/// Record the call an outermost application performs, if any.
pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() != "application_expression"
        || node
            .parent()
            .is_some_and(|parent| parent.kind() == "application_expression")
    {
        return Ok(());
    }
    let Some(head) = call_head(builder, node) else {
        return Ok(());
    };
    let name = builder.context.owned_text(head)?;
    if !is_dotted_identifier(&name) || NON_CALL_HEADS.contains(&name.as_str()) {
        return Ok(());
    }
    references::push_reference(
        builder,
        PendingReference {
            owner: builder.owners.last().cloned(),
            name,
            kind: ReferenceKind::Calls,
            node: head,
        },
    )
}

/// The node naming the function an outermost application calls: the leftmost
/// head of a curried application, the member of a dotted head, the function
/// of a type application (`f<int> x`), or the piped function of
/// `xs |> List.map f`, which the grammar parses as `(xs |> List.map) f`.
fn call_head<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    application: Node<'tree>,
) -> Option<Node<'tree>> {
    let mut head = application.named_child(0)?;
    for _ in 0..MAXIMUM_APPLICATION_DEPTH {
        match head.kind() {
            "application_expression" | "typed_expression" => head = head.named_child(0)?,
            "long_identifier_or_op" | "identifier" => return Some(head),
            "dot_expression" => return head.child_by_field_name("field"),
            "infix_expression" => head = piped_function(builder, head)?,
            _ => return None,
        }
    }
    None
}

/// The right operand of a forward pipe (`|>`, `||>`, `|||>`); other infix
/// operators name no callee.
fn piped_function<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    infix: Node<'tree>,
) -> Option<Node<'tree>> {
    let operator = named_child_of_kind(infix, "infix_op")?;
    if !matches!(builder.context.text(operator).trim(), "|>" | "||>" | "|||>") {
        return None;
    }
    named_children(infix)
        .last()
        .filter(|operand| *operand != operator)
}

/// Whether `name` is a plain, possibly dotted, identifier rather than an
/// operator or literal.
fn is_dotted_identifier(name: &str) -> bool {
    !super::specifier_safety::specifier_may_carry_credential(name)
        && name.starts_with(|character: char| character.is_ascii_alphabetic() || character == '_')
        && name
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '.' | '\''))
}

/// The dotted name an identifier or `long_identifier` spells, built in one
/// pass from its identifier segments so interleaved comments never become
/// part of it.
fn dotted_identifier(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let segments: Vec<&str> = if node.kind() == "identifier" {
        vec![builder.context.text(node).trim()]
    } else {
        named_children(node)
            .filter(|segment| segment.kind() == "identifier")
            .map(|segment| builder.context.text(segment).trim())
            .collect()
    };
    let separators = segments.len().saturating_sub(1);
    let length = segments
        .iter()
        .try_fold(separators, |total, segment| {
            total.checked_add(segment.len())
        })
        .ok_or(ExtractError::OutputLimit)?;
    builder.context.budget.ensure_string_length(length)?;
    let mut name = String::new();
    name.try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    for (index, segment) in segments.into_iter().enumerate() {
        if index > 0 {
            name.push('.');
        }
        name.push_str(segment);
    }
    Ok(is_dotted_identifier(&name).then_some(name))
}

/// The name of a `namespace` or top-level `module`.
///
/// `namespace rec Outer.Inner` reports the anonymous `rec` keyword for the
/// `name` field, so only identifier nodes are accepted there, falling back to
/// the declaration's `long_identifier` child.
fn scope_name(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("name")
        .filter(|name| matches!(name.kind(), "identifier" | "long_identifier"))
        .or_else(|| named_child_of_kind(node, "long_identifier"))
}

/// Namespaces, modules, `open` declarations, and `let` bindings.
fn visit_scope_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let (kind, name) = match node.kind() {
        "namespace" => (SymbolKind::Namespace, scope_name(node)),
        "named_module" => (SymbolKind::Module, scope_name(node)),
        "module_defn" => (SymbolKind::Module, named_child_of_kind(node, "identifier")),
        "import_decl" => {
            visit_open(builder, node)?;
            return Ok(true);
        }
        "function_or_value_defn" => {
            visit_let(builder, node, depth)?;
            return Ok(true);
        }
        _ => return Ok(false),
    };
    let Some(name) = name else {
        return Ok(false);
    };
    let visibility = access_visibility(builder, node);
    visit_scoped(
        builder,
        Scoped {
            declaration: NamedDeclaration::new(node, name, kind),
            depth,
            visibility,
        },
    )?;
    Ok(true)
}

/// Type definitions, record fields, enum cases, and members.
fn visit_type_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let kind = match node.kind() {
        "record_type_defn" => SymbolKind::Struct,
        "anon_type_defn" => SymbolKind::Class,
        "interface_type_defn" => SymbolKind::Interface,
        "enum_type_defn" => SymbolKind::Enum,
        "union_type_defn" => SymbolKind::Union,
        "type_abbrev_defn" => SymbolKind::TypeAlias,
        "record_field" => return emit_record_field(builder, node).map(|()| true),
        "enum_type_case" => return emit_enum_case(builder, node).map(|()| true),
        "method_or_prop_defn" => return visit_member(builder, node, depth).map(|()| true),
        _ => return Ok(false),
    };
    let Some(type_name) = named_child_of_kind(node, "type_name") else {
        return Ok(false);
    };
    let Some(name) = type_name_identifier(type_name) else {
        return Ok(false);
    };
    // `type private T = ..` marks the type; `type T = private { .. }` only
    // hides its representation.
    let visibility = access_visibility(builder, type_name);
    visit_scoped(
        builder,
        Scoped {
            declaration: NamedDeclaration::new(node, name, kind),
            depth,
            visibility,
        },
    )?;
    Ok(true)
}

/// The identifier naming a `type_name`.
///
/// A plain type keeps its name as a direct `identifier`; a generic type
/// (`Box<'T>`) stores it in the `type_name` field as a `long_identifier`,
/// whose last identifier segment is the declared name.
fn type_name_identifier(type_name: Node<'_>) -> Option<Node<'_>> {
    match type_name.child_by_field_name("type_name") {
        Some(name) if name.kind() == "identifier" => Some(name),
        Some(name) if name.kind() == "long_identifier" => named_children(name)
            .filter(|segment| segment.kind() == "identifier")
            .last(),
        _ => named_child_of_kind(type_name, "identifier"),
    }
}

/// The visibility an `access_modifier` child of `node` declares, if any.
fn access_visibility(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> Option<Visibility> {
    let modifier = named_child_of_kind(node, "access_modifier")?;
    match builder.context.text(modifier).trim() {
        "private" => Some(Visibility::Private),
        "internal" => Some(Visibility::Internal),
        "public" => Some(Visibility::Public),
        _ => None,
    }
}

/// Whether declarations here belong to a file, namespace, or module.
fn in_module_scope(builder: &ExtractionBuilder<'_, '_>) -> bool {
    builder.native_owner_kinds.is_empty()
        || super::current_owner_kind_in(builder, &MODULE_OWNER_KINDS)
}

/// Whether a declaration at `node` with `visibility` is exported: it sits in
/// a file, namespace, or module scope, is not itself `private`/`internal`,
/// and no enclosing `module` is `private`/`internal`.
fn module_export(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    visibility: Option<Visibility>,
) -> bool {
    in_module_scope(builder)
        && !is_hidden(visibility)
        && !std::iter::successors(node.parent(), Node::parent)
            .filter(|ancestor| ancestor.kind() == "module_defn")
            .any(|module| is_hidden(access_visibility(builder, module)))
}

/// Whether an access modifier keeps a declaration out of the public surface.
const fn is_hidden(visibility: Option<Visibility>) -> bool {
    matches!(visibility, Some(Visibility::Private | Visibility::Internal))
}

/// A declaration whose remaining children are visited inside its scope.
#[derive(Clone, Copy)]
struct Scoped<'tree> {
    declaration: NamedDeclaration<'tree>,
    depth: usize,
    visibility: Option<Visibility>,
}

/// Emit a scoping declaration and visit its other children inside it.
fn visit_scoped(
    builder: &mut ExtractionBuilder<'_, '_>,
    scoped: Scoped<'_>,
) -> Result<(), ExtractError> {
    let Scoped {
        declaration,
        depth,
        visibility,
    } = scoped;
    let Some(name) = dotted_identifier(builder, declaration.name)? else {
        return builder.visit_named_children(declaration.node, depth);
    };
    let exported = module_export(builder, declaration.node, visibility);
    let pending = PendingSymbol {
        body_node: Some(declaration.node),
        export: SymbolExportFlags::named(exported),
        visibility: visibility.or(exported.then_some(Visibility::Public)),
        ..PendingSymbol::plain(declaration.kind, name.clone(), declaration.node)
    };
    let id = builder.emit_symbol(pending)?;
    let scope = SymbolScope {
        id,
        kind: declaration.kind,
        name,
    };
    in_symbol_scope(builder, scope, |builder| {
        for child in named_children(declaration.node).filter(|child| *child != declaration.name) {
            builder.visit(child, depth.saturating_add(1))?;
        }
        Ok(())
    })
}

/// Emit every binding of a `let` (or `let rec .. and ..`) definition and
/// visit each body inside its own binding.
fn visit_let(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let bindings = let_bindings(node);
    if bindings.is_empty() {
        return builder.visit_named_children(node, depth);
    }
    let grouped = bindings.len() > 1;
    for binding in bindings {
        builder.context.ensure_active()?;
        // A lone binding spans its whole definition; members of a group span
        // from their own left side through their own body.
        let range = NodeRange {
            first: if grouped { binding.left } else { node },
            last: if grouped {
                binding.body.unwrap_or(binding.left)
            } else {
                node
            },
        };
        visit_let_binding(
            builder,
            LetVisit {
                binding,
                range,
                depth,
            },
        )?;
    }
    Ok(())
}

/// One binding of a `let` definition: a function when its left side takes
/// arguments, otherwise a value named by a single identifier pattern.
#[derive(Clone, Copy)]
struct LetBinding<'tree> {
    kind: SymbolKind,
    left: Node<'tree>,
    name: Option<Node<'tree>>,
    body: Option<Node<'tree>>,
}

/// The bindings of a `let` definition, each paired with the body after it.
fn let_bindings(node: Node<'_>) -> Vec<LetBinding<'_>> {
    let mut cursor = node.walk();
    // Bodies follow their left sides in source order, so one forward pass
    // pairs each binding with the next body after it.
    let mut bodies = node
        .children_by_field_name("body", &mut cursor)
        .collect::<Vec<_>>()
        .into_iter();
    named_children(node)
        .filter_map(|left| match left.kind() {
            "function_declaration_left" => Some((
                SymbolKind::Function,
                left,
                named_child_of_kind(left, "identifier"),
            )),
            "value_declaration_left" => Some((SymbolKind::Variable, left, single_value_name(left))),
            _ => None,
        })
        .map(|(kind, left, name)| LetBinding {
            kind,
            left,
            name,
            body: bodies.find(|body| body.start_byte() >= left.end_byte()),
        })
        .collect()
}

/// One `let` binding to visit, with the source range its symbol spans.
#[derive(Clone, Copy)]
struct LetVisit<'tree> {
    binding: LetBinding<'tree>,
    range: NodeRange<'tree>,
    depth: usize,
}

/// Emit one binding and visit its body inside it; a destructuring pattern
/// names no symbol, so its body runs in the enclosing scope.
fn visit_let_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    visit: LetVisit<'_>,
) -> Result<(), ExtractError> {
    let LetVisit {
        binding,
        range,
        depth,
    } = visit;
    let scope = binding
        .name
        .filter(|name| {
            !super::specifier_safety::specifier_may_carry_credential(builder.context.text(*name))
        })
        .map(|name| {
            emit_let(
                builder,
                LetSymbol {
                    binding,
                    name,
                    range,
                },
            )
        })
        .transpose()?;
    let Some(body) = binding.body else {
        return Ok(());
    };
    match scope {
        Some(scope) => in_symbol_scope(builder, scope, |builder| {
            builder.visit(body, depth.saturating_add(1))
        }),
        None => builder.visit(body, depth.saturating_add(1)),
    }
}

/// A `let` binding with the identifier it declares and the range it spans.
#[derive(Clone, Copy)]
struct LetSymbol<'tree> {
    binding: LetBinding<'tree>,
    name: Node<'tree>,
    range: NodeRange<'tree>,
}

/// Emit a `let` symbol, returning the scope its body is visited in.
fn emit_let(
    builder: &mut ExtractionBuilder<'_, '_>,
    symbol: LetSymbol<'_>,
) -> Result<SymbolScope, ExtractError> {
    let LetSymbol {
        binding,
        name,
        range,
    } = symbol;
    let visibility = access_visibility(builder, binding.left);
    let exported = module_export(builder, binding.left, visibility);
    let name = builder.context.owned_text(name)?;
    let signature = match (binding.kind, binding.body) {
        (SymbolKind::Variable, Some(value)) => safe_assignment_signature(builder, value)?,
        _ => None,
    };
    let id = builder.emit_symbol(PendingSymbol {
        body_node: binding.body,
        signature,
        export: SymbolExportFlags::named(exported),
        visibility: visibility.or(exported.then_some(Visibility::Public)),
        // A grouped binding's left side alone would make every body edit
        // invisible to its structural digests.
        structural_node: if range.first == range.last {
            range.first
        } else {
            binding.body.unwrap_or(range.first)
        },
        ..PendingSymbol::plain(binding.kind, name.clone(), range.first)
    })?;
    if range.first != range.last {
        widen_symbol_span(builder, &id, range)?;
    }
    Ok(SymbolScope {
        id,
        kind: binding.kind,
        name,
    })
}

/// The identifier a value binding names when its pattern is one identifier.
fn single_value_name(left: Node<'_>) -> Option<Node<'_>> {
    let mut patterns = named_children(left).filter(|child| child.kind() != "access_modifier");
    let pattern = patterns.next()?;
    if patterns.next().is_some() || pattern.kind() != "identifier_pattern" {
        return None;
    }
    let mut names = named_children(pattern);
    let name = names.next()?;
    if names.next().is_some() {
        return None;
    }
    match name.kind() {
        "identifier" => Some(name),
        "long_identifier_or_op" => {
            let mut parts = named_children(name);
            let identifier = parts.next().filter(|part| part.kind() == "identifier")?;
            parts.next().is_none().then_some(identifier)
        }
        _ => None,
    }
}

/// Emit a `member` method or property named by its member identifier.
fn visit_member(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_field) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let name_node = name_field
        .child_by_field_name("method")
        .or_else(|| named_child_of_kind(name_field, "identifier"))
        .filter(|name| name.kind() == "identifier");
    let Some(name_node) = name_node else {
        return builder.visit_named_children(node, depth);
    };
    let member = node
        .parent()
        .filter(|parent| parent.kind() == "member_defn");
    let visibility = member
        .and_then(|member| access_visibility(builder, member))
        .unwrap_or(Visibility::Public);
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(());
    };
    let id = builder.emit_symbol(PendingSymbol {
        body_node: Some(node),
        static_member: member.is_some_and(|member| has_child_kind(member, "static")),
        visibility: Some(visibility),
        ..PendingSymbol::plain(SymbolKind::Method, name.clone(), node)
    })?;
    let scope = SymbolScope {
        id,
        kind: SymbolKind::Method,
        name,
    };
    in_symbol_scope(builder, scope, |builder| {
        for child in named_children(node).filter(|child| *child != name_field) {
            builder.visit(child, depth.saturating_add(1))?;
        }
        Ok(())
    })
}

/// Emit a record field with its literal-free `Name: Type` signature.
fn emit_record_field(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name) = named_child_of_kind(node, "identifier") else {
        return Ok(());
    };
    let text = builder.context.text(node).trim();
    let commented = text.contains("(*") || text.contains("//");
    let signature = (text.len() <= super::MAX_SAFE_SIGNATURE_BYTES
        && !commented
        && callable_signature_is_literal_free(text))
    .then(|| builder.context.copy_text(text))
    .transpose()?;
    let Some(name) = screened_name(builder, name)? else {
        return Ok(());
    };
    builder.emit_symbol(PendingSymbol {
        signature,
        visibility: Some(Visibility::Public),
        ..PendingSymbol::plain(SymbolKind::Field, name, node)
    })?;
    Ok(())
}

/// Emit an enum case; its value is a literal and never retained.
fn emit_enum_case(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name) = named_child_of_kind(node, "identifier") else {
        return Ok(());
    };
    let Some(name) = screened_name(builder, name)? else {
        return Ok(());
    };
    builder.emit_symbol(PendingSymbol::plain(SymbolKind::EnumMember, name, node))?;
    Ok(())
}

/// `open X.Y`: a root-scoped import of the whole namespace or module.
fn visit_open(builder: &mut ExtractionBuilder<'_, '_>, node: Node<'_>) -> Result<(), ExtractError> {
    let Some(target) = named_child_of_kind(node, "long_identifier") else {
        return Ok(());
    };
    let Some(module) = dotted_identifier(builder, target)? else {
        return Ok(());
    };
    let symbol_name = module.clone();
    with_root_scope(builder, |builder| {
        builder.emit_symbol(PendingSymbol::plain(SymbolKind::Import, symbol_name, node))
    })?;
    references::push_reference(
        builder,
        PendingReference {
            owner: None,
            name: module.clone(),
            kind: ReferenceKind::Imports,
            node: target,
        },
    )?;
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: module,
        imported_name: WILDCARD.to_owned(),
        local_name: WILDCARD.to_owned(),
        span: span_for(target)?,
    })
}
