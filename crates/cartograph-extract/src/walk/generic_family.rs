use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId, SymbolKind, Visibility};
use tree_sitter::Node;

use crate::{
    ExtractError, ExtractedImportBinding, ExtractedReference, ImportBindingKind,
    walk::{
        ExtractionBuilder, PendingSymbol, specifier_safety::specifier_may_carry_credential,
        syntax::span_for,
    },
};

mod abap;

const NAME_SEARCH_DEPTH: usize = 6;
const PREFIX_SCAN_BYTES: usize = 512;

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let abap = builder.context.snapshot.language() == SourceLanguage::Abap;
    // A parentless `program` node is the grammar's compilation unit (ABAP,
    // PHP, R, Ruby), never a declaration that owns the rest of the file.
    if node.parent().is_none() && node.kind() == "program" {
        if abap {
            abap::visit_program(builder, node, depth)?;
        } else {
            builder.visit_named_children(node, depth)?;
        }
        return Ok(true);
    }
    if is_import_node(node.kind()) {
        capture_import(builder, node)?;
        return Ok(true);
    }
    let declared = if abap {
        abap::declaration(builder, node)?
    } else {
        declaration(builder, node)?
    };
    let Some((kind, name)) = declared else {
        return Ok(false);
    };
    if should_skip_markup_symbol(builder.context.snapshot.language(), kind, &name) {
        return Ok(false);
    }
    if builder
        .qualifiers
        .last()
        .is_some_and(|qualifier| qualifier == &name)
    {
        builder.visit_named_children(node, depth)?;
        return Ok(true);
    }
    let qualified_name = builder.qualified_name(&name)?;
    // A same-named declaration merges into the first one (a separate
    // declaration and definition), except in ABAP, whose implemented
    // definitions are merged by the ABAP walk itself and whose same-named
    // methods are distinct misparsed interface implementations.
    if !abap
        && let Some(existing) = builder
            .facts
            .symbols
            .iter()
            .find(|symbol| symbol.kind == kind && symbol.qualified_name == qualified_name)
            .map(|symbol| symbol.id.clone())
    {
        push_scope(builder, (existing, kind, None, name));
        builder.visit_named_children(node, depth)?;
        pop_scope(builder);
        return Ok(true);
    }
    let scope = emit_declaration(
        builder,
        GenericDeclaration {
            kind,
            name,
            node,
            modifier_node: node,
        },
    )?;
    push_scope(builder, scope);
    builder.visit_named_children(node, depth)?;
    pop_scope(builder);
    Ok(true)
}

/// One declaration the generic walker emits.
struct GenericDeclaration<'tree> {
    /// Declared symbol kind.
    kind: SymbolKind,
    /// Declared local name.
    name: String,
    /// Node whose span, structure and body the symbol covers.
    node: Node<'tree>,
    /// Node whose leading tokens and doc comment carry the declaration's
    /// modifiers; an ABAP class implementation takes them from its definition.
    modifier_node: Node<'tree>,
}

/// An emitted declaration's owner scope: id, kind, visibility and name.
type GenericScope = (SymbolId, SymbolKind, Option<Visibility>, String);

/// Emit one generic declaration and return the scope its children nest in.
fn emit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: GenericDeclaration<'_>,
) -> Result<GenericScope, ExtractError> {
    let GenericDeclaration {
        kind,
        name,
        node,
        modifier_node,
    } = declaration;
    let language = builder.context.snapshot.language();
    let body = if language == SourceLanguage::Abap {
        abap::body(node)
    } else {
        node.child_by_field_name("body")
    };
    let definition = is_definition_node(node.kind());
    let source = builder.context.source();
    let visibility = generic_visibility(modifier_node, source);
    let exported = generic_exported(GenericExportInput {
        language,
        top_level: builder.owners.is_empty(),
        visibility,
        name: &name,
        node: modifier_node,
        source,
    });
    let pending = PendingSymbol {
        kind,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: modifier_node,
        body_node: body.or(definition.then_some(node)),
        declaration_only: is_callable(kind) && body.is_none() && !definition,
        signature: None,
        export: crate::SymbolExportFlags::named(exported),
        async_symbol: source_prefix_contains(modifier_node, source, "async"),
        static_member: source_prefix_contains(modifier_node, source, "static"),
        visibility,
    };
    let id = builder.emit_symbol(pending)?;
    Ok((id, kind, visibility, name))
}

