//! Lua, Luau, and KHN structural extraction.
//!
//! Restores the v1 Lua extractor (shared by Luau and KHN): definitions keep
//! their full dotted (`M.f`) or colon (`M:f`) names, colon definitions are
//! methods, each name in a `local a, b = ...` list is its own binding,
//! function values are named by the variable they are assigned to rather than
//! by a parameter, and `require` with a literal module name is an import.
//! Luau adds type aliases that are exported only when declared `export type`
//! and return types in callable signatures.

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind};
use tree_sitter::Node;

use crate::{ExtractError, ImportBindingKind, SymbolExportFlags};

use super::{
    ExtractionBuilder, MAX_SAFE_SIGNATURE_BYTES, PendingReference, references,
    script_support::{
        LoadImport, MAX_SCRIPT_NAME_BYTES, OwnerScope, bounded_name, bounded_reference_name,
        bounded_text, contains_literal, emit_load_import, literal_free_assignment_signature,
        literal_free_signature, plain_symbol, with_owner,
    },
    syntax::named_children,
};

/// Index syntax whose table and member name a call target (`a.b`, `a:b`).
const INDEX_CALLEES: [&str; 2] = ["dot_index_expression", "method_index_expression"];
/// Maximum syntax steps (members and parentheses) followed through one callee.
const MAX_CALLEE_STEPS: usize = MAX_SCRIPT_NAME_BYTES;
/// Parameter list rendered for a definition that has none.
const EMPTY_PARAMETERS: &str = "()";

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "function_declaration" => visit_function_declaration(builder, node, depth),
        "variable_declaration" => visit_local_declaration(builder, node, depth).map(|()| true),
        "assignment_statement" => visit_function_assignment(builder, node, depth),
        "function_definition" => {
            // An anonymous function value is not a declaration; its body still
            // belongs to the enclosing owner.
            builder.visit_named_children(node, depth)?;
            Ok(true)
        }
        "type_definition" => visit_type_definition(builder, node),
        _ => Ok(false),
    }
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() != "function_call" {
        return Ok(());
    }
    let Some(callee) = node.child_by_field_name("name") else {
        return Ok(());
    };
    // A computed callee (`get(x).run()`) has no stable name; its inner calls
    // are captured on their own.
    let Some(name) = static_callee_name(builder, callee)
        .map(|name| bounded_reference_name(builder, &name))
        .transpose()?
        .flatten()
    else {
        return Ok(());
    };
    let is_require = name == "require";
    let owner = builder.owners.last().cloned();
    references::push_reference(
        builder,
        PendingReference {
            owner,
            name,
            kind: ReferenceKind::Calls,
            node: callee,
        },
    )?;
    if is_require {
        capture_require(builder, node)?;
    }
    Ok(())
}

fn visit_function_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(false);
    };
    let kind = match name_node.kind() {
        "identifier" | "dot_index_expression" => SymbolKind::Function,
        "method_index_expression" => SymbolKind::Method,
        _ => return Ok(false),
    };
    let Some(name) = bounded_name(builder, name_node)? else {
        return Ok(false);
    };
    let global = name_node.kind() == "identifier" && !declared_local(node);
    let signature = callable_signature(builder, CallableShape { node, name: &name })?;
    let mut pending = plain_symbol(kind, name.clone(), node);
    pending.body_node = Some(node);
    pending.signature = signature;
    pending.export = SymbolExportFlags::named(global);
    let id = builder.emit_symbol(pending)?;
    let scope = OwnerScope {
        id: &id,
        kind,
        name: &name,
    };
    with_owner(builder, scope, |builder| {
        builder.visit_named_children(node, depth)
    })?;
    Ok(true)
}

/// `local a, b = 1, function(q) ... end` declares one binding per name, paired
/// with the value at the same position.
fn visit_local_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let assignment = named_children(node).find(|child| child.kind() == "assignment_statement");
    let names_parent = assignment.unwrap_or(node);
    let Some(name_list) =
        named_children(names_parent).find(|child| child.kind() == "variable_list")
    else {
        return builder.visit_named_children(node, depth);
    };
    let names = field_children(name_list, "name");
    let values = assignment
        .and_then(|assignment| {
            named_children(assignment).find(|child| child.kind() == "expression_list")
        })
        .map(|list| field_children(list, "value"))
        .unwrap_or_default();
    for (index, name_node) in names.iter().enumerate() {
        let binding = LocalBinding {
            declaration: node,
            name_node: *name_node,
            value: values.get(index).copied(),
            sole: names.len() == 1,
            depth,
        };
        visit_local_binding(builder, binding)?;
    }
    for value in values.iter().skip(names.len()) {
        builder.visit(*value, depth.saturating_add(1))?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct LocalBinding<'tree> {
    declaration: Node<'tree>,
    name_node: Node<'tree>,
    value: Option<Node<'tree>>,
    sole: bool,
    depth: usize,
}

