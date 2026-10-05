//! TypeScript contract properties named by string-literal type arguments.
//!
//! RPC-style service lists encode their operation names as string-literal
//! generic arguments: `export type Api = [Service<'apply_confirm', Req, Resp>]`.
//! Each such literal becomes a [`SymbolKind::Property`] contained by the type
//! alias (`Api::apply_confirm`), exported with it, and signed by the generic
//! head (`Service`) — the v1 alias-contained contract property. A plain
//! literal union (`'metric' | 'imperial'`) is not a generic argument and
//! produces nothing.
//!
//! v1 treated every string-literal type argument as a contract member, which
//! also published keys that utility types select, remove, or transform
//! (`Omit<Model, 'debug'>`, `ComponentPropsWithoutRef<'button'>`,
//! `Uppercase<'x'>`), even when a union repeats them for each element of a
//! polymorphic component. Here a literal names a contract member only with
//! declared evidence: it is the first type argument of a generic declared at
//! the top level of the same file as an object shape (an interface, a class,
//! or an object-literal type alias) whose first type parameter is a string
//! name that one of its members is typed by
//! (`interface Service<Name extends string, Req, Resp> { name: Name }`), and
//! no nearer scope (a type parameter, a block's type or import alias)
//! declares the same name. A string transformer such as
//! ``type EventName<T extends string> = `on${T}` `` is no contract, and a
//! generic imported from another file carries no evidence in per-file
//! extraction.

use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::{SourceLanguage, SymbolId, SymbolKind};
use tree_sitter::Node;

use crate::{ExtractError, SymbolExportFlags};

use super::{
    ExtractionBuilder, PendingSymbol, schema,
    syntax::{descendants_including_root, named_children},
};

/// Top-level declarations that can introduce a generic type.
const GENERIC_DECLARATION_KINDS: &[&str] = &[
    "interface_declaration",
    "type_alias_declaration",
    "class_declaration",
    "abstract_class_declaration",
];

/// Most `export`/`declare` wrappers looked through around one declaration.
const MAX_DECLARATION_WRAPPERS: usize = 4;
/// Most ancestors of one generic instantiation inspected for a nearer
/// declaration of its name; a deeper instantiation counts as shadowed.
const MAX_SHADOW_ANCESTORS: usize = crate::MAXIMUM_AST_DEPTH;
/// Most syntax nodes scanned for block-declared contract names; a larger
/// file treats every block as shadowing.
const MAX_SHADOW_SCAN_NODES: usize = 4_000_000;
/// Shadow-scan nodes between two cancellation checks.
const SHADOW_CANCELLATION_INTERVAL: usize = 256;

/// Generics this file declares with a string name as their first type
/// parameter.
#[derive(Default)]
pub(super) struct ContractGenerics {
    names: BTreeSet<String>,
    /// For each scope (by byte range) that declares a type named like a
    /// contract generic — a block or `switch` body declaring a type, an
    /// interface, a class, an enum, or an import alias, or a declaration with
    /// such a type parameter — those names: an instantiation inside the scope
    /// names that nearer declaration.
    shadows: BTreeMap<(usize, usize), BTreeSet<String>>,
    /// The file was too large to scan for shadows; every scope that could
    /// declare a type counts as shadowing.
    shadows_unknown: bool,
}

/// The alias that contains the contract properties.
#[derive(Clone, Copy)]
pub(super) struct ContractAlias<'alias> {
    id: &'alias SymbolId,
    name: &'alias str,
    export: SymbolExportFlags,
}

impl<'alias> ContractAlias<'alias> {
    pub(super) const fn new(
        id: &'alias SymbolId,
        name: &'alias str,
        export: SymbolExportFlags,
    ) -> Self {
        Self { id, name, export }
    }
}

/// One first-position string-literal generic argument and the generic that
/// receives it.
#[derive(Clone, Copy)]
struct ContractArgument<'tree> {
    literal: Node<'tree>,
    generic_name: Node<'tree>,
}

/// Record the top-level generics whose first type parameter is a string name
/// (`interface Service<Name extends string, ..>`), before the walk reaches
/// the aliases that instantiate them.
pub(super) fn collect_contract_generics(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    if !is_typescript(builder) {
        return Ok(());
    }
    let source = builder.context.snapshot.source();
    for statement in named_children(root) {
        builder.context.ensure_active()?;
        let Some(declaration) = generic_declaration(statement)
            .filter(|declaration| declares_contract_name_parameter(*declaration, source))
        else {
            continue;
        };
        if let Some(name) = declaration.child_by_field_name("name") {
            let name = builder.context.owned_text(name)?;
            builder.javascript.contract_generics.names.insert(name);
        }
    }
    if builder.javascript.contract_generics.names.is_empty() {
        return Ok(());
    }
    collect_block_shadows(builder, root)
}