/// Make `scope` the innermost owner of the declarations visited next.
fn push_scope(builder: &mut ExtractionBuilder<'_, '_>, scope: GenericScope) {
    let (id, kind, visibility, name) = scope;
    builder.owners.push(id);
    builder.native_owner_kinds.push(kind);
    builder.native_visibilities.push(visibility);
    builder.qualifiers.push(name);
}

/// Undo the most recent [`push_scope`].
fn pop_scope(builder: &mut ExtractionBuilder<'_, '_>) {
    builder.qualifiers.pop();
    builder.native_visibilities.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if is_import_node(node.kind()) {
        return capture_import(builder, node);
    }
    if is_call_node(node.kind()) {
        capture_call(builder, node)?;
    }
    if is_inheritance_node(node.kind()) {
        capture_named_reference(builder, node, ReferenceKind::Inherits)?;
    }
    Ok(())
}

fn declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<(SymbolKind, String)>, ExtractError> {
    if let Some(kind) = declaration_kind(node.kind()) {
        let name = declared_name_node(node)
            .map(|name| builder.context.owned_text(name))
            .transpose()?
            .and_then(|name| normalize_name(&name));
        if let Some(name) = name {
            return Ok(Some((kind, name)));
        }
    }
    textual_declaration(builder, node)
}

/// Function-value node kinds that are anonymous unless they carry their own
/// name (`function(q) ... end`, `(a, b) => ...`).
const ANONYMOUS_CAPABLE_FUNCTION_KINDS: [&str; 6] = [
    "function",
    "function_definition",
    "function_expression",
    "arrow_function",
    "lambda",
    "lambda_expression",
];

/// The node naming a declaration. An anonymous function value is named only
/// by its own identifier-shaped `name` field: never by a parameter, by a bare
/// identifier body (`function(v) v`), or by a keyword token (R stores
/// `function` itself in the `name` field).
fn declared_name_node(node: Node<'_>) -> Option<Node<'_>> {
    // C-family definitions (Objective-C) name the function through their
    // `declarator`, so only declarator-free function values can be anonymous.
    if ANONYMOUS_CAPABLE_FUNCTION_KINDS.contains(&node.kind())
        && node.child_by_field_name("declarator").is_none()
    {
        return node
            .child_by_field_name("name")
            .filter(|name| is_name_node(name.kind()));
    }
    find_name_node(node, 0)
}

fn declaration_kind(node_kind: &str) -> Option<SymbolKind> {
    EXACT_DECLARATION_RULES
        .iter()
        .find(|rule| rule.node_kinds.contains(&node_kind))
        .map(|rule| rule.kind)
        .or_else(|| heuristic_declaration_kind(node_kind))
}

struct ExactDeclarationRule {
    node_kinds: &'static [&'static str],
    kind: SymbolKind,
}