fn visit_local_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    binding: LocalBinding<'_>,
) -> Result<(), ExtractError> {
    let Some(name) = bounded_name(builder, binding.name_node)? else {
        if let Some(value) = binding.value {
            builder.visit(value, binding.depth.saturating_add(1))?;
        }
        return Ok(());
    };
    if let Some(value) = binding
        .value
        .filter(|value| value.kind() == "function_definition")
    {
        let span = if binding.sole {
            binding.declaration
        } else {
            value
        };
        return visit_assigned_function(
            builder,
            &AssignedFunction {
                span,
                value,
                name,
                exported: false,
                depth: binding.depth,
            },
        );
    }
    let signature = match binding.value {
        Some(value) => literal_free_assignment_signature(builder, value)?,
        None => None,
    };
    let mut pending = plain_symbol(SymbolKind::Variable, name.clone(), binding.name_node);
    // A sole binding's structure is its whole declaration; in a list each
    // binding is analysed through its own value (or name), so a wide
    // `local a, b, ...` list is not re-analysed once per name.
    pending.structural_node = if binding.sole {
        binding.declaration
    } else {
        binding.value.unwrap_or(binding.name_node)
    };
    pending.doc_anchor = binding.declaration;
    pending.signature = signature;
    let id = builder.emit_symbol(pending)?;
    if let Some(value) = binding.value {
        let scope = OwnerScope {
            id: &id,
            kind: SymbolKind::Variable,
            name: &name,
        };
        with_owner(builder, scope, |builder| {
            builder.visit(value, binding.depth.saturating_add(1))
        })?;
    }
    Ok(())
}

/// `name = function(...) ... end` and `M.f = function(...) ... end` name the
/// function after their single target, exactly like `function name(...)` and
/// `function M.f(...)`. Without scope analysis an identifier target may be a
/// forward-declared local, so assigned functions are never treated as globals.
fn visit_function_assignment(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    if node
        .parent()
        .is_some_and(|parent| parent.kind() == "variable_declaration")
    {
        return Ok(false);
    }
    let names = named_children(node)
        .find(|child| child.kind() == "variable_list")
        .map(|list| field_children(list, "name"))
        .unwrap_or_default();
    let values = named_children(node)
        .find(|child| child.kind() == "expression_list")
        .map(|list| field_children(list, "value"))
        .unwrap_or_default();
    let ([target], [value]) = (names.as_slice(), values.as_slice()) else {
        return Ok(false);
    };
    if !matches!(target.kind(), "identifier" | "dot_index_expression")
        || value.kind() != "function_definition"
    {
        return Ok(false);
    }
    // Only a static target (`f`, `M.f`, `a.b.c`) names a declaration; a
    // computed one (`make().f`, `t[1].f`) stays an ordinary assignment whose
    // left-hand calls are still captured.
    let Some(name) = static_callee_name(builder, *target)
        .map(|name| bounded_reference_name(builder, &name))
        .transpose()?
        .flatten()
    else {
        return Ok(false);
    };
    visit_assigned_function(
        builder,
        &AssignedFunction {
            span: node,
            value: *value,
            name,
            exported: false,
            depth,
        },
    )?;
    Ok(true)
}

/// A function value named by the binding it is assigned to.
struct AssignedFunction<'tree> {
    span: Node<'tree>,
    value: Node<'tree>,
    name: String,
    exported: bool,
    depth: usize,
}

fn visit_assigned_function(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: &AssignedFunction<'_>,
) -> Result<(), ExtractError> {
    let signature = callable_signature(
        builder,
        CallableShape {
            node: input.value,
            name: &input.name,
        },
    )?;
    let mut pending = plain_symbol(SymbolKind::Function, input.name.clone(), input.span);
    pending.structural_node = input.value;
    pending.body_node = Some(input.value);
    pending.signature = signature;
    pending.export = SymbolExportFlags::named(input.exported);
    let id = builder.emit_symbol(pending)?;
    let scope = OwnerScope {
        id: &id,
        kind: SymbolKind::Function,
        name: &input.name,
    };
    with_owner(builder, scope, |builder| {
        builder.visit_named_children(input.value, input.depth.saturating_add(1))
    })
}

/// Luau `type Name = ...` aliases, exported only with an explicit `export`.
fn visit_type_definition(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(false);
    };
    let identifier = if name_node.kind() == "identifier" {
        Some(name_node)
    } else {
        named_children(name_node).find(|child| child.kind() == "identifier")
    };
    let Some(name) = identifier
        .map(|identifier| bounded_name(builder, identifier))
        .transpose()?
        .flatten()
    else {
        return Ok(false);
    };
    let mut pending = plain_symbol(SymbolKind::TypeAlias, name, node);
    pending.export = SymbolExportFlags::named(leading_keyword(node, "export"));
    builder.emit_symbol(pending)?;
    Ok(true)
}

#[derive(Clone, Copy)]
struct CallableShape<'tree, 'name> {
    node: Node<'tree>,
    name: &'name str,
}