/// One bounded pass recording the scopes that declare a type named like one
/// of the file's contract generics, so each instantiation checks its
/// enclosing scopes by lookup instead of rescanning their declarations.
fn collect_block_shadows(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    let mut cursor = root.walk();
    let mut visits = 0_usize;
    loop {
        visits = visits.saturating_add(1);
        if visits > MAX_SHADOW_SCAN_NODES {
            builder.javascript.contract_generics.shadows_unknown = true;
            return Ok(());
        }
        if visits.is_multiple_of(SHADOW_CANCELLATION_INTERVAL) {
            builder.context.ensure_active()?;
        }
        record_scope_shadows(builder, (cursor.node(), source))?;
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(());
            }
        }
    }
}

/// Record the contract-generic names one scope declares: a block's or
/// `switch` body's statement declarations, or a declaration's type
/// parameters.
fn record_scope_shadows(
    builder: &mut ExtractionBuilder<'_, '_>,
    (scope, source): (Node<'_>, &str),
) -> Result<(), ExtractError> {
    match scope.kind() {
        "statement_block" => {
            for statement in named_children(scope) {
                record_shadow(builder, scope, block_type_name(statement, source))?;
            }
        }
        "switch_body" => {
            for statement in named_children(scope).flat_map(named_children) {
                record_shadow(builder, scope, block_type_name(statement, source))?;
            }
        }
        _ => {}
    }
    let Some(parameters) = scope.child_by_field_name("type_parameters") else {
        return Ok(());
    };
    for parameter in named_children(parameters) {
        let name = parameter
            .child_by_field_name("name")
            .and_then(|name| source.get(name.start_byte()..name.end_byte()));
        record_shadow(builder, scope, name)?;
    }
    Ok(())
}

/// Record `name` as declared by `scope` when a contract generic has it.
fn record_shadow(
    builder: &mut ExtractionBuilder<'_, '_>,
    scope: Node<'_>,
    name: Option<&str>,
) -> Result<(), ExtractError> {
    let Some(name) = name.filter(|name| builder.javascript.contract_generics.names.contains(*name))
    else {
        return Ok(());
    };
    let name = builder.context.copy_text(name)?;
    builder
        .javascript
        .contract_generics
        .shadows
        .entry((scope.start_byte(), scope.end_byte()))
        .or_default()
        .insert(name);
    Ok(())
}

/// The generic-introducing declaration of a top-level statement.
fn generic_declaration(statement: Node<'_>) -> Option<Node<'_>> {
    unwrapped_declaration(statement)
        .filter(|declaration| GENERIC_DECLARATION_KINDS.contains(&declaration.kind()))
}

/// The declaration a statement makes, looking through `export` and `declare`
/// in any combination (`export declare class`).
fn unwrapped_declaration(statement: Node<'_>) -> Option<Node<'_>> {
    let mut declaration = statement;
    for _ in 0..MAX_DECLARATION_WRAPPERS {
        declaration = match declaration.kind() {
            "export_statement" => declaration.child_by_field_name("declaration")?,
            "ambient_declaration" => first_named(declaration)?,
            _ => return Some(declaration),
        };
    }
    None
}

/// Whether the first type parameter is a string name (`Name extends
/// string`) that a member of the declared object shape is typed by
/// (`name: Name`).
fn declares_contract_name_parameter(declaration: Node<'_>, source: &str) -> bool {
    let Some(parameter) = declaration
        .child_by_field_name("type_parameters")
        .and_then(first_named)
        .filter(|parameter| is_string_constrained(*parameter, source))
        .and_then(|parameter| parameter.child_by_field_name("name"))
        .and_then(|name| source.get(name.start_byte()..name.end_byte()))
    else {
        return false;
    };
    object_shape_body(declaration).is_some_and(|body| {
        named_children(body).any(|member| member_is_typed_by(member, parameter, source))
    })
}

/// Whether a type parameter is constrained to `string`.
fn is_string_constrained(parameter: Node<'_>, source: &str) -> bool {
    parameter.kind() == "type_parameter"
        && parameter
            .child_by_field_name("constraint")
            .and_then(first_named)
            .is_some_and(|bound| {
                bound.kind() == "predefined_type"
                    && source.get(bound.start_byte()..bound.end_byte()) == Some("string")
            })
}

/// The member list of an interface, a class, or an object-literal type alias.
fn object_shape_body(declaration: Node<'_>) -> Option<Node<'_>> {
    let body = match declaration.kind() {
        "type_alias_declaration" => declaration.child_by_field_name("value"),
        _ => declaration.child_by_field_name("body"),
    }?;
    matches!(body.kind(), "interface_body" | "object_type" | "class_body").then_some(body)
}

/// Whether a property member's declared type is exactly `parameter`.
fn member_is_typed_by(member: Node<'_>, parameter: &str, source: &str) -> bool {
    matches!(
        member.kind(),
        "property_signature" | "public_field_definition"
    ) && member
        .child_by_field_name("type")
        .and_then(first_named)
        .is_some_and(|declared| {
            declared.kind() == "type_identifier"
                && source.get(declared.start_byte()..declared.end_byte()) == Some(parameter)
        })
}

/// The first named child that is not a comment.
fn first_named(node: Node<'_>) -> Option<Node<'_>> {
    named_children(node).find(|child| child.kind() != "comment")
}

/// Whether the file is TypeScript or TSX, the only grammars with generic
/// type arguments.
fn is_typescript(builder: &ExtractionBuilder<'_, '_>) -> bool {
    matches!(
        builder.context.snapshot.language(),
        SourceLanguage::TypeScript | SourceLanguage::Tsx
    )
}

/// Emit one contract property per distinct safe string literal that a
/// contract generic of this file takes as its first argument anywhere in the
/// alias value.
pub(super) fn emit_contract_properties(
    builder: &mut ExtractionBuilder<'_, '_>,
    value: Node<'_>,
    alias: ContractAlias<'_>,
) -> Result<(), ExtractError> {
    if !is_typescript(builder) || builder.javascript.contract_generics.names.is_empty() {
        return Ok(());
    }
    let source = builder.context.snapshot.source();
    let mut arguments = Vec::new();
    for node in descendants_including_root(value) {
        builder.context.ensure_active()?;
        if node.kind() != "generic_type" {
            continue;
        }
        let Some(argument) = first_literal_argument(node).filter(|argument| {
            source
                .get(argument.generic_name.start_byte()..argument.generic_name.end_byte())
                .map(str::trim)
                .is_some_and(|head| {
                    let generics = &builder.javascript.contract_generics;
                    generics.names.contains(head) && !generics.nearer_declaration(node, head)
                })
        }) else {
            continue;
        };
        arguments
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        arguments.push(argument);
    }
    if arguments.is_empty() {
        return Ok(());
    }
    builder.owners.push(alias.id.clone());
    builder.qualifiers.push(alias.name.to_owned());
    let emitted = emit_arguments(builder, &arguments, alias.export);
    builder.qualifiers.pop();
    builder.owners.pop();
    emitted
}

impl ContractGenerics {
    /// Whether a scope between an instantiation and the top level declares
    /// its generic's name again — a type parameter (`type Api<Service> = ..`)
    /// or a type, interface, class, enum, or import alias declared in an
    /// enclosing block — so the instantiation does not name the file's
    /// top-level contract generic.
    fn nearer_declaration(&self, instantiation: Node<'_>, head: &str) -> bool {
        let mut current = instantiation.parent();
        for _ in 0..MAX_SHADOW_ANCESTORS {
            let Some(scope) = current else {
                return false;
            };
            if scope.kind() == "program" {
                return false;
            }
            let shadowed = if self.shadows_unknown {
                matches!(scope.kind(), "statement_block" | "switch_body")
                    || scope.child_by_field_name("type_parameters").is_some()
            } else {
                self.shadows
                    .get(&(scope.start_byte(), scope.end_byte()))
                    .is_some_and(|shadows| shadows.contains(head))
            };
            if shadowed {
                return true;
            }
            current = scope.parent();
        }
        true
    }
}

/// The type-level name a block statement declares (a namespace body's
/// included): a type alias, interface, class, enum, or import alias.
fn block_type_name<'source>(statement: Node<'_>, source: &'source str) -> Option<&'source str> {
    let declaration = unwrapped_declaration(statement)?;
    let name = if declaration.kind() == "import_alias" {
        first_named(declaration)
    } else if GENERIC_DECLARATION_KINDS.contains(&declaration.kind())
        || declaration.kind() == "enum_declaration"
    {
        declaration.child_by_field_name("name")
    } else {
        None
    }?;
    source.get(name.start_byte()..name.end_byte())
}

/// The string literal a generic type takes as its first type argument.
fn first_literal_argument(generic: Node<'_>) -> Option<ContractArgument<'_>> {
    let generic_name = generic.child_by_field_name("name")?;
    let first = first_named(generic.child_by_field_name("type_arguments")?)?;
    let literal = match first.kind() {
        "string" => Some(first),
        "literal_type" => named_children(first).find(|child| child.kind() == "string"),
        _ => None,
    }?;
    Some(ContractArgument {
        literal,
        generic_name,
    })
}

/// Emit one property per distinct safe literal, in source order.
fn emit_arguments(
    builder: &mut ExtractionBuilder<'_, '_>,
    arguments: &[ContractArgument<'_>],
    export: SymbolExportFlags,
) -> Result<(), ExtractError> {
    let mut seen = BTreeSet::new();
    for argument in arguments {
        let name = builder.context.owned_unquoted_text(argument.literal)?;
        if !schema::safe_literal_name(&name) || seen.contains(&name) {
            continue;
        }
        let signature = generic_head(builder, argument.generic_name)?;
        seen.insert(name.clone());
        builder.emit_symbol(PendingSymbol {
            kind: SymbolKind::Property,
            name,
            span_node: argument.literal,
            structural_node: argument.literal,
            doc_anchor: argument.literal,
            body_node: None,
            declaration_only: false,
            signature,
            export,
            async_symbol: false,
            static_member: false,
            visibility: None,
        })?;
    }
    Ok(())
}

/// The literal-free generic head (`Service`, `api.Service`) used as the
/// property signature; the literal itself is the property name.
fn generic_head(
    builder: &mut ExtractionBuilder<'_, '_>,
    generic_name: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let head = builder.context.text(generic_name).trim();
    if head.is_empty()
        || !head
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'.'))
    {
        return Ok(None);
    }
    builder.context.copy_text(head).map(Some)
}