const EXACT_DECLARATION_RULES: &[ExactDeclarationRule] = &[
    ExactDeclarationRule {
        node_kinds: &[
            "function",
            "function_declaration",
            "function_definition",
            "function_item",
            "function_statement",
            "function_or_value_defn",
            "function_binding",
            "procedure_declaration",
            "procedure_definition",
            "defProc",
            "subroutine",
            "constructor_declaration",
            "destructor_declaration",
            "macro_declaration",
            "macro_definition",
        ],
        kind: SymbolKind::Function,
    },
    ExactDeclarationRule {
        node_kinds: &[
            "method",
            "singleton_method",
            "method_declaration",
            "method_definition",
            "method_implementation",
            "method_block",
            "method_statement",
            "constructor_definition",
        ],
        kind: SymbolKind::Method,
    },
    ExactDeclarationRule {
        node_kinds: &[
            "class",
            "class_declaration",
            "class_definition",
            "class_block",
            "class_interface",
            "class_implementation",
            "object_declaration",
            "object_definition",
            "contract_declaration",
        ],
        kind: SymbolKind::Class,
    },
    ExactDeclarationRule {
        node_kinds: &[
            "struct_declaration",
            "struct_definition",
            "structure_declaration",
            "record_declaration",
            "record_definition",
            "model_declaration",
        ],
        kind: SymbolKind::Struct,
    },
    ExactDeclarationRule {
        node_kinds: &[
            "interface_declaration",
            "interface_definition",
            "protocol_declaration",
            "trait_declaration",
            "trait_definition",
            "object_type_definition",
        ],
        kind: SymbolKind::Interface,
    },
    ExactDeclarationRule {
        node_kinds: &[
            "enum",
            "enum_declaration",
            "enum_definition",
            "enum_type_definition",
        ],
        kind: SymbolKind::Enum,
    },
    ExactDeclarationRule {
        node_kinds: &["enum_member", "enum_value_definition"],
        kind: SymbolKind::EnumMember,
    },
    ExactDeclarationRule {
        node_kinds: &[
            "module",
            "program",
            "module_declaration",
            "module_definition",
            "module_block",
            "namespace_declaration",
            "namespace_definition",
            "unit_declaration",
            "program_declaration",
            "package_declaration",
        ],
        kind: SymbolKind::Module,
    },
    ExactDeclarationRule {
        node_kinds: &[
            "type_alias",
            "type_alias_declaration",
            "type_declaration",
            "type_definition",
            "alias_declaration",
        ],
        kind: SymbolKind::TypeAlias,
    },
    ExactDeclarationRule {
        node_kinds: &[
            "field",
            "field_declaration",
            "field_definition",
            "property_declaration",
            "property_definition",
            "property_statement",
        ],
        kind: SymbolKind::Property,
    },
    ExactDeclarationRule {
        node_kinds: &[
            "constant",
            "const_declaration",
            "constant_declaration",
            "constant_definition",
        ],
        kind: SymbolKind::Constant,
    },
    ExactDeclarationRule {
        node_kinds: &[
            "variable_declaration",
            "variable_definition",
            "let_declaration",
            "value_declaration",
            "binding",
        ],
        kind: SymbolKind::Variable,
    },
    ExactDeclarationRule {
        node_kinds: &[
            "event_definition",
            "event_declaration",
            "schema_declaration",
            "schema_definition",
            "resource_declaration",
        ],
        kind: SymbolKind::Resource,
    },
    ExactDeclarationRule {
        node_kinds: &[
            "table_declaration",
            "create_table_statement",
            "create_table",
        ],
        kind: SymbolKind::Table,
    },
    ExactDeclarationRule {
        node_kinds: &["component_declaration"],
        kind: SymbolKind::Component,
    },
];

struct HeuristicDeclarationRule {
    fragments: &'static [&'static str],
    kind: SymbolKind,
}

const DECLARATION_SUFFIXES: [&str; 4] = ["_declaration", "_definition", "_defn", "_block"];
const HEURISTIC_DECLARATION_RULES: [HeuristicDeclarationRule; 11] = [
    HeuristicDeclarationRule {
        fragments: &["function", "procedure"],
        kind: SymbolKind::Function,
    },
    HeuristicDeclarationRule {
        fragments: &["method", "constructor"],
        kind: SymbolKind::Method,
    },
    HeuristicDeclarationRule {
        fragments: &["class", "contract"],
        kind: SymbolKind::Class,
    },
    HeuristicDeclarationRule {
        fragments: &["interface", "protocol"],
        kind: SymbolKind::Interface,
    },
    HeuristicDeclarationRule {
        fragments: &["struct", "record"],
        kind: SymbolKind::Struct,
    },
    HeuristicDeclarationRule {
        fragments: &["enum"],
        kind: SymbolKind::Enum,
    },
    HeuristicDeclarationRule {
        fragments: &["module", "namespace"],
        kind: SymbolKind::Module,
    },
    HeuristicDeclarationRule {
        fragments: &["type", "alias"],
        kind: SymbolKind::TypeAlias,
    },
    HeuristicDeclarationRule {
        fragments: &["field", "property"],
        kind: SymbolKind::Property,
    },
    HeuristicDeclarationRule {
        fragments: &["constant"],
        kind: SymbolKind::Constant,
    },
    HeuristicDeclarationRule {
        fragments: &["variable", "value"],
        kind: SymbolKind::Variable,
    },
];

fn heuristic_declaration_kind(node_kind: &str) -> Option<SymbolKind> {
    if !DECLARATION_SUFFIXES
        .iter()
        .any(|suffix| node_kind.ends_with(suffix))
    {
        return None;
    }
    HEURISTIC_DECLARATION_RULES
        .iter()
        .find(|rule| {
            rule.fragments
                .iter()
                .any(|fragment| node_kind.contains(fragment))
        })
        .map(|rule| rule.kind)
}