/// `function <name><parameters>`, with a Luau return annotation appended as
/// `: <type>`, retained only when neither part contains literal syntax.
fn callable_signature(
    builder: &ExtractionBuilder<'_, '_>,
    shape: CallableShape<'_, '_>,
) -> Result<Option<String>, ExtractError> {
    let parameters = shape.node.child_by_field_name("parameters");
    let parameter_text = parameters.map_or(EMPTY_PARAMETERS, |node| builder.context.text(node));
    let return_node = parameters
        .and_then(|parameters| parameters.next_named_sibling())
        .filter(|_| builder.context.snapshot.language() == SourceLanguage::Luau)
        .filter(|candidate| shape.node.child_by_field_name("body") != Some(*candidate));
    if [parameters, return_node]
        .into_iter()
        .flatten()
        .any(contains_literal)
    {
        return Ok(None);
    }
    let return_type = return_node
        .map(|candidate| {
            builder
                .context
                .text(candidate)
                .trim()
                .trim_start_matches(':')
                .trim()
        })
        .filter(|text| !text.is_empty());
    let unbounded = parameter_text
        .len()
        .saturating_add(return_type.map_or(0, str::len))
        > MAX_SAFE_SIGNATURE_BYTES;
    if unbounded {
        return Ok(None);
    }
    let mut signature = format!("function {}{}", shape.name, parameter_text.trim());
    if let Some(return_type) = return_type {
        signature.push_str(": ");
        signature.push_str(return_type);
    }
    literal_free_signature(builder, &signature)
}

/// `f`, `a.b.c`, or `a.b:c` rebuilt from identifier parts (parentheses and
/// spacing dropped), or `None` for a callee with any computed part. The chain
/// is followed iteratively and bounded by the retained name length, so a long
/// static path is named however deep it nests.
fn static_callee_name(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> Option<String> {
    let mut members = Vec::new();
    let mut length = 0_usize;
    let mut current = node;
    for _ in 0..MAX_CALLEE_STEPS {
        match current.kind() {
            "identifier" => {
                let head = builder.context.text(current).trim();
                return (head.len().saturating_add(length) <= MAX_SCRIPT_NAME_BYTES)
                    .then(|| join_members(head, members));
            }
            "parenthesized_expression" => current = sole_named_child(current)?,
            _ => {
                let step = index_step(builder, current)?;
                length = length
                    .saturating_add(step.member.len())
                    .saturating_add(step.separator.len_utf8());
                if length > MAX_SCRIPT_NAME_BYTES {
                    return None;
                }
                members.push((step.separator, step.member));
                current = step.table;
            }
        }
    }
    None
}

/// One `table.member` or `table:member` step of a static callee path.
struct IndexStep<'tree, 'source> {
    table: Node<'tree>,
    separator: char,
    member: &'source str,
}

/// The table, separator, and identifier member of an index expression.
fn index_step<'tree, 'source>(
    builder: &'source ExtractionBuilder<'_, '_>,
    node: Node<'tree>,
) -> Option<IndexStep<'tree, 'source>> {
    let kind = node.kind();
    if !INDEX_CALLEES.contains(&kind) {
        return None;
    }
    let (member_field, separator) = if kind == "method_index_expression" {
        ("method", ':')
    } else {
        ("field", '.')
    };
    let member = node
        .child_by_field_name(member_field)
        .filter(|member| member.kind() == "identifier")?;
    Some(IndexStep {
        table: node.child_by_field_name("table")?,
        separator,
        member: builder.context.text(member).trim(),
    })
}

/// `head` followed by the members collected outermost-first, in source order.
fn join_members(head: &str, members: Vec<(char, &str)>) -> String {
    let mut name = head.to_owned();
    for (separator, member) in members.into_iter().rev() {
        name.push(separator);
        name.push_str(member);
    }
    name
}

/// The only named child of `node`, if it has exactly one.
fn sole_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut inner = named_children(node);
    match (inner.next(), inner.next()) {
        (Some(child), None) => Some(child),
        _ => None,
    }
}

/// A `require("x")` / `require "x"` call with a literal module name.
fn capture_require(
    builder: &mut ExtractionBuilder<'_, '_>,
    call: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(content) = call
        .child_by_field_name("arguments")
        .and_then(|arguments| named_children(arguments).next())
        .filter(|argument| argument.kind() == "string")
        .and_then(sole_string_content)
    else {
        return Ok(());
    };
    let text = builder.context.text(content);
    if text.contains('\\') {
        return Ok(());
    }
    let Some(display) = bounded_text(builder, text)? else {
        return Ok(());
    };
    let specifier = builder.context.copy_text(&display)?;
    emit_load_import(
        builder,
        LoadImport {
            site: call,
            display,
            specifier,
            namespace: None,
            kind: ImportBindingKind::Namespace,
        },
    )
}

/// The content of a string with no interpolation or other parts.
fn sole_string_content(string: Node<'_>) -> Option<Node<'_>> {
    let mut parts = named_children(string);
    let content = parts.next()?;
    (parts.next().is_none() && content.kind() == "string_content").then_some(content)
}

fn field_children<'tree>(node: Node<'tree>, field: &str) -> Vec<Node<'tree>> {
    let mut cursor = node.walk();
    node.children_by_field_name(field, &mut cursor).collect()
}

fn declared_local(node: Node<'_>) -> bool {
    leading_keyword(node, "local")
}

fn leading_keyword(node: Node<'_>, keyword: &str) -> bool {
    node.child(0).is_some_and(|first| first.kind() == keyword)
}