fn textual_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<(SymbolKind, String)>, ExtractError> {
    let language = builder.context.snapshot.language();
    let text = builder.context.text(node).trim_start();
    let candidate = match language {
        SourceLanguage::R if matches!(node.kind(), "left_assignment" | "binary_operator") => {
            r_declaration(text)
        }
        SourceLanguage::Sql if node.kind().contains("create") || starts_keyword(text, "create") => {
            sql_declaration(text)
        }
        SourceLanguage::Nix if matches!(node.kind(), "binding" | "attrpath_value") => {
            assignment_declaration(text, SymbolKind::Property)
        }
        SourceLanguage::Yaml if node.kind().contains("mapping_pair") => {
            assignment_declaration(text, SymbolKind::Property)
        }
        SourceLanguage::Html | SourceLanguage::Astro if node.kind().contains("element") => {
            markup_declaration(text)
        }
        _ => None,
    };
    let candidate = candidate.filter(|(_, name)| {
        !crate::framework_salesforce::is_lwc_use(builder.context.snapshot.path().as_str(), name)
    });
    candidate
        .map(|(kind, name)| builder.context.copy_text(&name).map(|name| (kind, name)))
        .transpose()
}

fn r_declaration(text: &str) -> Option<(SymbolKind, String)> {
    let (left, right) = text.split_once("<-").or_else(|| text.split_once('='))?;
    if !right.trim_start().starts_with("function") {
        return None;
    }
    normalize_name(left.trim()).map(|name| (SymbolKind::Function, name))
}

fn sql_declaration(text: &str) -> Option<(SymbolKind, String)> {
    let mut tokens = text
        .split(|character: char| character.is_whitespace() || matches!(character, '(' | ';'))
        .filter(|token| !token.is_empty());
    if !tokens.next()?.eq_ignore_ascii_case("create") {
        return None;
    }
    let mut object = tokens.next()?;
    if object.eq_ignore_ascii_case("or") {
        if !tokens.next()?.eq_ignore_ascii_case("replace") {
            return None;
        }
        object = tokens.next()?;
    }
    let kind = if object.eq_ignore_ascii_case("table") || object.eq_ignore_ascii_case("view") {
        SymbolKind::Table
    } else if object.eq_ignore_ascii_case("function") || object.eq_ignore_ascii_case("procedure") {
        SymbolKind::Function
    } else if object.eq_ignore_ascii_case("schema")
        || object.eq_ignore_ascii_case("database")
        || object.eq_ignore_ascii_case("trigger")
    {
        SymbolKind::Resource
    } else {
        return None;
    };
    normalize_name(tokens.next()?).map(|name| (kind, name))
}

fn assignment_declaration(text: &str, kind: SymbolKind) -> Option<(SymbolKind, String)> {
    let delimiter = if text.contains('=') { '=' } else { ':' };
    let (name, _) = text.split_once(delimiter)?;
    normalize_name(name.trim()).map(|name| (kind, name))
}

fn markup_declaration(text: &str) -> Option<(SymbolKind, String)> {
    let start = text.find('<')?.saturating_add(1);
    let tail = text.get(start..)?.trim_start_matches('/');
    let end = tail
        .find(|character: char| character.is_whitespace() || matches!(character, '>' | '/'))
        .unwrap_or(tail.len());
    let name = normalize_name(tail.get(..end)?)?;
    (name.contains('-') || name.chars().next().is_some_and(char::is_uppercase))
        .then_some((SymbolKind::Component, name))
}

fn find_name_node(node: Node<'_>, depth: usize) -> Option<Node<'_>> {
    if depth > NAME_SEARCH_DEPTH {
        return None;
    }
    name_from_fields(node, depth)
        .or_else(|| direct_name_child(node))
        .or_else(|| nested_name_child(node, depth))
}

fn name_from_fields(node: Node<'_>, depth: usize) -> Option<Node<'_>> {
    for field in [
        "name",
        "identifier",
        "declarator",
        "path",
        "key",
        "property",
        "type",
    ] {
        if let Some(candidate) = node.child_by_field_name(field) {
            if is_name_node(candidate.kind()) {
                return Some(candidate);
            }
            if let Some(found) = find_name_node(candidate, depth.saturating_add(1)) {
                return Some(found);
            }
        }
    }
    None
}

fn direct_name_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| is_name_node(child.kind()))
}

fn nested_name_child(node: Node<'_>, depth: usize) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_name_node(child, depth.saturating_add(1)) {
            return Some(found);
        }
    }
    None
}

fn is_name_node(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "name"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "constant"
            | "simple_identifier"
            | "bare_key"
            | "word"
            | "symbol"
            | "atom"
            | "namespace_name"
            | "class_name"
            | "method_name"
            | "function_name"
    ) || kind.ends_with("_identifier")
        || kind.ends_with("_name")
}

fn normalize_name(value: &str) -> Option<String> {
    let value = value.trim().trim_matches(|character: char| {
        matches!(
            character,
            '"' | '\'' | '`' | ':' | ',' | ';' | '(' | ')' | '{' | '}'
        )
    });
    if value.is_empty()
        || value.len() > 512
        || specifier_may_carry_credential(value)
        || value.chars().any(|character| {
            !(character.is_alphanumeric()
                || matches!(character, '_' | '$' | ':' | '.' | '-' | '/' | '@' | '#'))
        })
    {
        None
    } else {
        Some(value.to_owned())
    }
}

fn capture_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let target = ["function", "callee", "method", "name", "command"]
        .into_iter()
        .find_map(|field| node.child_by_field_name(field))
        .or_else(|| {
            let mut cursor = node.walk();
            node.named_children(&mut cursor).next()
        });
    let Some(target) = target else {
        return Ok(());
    };
    let name = builder.context.owned_text(target)?;
    let Some(name) = normalize_reference_name(&name) else {
        return Ok(());
    };
    let reference = super::PendingReference {
        owner: builder.owners.last().cloned(),
        name,
        kind: ReferenceKind::Calls,
        node: target,
    };
    super::references::push_reference(builder, reference)
}

fn capture_named_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    kind: ReferenceKind,
) -> Result<(), ExtractError> {
    let Some(target) = find_name_node(node, 0) else {
        return Ok(());
    };
    let name = builder.context.owned_text(target)?;
    let Some(name) = normalize_reference_name(&name) else {
        return Ok(());
    };
    builder.emit_reference(ExtractedReference {
        owner: builder.owners.last().cloned(),
        name,
        resolution_name: None,
        kind,
        span: span_for(target)?,
    })
}

fn capture_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let target = ["source", "module", "path", "argument", "name"]
        .into_iter()
        .find_map(|field| node.child_by_field_name(field))
        .or_else(|| find_import_target(node, 0));
    let Some(target) = target else {
        return Ok(());
    };
    let raw = builder.context.owned_text(target)?;
    let Some(module) =
        normalize_module_specifier(&raw).filter(|module| !specifier_may_carry_credential(module))
    else {
        return Ok(());
    };
    let span = span_for(target)?;
    builder.emit_reference(ExtractedReference {
        owner: builder.owners.last().cloned(),
        name: module.clone(),
        resolution_name: None,
        kind: ReferenceKind::Imports,
        span,
    })?;
    emit_import_symbol(builder, node, &module)?;
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: module,
        imported_name: "*".to_owned(),
        local_name: "*".to_owned(),
        span,
    })
}

/// One `Import` declaration per recognized import, named by its module, so
/// file import listings see it. The raw statement is not retained.
fn emit_import_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    module: &str,
) -> Result<(), ExtractError> {
    builder
        .emit_symbol(PendingSymbol {
            kind: SymbolKind::Import,
            name: builder.context.copy_text(module)?,
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: None,
            declaration_only: false,
            signature: None,
            export: crate::SymbolExportFlags::named(false),
            async_symbol: false,
            static_member: false,
            visibility: None,
        })
        .map(drop)
}

fn find_import_target(node: Node<'_>, depth: usize) -> Option<Node<'_>> {
    if depth > NAME_SEARCH_DEPTH {
        return None;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind().contains("string")
            || is_name_node(child.kind())
            || matches!(
                child.kind(),
                "scoped_identifier" | "dotted_name" | "namespace_identifier"
            )
        {
            return Some(child);
        }
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find_map(|child| find_import_target(child, depth.saturating_add(1)))
}

pub(super) fn normalize_reference_name(value: &str) -> Option<String> {
    let value = value.trim();
    let head = value
        .split(['(', '[', '{', ' ', '\t', '\n'])
        .next()
        .unwrap_or_default();
    normalize_name(head)
}

fn normalize_module_specifier(value: &str) -> Option<String> {
    normalize_name(
        value
            .trim()
            .trim_matches(|character| matches!(character, '"' | '\'' | '`')),
    )
}

fn generic_visibility(node: Node<'_>, source: &str) -> Option<Visibility> {
    for (token, visibility) in [
        ("public", Visibility::Public),
        ("private", Visibility::Private),
        ("protected", Visibility::Protected),
        ("internal", Visibility::Internal),
    ] {
        if source_prefix_contains(node, source, token) {
            return Some(visibility);
        }
    }
    None
}

#[derive(Clone, Copy)]
struct GenericExportInput<'a> {
    language: SourceLanguage,
    top_level: bool,
    visibility: Option<Visibility>,
    name: &'a str,
    node: Node<'a>,
    source: &'a str,
}

fn generic_exported(input: GenericExportInput<'_>) -> bool {
    let GenericExportInput {
        language,
        top_level,
        visibility,
        name,
        node,
        source,
    } = input;
    if visibility == Some(Visibility::Private) || name.starts_with('_') {
        return false;
    }
    if visibility == Some(Visibility::Public)
        || ["pub", "export", "external"]
            .into_iter()
            .any(|token| source_prefix_contains(node, source, token))
    {
        return true;
    }
    top_level
        && matches!(
            language,
            SourceLanguage::Dart
                | SourceLanguage::GraphQl
                | SourceLanguage::Html
                | SourceLanguage::Khn
                | SourceLanguage::Lua
                | SourceLanguage::Luau
                | SourceLanguage::Php
                | SourceLanguage::Prisma
                | SourceLanguage::R
                | SourceLanguage::Ruby
                | SourceLanguage::Solidity
                | SourceLanguage::Sql
                | SourceLanguage::Yaml
        )
}

fn source_prefix_contains(node: Node<'_>, source: &str, token: &str) -> bool {
    let end = node
        .end_byte()
        .min(node.start_byte().saturating_add(PREFIX_SCAN_BYTES));
    let Some(prefix) = source.get(node.start_byte()..end) else {
        return false;
    };
    prefix
        .split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .any(|candidate| candidate.eq_ignore_ascii_case(token))
}

fn is_definition_node(kind: &str) -> bool {
    kind.ends_with("_definition")
        || kind.ends_with("_block")
        || matches!(
            kind,
            "function"
                | "method"
                | "singleton_method"
                | "class"
                | "module"
                | "class_implementation"
                | "method_implementation"
                | "function_item"
        )
}

fn is_callable(kind: SymbolKind) -> bool {
    matches!(kind, SymbolKind::Function | SymbolKind::Method)
}

fn is_import_node(kind: &str) -> bool {
    matches!(
        kind,
        "import_declaration"
            | "import_statement"
            | "import_directive"
            | "use_declaration"
            | "use_statement"
            | "include_statement"
            | "require_expression"
            | "include_expression"
    ) || (kind.contains("import")
        && (kind.ends_with("_declaration") || kind.ends_with("_statement")))
}

fn is_call_node(kind: &str) -> bool {
    matches!(
        kind,
        "call"
            | "call_expression"
            | "function_call"
            | "function_call_expression"
            | "method_invocation"
            | "invocation_expression"
            | "command_call"
            | "send"
    )
}

fn is_inheritance_node(kind: &str) -> bool {
    matches!(
        kind,
        "superclass"
            | "base_list"
            | "extends_clause"
            | "implements_clause"
            | "inheritance_specifier"
            | "super_interfaces"
    )
}

fn should_skip_markup_symbol(language: SourceLanguage, kind: SymbolKind, name: &str) -> bool {
    matches!(language, SourceLanguage::Html | SourceLanguage::Astro)
        && kind == SymbolKind::Component
        && !name.contains('-')
        && !name.chars().next().is_some_and(char::is_uppercase)
}

fn starts_keyword(value: &str, keyword: &str) -> bool {
    value
        .get(..keyword.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(keyword))
        && value
            .get(keyword.len()..)
            .and_then(|tail| tail.chars().next())
            .is_none_or(|character| !character.is_alphanumeric() && character != '_')
}
