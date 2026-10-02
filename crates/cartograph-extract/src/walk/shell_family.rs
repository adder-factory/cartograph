use cartograph_domain::{
    ReferenceKind, SourceLanguage, SymbolKind, callable_signature_is_literal_free,
};
use tree_sitter::Node;

use crate::ExtractError;

use super::{
    ExtractionBuilder, PendingReference, PendingSymbol, references, safe_assignment_signature,
    syntax::{descendants_including_root, named_children},
};

const MAX_SIGNATURE_BYTES: usize = 512;

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match builder.context.snapshot.language() {
        SourceLanguage::Bash | SourceLanguage::Zsh => visit_bash_declaration(builder, node, depth),
        SourceLanguage::Fish => visit_fish_declaration(builder, node, depth),
        SourceLanguage::PowerShell => visit_powershell_declaration(builder, node, depth),
        _ => Err(ExtractError::UnsupportedLanguage),
    }
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match builder.context.snapshot.language() {
        SourceLanguage::Bash | SourceLanguage::Zsh | SourceLanguage::Fish
            if node.kind() == "command" =>
        {
            capture_command(builder, node)
        }
        SourceLanguage::PowerShell if node.kind() == "command" => {
            capture_powershell_command(builder, node)
        }
        SourceLanguage::PowerShell if node.kind() == "invokation_expression" => {
            capture_powershell_member_invocation(builder, node)
        }
        SourceLanguage::Bash
        | SourceLanguage::Zsh
        | SourceLanguage::Fish
        | SourceLanguage::PowerShell => Ok(()),
        _ => Err(ExtractError::UnsupportedLanguage),
    }
}

fn visit_bash_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "function_definition" => {
            let Some(name_node) = node.child_by_field_name("name") else {
                return Ok(false);
            };
            let name = builder.context.owned_text(name_node)?;
            let signature = safe_function_signature(builder, &format!("{name}()"))?;
            visit_scoped_symbol(
                builder,
                ScopedSymbol {
                    node,
                    depth,
                    kind: SymbolKind::Function,
                    name,
                    signature,
                    body: node.child_by_field_name("body"),
                },
            )?;
            Ok(true)
        }
        "declaration_command" => {
            visit_bash_declaration_command(builder, node)?;
            Ok(true)
        }
        "variable_assignment" => {
            visit_bash_assignment(builder, node, None)?;
            Ok(true)
        }
        "command" => match shell_command_name(builder, node)?.as_deref() {
            Some("source" | ".") => {
                visit_shell_source(builder, node)?;
                Ok(true)
            }
            _ => Ok(false),
        },
        _ => Ok(false),
    }
}

fn visit_bash_declaration_command(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let modifier = node
        .child(0)
        .map(|child| builder.context.text(child).trim().to_ascii_lowercase())
        .unwrap_or_default();
    if modifier == "local" {
        return Ok(());
    }
    for assignment in named_children(node).filter(|child| child.kind() == "variable_assignment") {
        visit_bash_assignment(builder, assignment, Some(modifier.as_str()))?;
    }
    Ok(())
}

fn visit_bash_assignment(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    modifier: Option<&str>,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let kind = if modifier == Some("readonly") {
        SymbolKind::Constant
    } else {
        SymbolKind::Variable
    };
    let signature = if let Some(value) = node.child_by_field_name("value") {
        safe_assignment_signature(builder, value)?
    } else {
        None
    };
    emit_leaf(
        builder,
        LeafSymbol {
            node,
            kind,
            name_node,
            signature,
            exported: modifier == Some("export"),
        },
    )
}

fn visit_fish_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    if node.kind() == "function_definition" {
        let Some(name_node) = node.child_by_field_name("name") else {
            return Ok(false);
        };
        let name = builder.context.owned_text(name_node)?;
        let signature = fish_function_signature(builder, node, &name)?;
        visit_scoped_symbol(
            builder,
            ScopedSymbol {
                node,
                depth,
                kind: SymbolKind::Function,
                name,
                signature,
                body: None,
            },
        )?;
        return Ok(true);
    }
    if node.kind() != "command" {
        return Ok(false);
    }
    match shell_command_name(builder, node)?.as_deref() {
        Some("source" | ".") => {
            visit_shell_source(builder, node)?;
            Ok(true)
        }
        Some("set") => {
            visit_fish_set(builder, node)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn fish_function_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    name: &str,
) -> Result<Option<String>, ExtractError> {
    let mut signature = builder.context.copy_text(name)?;
    for option in named_children(node).skip(1) {
        builder.context.ensure_active()?;
        if !matches!(
            option.kind(),
            "word" | "single_quote_string" | "double_quote_string"
        ) {
            break;
        }
        let raw = builder.context.text(option).trim();
        if !raw.starts_with('-') {
            continue;
        }
        let next_length = signature
            .len()
            .checked_add(1)
            .and_then(|length| length.checked_add(raw.len()))
            .ok_or(ExtractError::OutputLimit)?;
        if next_length > MAX_SIGNATURE_BYTES {
            break;
        }
        builder.context.budget.ensure_string_length(next_length)?;
        signature
            .try_reserve(1_usize.saturating_add(raw.len()))
            .map_err(|_| ExtractError::OutputLimit)?;
        signature.push(' ');
        signature.push_str(raw);
    }
    Ok(Some(signature))
}

fn visit_fish_set(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let mut exported = false;
    let mut name_node = None;
    let mut value_node = None;
    let mut seen_name = false;
    for argument in shell_command_arguments(node) {
        builder.context.ensure_active()?;
        let text = builder.context.text(argument).trim();
        if !seen_name && text.starts_with('-') {
            if fish_non_declaration_flag(text) {
                return Ok(());
            }
            exported |= matches!(text, "-x" | "--export" | "-U" | "--universal");
            continue;
        }
        if !seen_name {
            name_node = Some(argument);
            seen_name = true;
        } else if value_node.is_none() {
            value_node = Some(argument);
        }
    }
    let Some(name_node) = name_node else {
        return Ok(());
    };
    let signature = value_node
        .map(|value| safe_assignment_signature(builder, value))
        .transpose()?
        .flatten();
    emit_leaf(
        builder,
        LeafSymbol {
            node: name_node,
            kind: SymbolKind::Variable,
            name_node,
            signature,
            exported,
        },
    )
}

fn fish_non_declaration_flag(flag: &str) -> bool {
    matches!(
        flag,
        "-l" | "--local" | "-e" | "--erase" | "-q" | "--query" | "-S" | "--show"
    )
}

fn visit_powershell_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "class_statement" => {
            visit_powershell_scope(
                builder,
                PowerShellScope {
                    node,
                    depth,
                    name_kind: "simple_name",
                    kind: SymbolKind::Class,
                },
            )?;
            Ok(true)
        }
        "function_statement" => {
            visit_powershell_scope(
                builder,
                PowerShellScope {
                    node,
                    depth,
                    name_kind: "function_name",
                    kind: SymbolKind::Function,
                },
            )?;
            Ok(true)
        }
        "class_method_definition" => {
            visit_powershell_scope(
                builder,
                PowerShellScope {
                    node,
                    depth,
                    name_kind: "simple_name",
                    kind: SymbolKind::Method,
                },
            )?;
            Ok(true)
        }
        "enum_statement" => {
            visit_powershell_scope(
                builder,
                PowerShellScope {
                    node,
                    depth,
                    name_kind: "simple_name",
                    kind: SymbolKind::Enum,
                },
            )?;
            Ok(true)
        }
        "class_property_definition" => {
            visit_powershell_field(builder, node)?;
            Ok(true)
        }
        "enum_member" => {
            visit_powershell_leaf(
                builder,
                PowerShellLeaf {
                    node,
                    name_kind: "simple_name",
                    kind: SymbolKind::EnumMember,
                },
            )?;
            Ok(true)
        }
        "assignment_expression" if builder.native_owner_kinds.is_empty() => {
            visit_powershell_assignment(builder, node)?;
            Ok(true)
        }
        "command" => {
            let name = powershell_command_name(builder, node)?;
            if name.eq_ignore_ascii_case("using") {
                visit_powershell_using(builder, node)?;
                Ok(true)
            } else if powershell_control_name(&name) {
                Ok(true)
            } else {
                Ok(false)
            }
        }
        _ => Ok(false),
    }
}

#[derive(Clone, Copy)]
struct PowerShellScope<'tree> {
    node: Node<'tree>,
    depth: usize,
    name_kind: &'static str,
    kind: SymbolKind,
}

fn visit_powershell_scope(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: PowerShellScope<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = named_children(input.node).find(|child| child.kind() == input.name_kind)
    else {
        return builder.visit_named_children(input.node, input.depth);
    };
    let name = powershell_name(builder, name_node)?;
    visit_scoped_symbol(
        builder,
        ScopedSymbol {
            node: input.node,
            depth: input.depth,
            kind: input.kind,
            name,
            signature: None,
            body: None,
        },
    )
}

fn visit_powershell_field(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(variable) = named_children(node).find(|child| child.kind() == "variable") else {
        return Ok(());
    };
    let signature = named_children(node)
        .find(|child| child.kind() == "type_literal")
        .map(|type_node| safe_type_signature(builder, type_node))
        .transpose()?
        .flatten();
    let name = powershell_name(builder, variable)?;
    emit_named_leaf(
        builder,
        NamedLeafSymbol {
            node,
            kind: SymbolKind::Field,
            name,
            signature,
            exported: false,
        },
    )
}

#[derive(Clone, Copy)]
struct PowerShellLeaf<'tree> {
    node: Node<'tree>,
    name_kind: &'static str,
    kind: SymbolKind,
}

fn visit_powershell_leaf(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: PowerShellLeaf<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = named_children(input.node).find(|child| child.kind() == input.name_kind)
    else {
        return Ok(());
    };
    let name = powershell_name(builder, name_node)?;
    emit_named_leaf(
        builder,
        NamedLeafSymbol {
            node: input.node,
            kind: input.kind,
            name,
            signature: None,
            exported: false,
        },
    )
}

fn visit_powershell_assignment(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(left) =
        named_children(node).find(|child| child.kind() == "left_assignment_expression")
    else {
        return Ok(());
    };
    let Some(variable) = descendants_including_root(left).find(|child| child.kind() == "variable")
    else {
        return Ok(());
    };
    let name = powershell_name(builder, variable)?;
    let signature = node
        .child_by_field_name("value")
        .map(|value| safe_assignment_signature(builder, value))
        .transpose()?
        .flatten();
    emit_named_leaf(
        builder,
        NamedLeafSymbol {
            node,
            kind: SymbolKind::Variable,
            name,
            signature,
            exported: false,
        },
    )
}

fn visit_powershell_using(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let raw = builder.context.text(node).trim();
    let lower = raw.to_ascii_lowercase();
    for kind in ["module", "namespace", "assembly"] {
        let prefix = format!("using {kind} ");
        if !lower.starts_with(&prefix) {
            continue;
        }
        let raw_path = raw.get(prefix.len()..).unwrap_or_default().trim();
        let Some(path) = literal_shell_text(raw_path) else {
            return Ok(());
        };
        let module_name = builder.context.copy_text(path)?;
        return emit_import(
            builder,
            ImportSymbol {
                node,
                span_node: node,
                name: module_name,
            },
        );
    }
    Ok(())
}

fn visit_shell_source(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(argument) = shell_command_arguments(node).next() else {
        return Ok(());
    };
    let Some(path) = literal_shell_node(builder, argument) else {
        return Ok(());
    };
    let module_name = builder.context.copy_text(path)?;
    emit_import(
        builder,
        ImportSymbol {
            node,
            span_node: argument,
            name: module_name,
        },
    )
}

struct ImportSymbol<'tree> {
    node: Node<'tree>,
    span_node: Node<'tree>,
    name: String,
}

fn emit_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ImportSymbol<'_>,
) -> Result<(), ExtractError> {
    let signature = builder
        .context
        .copy_text(builder.context.text(input.node).trim())?;
    let pending = PendingSymbol {
        kind: SymbolKind::Import,
        name: input.name.clone(),
        span_node: input.node,
        structural_node: input.node,
        doc_anchor: input.node,
        body_node: None,
        declaration_only: false,
        signature: Some(signature),
        export: crate::SymbolExportFlags::new(false, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    };
    builder.emit_symbol(pending)?;
    references::push_reference(
        builder,
        PendingReference {
            owner: None,
            name: input.name,
            kind: ReferenceKind::Imports,
            node: input.span_node,
        },
    )
}

struct ScopedSymbol<'tree> {
    node: Node<'tree>,
    depth: usize,
    kind: SymbolKind,
    name: String,
    signature: Option<String>,
    body: Option<Node<'tree>>,
}

fn visit_scoped_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ScopedSymbol<'_>,
) -> Result<(), ExtractError> {
    let pending = PendingSymbol {
        kind: input.kind,
        name: input.name.clone(),
        span_node: input.node,
        structural_node: input.node,
        doc_anchor: input.node,
        body_node: input.body.or(Some(input.node)),
        declaration_only: false,
        signature: input.signature,
        export: crate::SymbolExportFlags::new(false, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    };
    let id = builder.emit_symbol(pending)?;
    builder.owners.push(id);
    builder.native_owner_kinds.push(input.kind);
    builder.qualifiers.push(input.name);
    let result = if let Some(body) = input.body {
        builder.visit(body, input.depth.saturating_add(1))
    } else {
        builder.visit_named_children(input.node, input.depth)
    };
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

struct LeafSymbol<'tree> {
    node: Node<'tree>,
    kind: SymbolKind,
    name_node: Node<'tree>,
    signature: Option<String>,
    exported: bool,
}

fn emit_leaf(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: LeafSymbol<'_>,
) -> Result<(), ExtractError> {
    let name = builder.context.owned_text(input.name_node)?;
    emit_named_leaf(
        builder,
        NamedLeafSymbol {
            node: input.node,
            kind: input.kind,
            name,
            signature: input.signature,
            exported: input.exported,
        },
    )
}

struct NamedLeafSymbol<'tree> {
    node: Node<'tree>,
    kind: SymbolKind,
    name: String,
    signature: Option<String>,
    exported: bool,
}

fn emit_named_leaf(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: NamedLeafSymbol<'_>,
) -> Result<(), ExtractError> {
    if input.name.is_empty() {
        return Ok(());
    }
    let pending = PendingSymbol {
        kind: input.kind,
        name: input.name,
        span_node: input.node,
        structural_node: input.node,
        doc_anchor: input.node,
        body_node: None,
        declaration_only: false,
        signature: input.signature,
        export: crate::SymbolExportFlags::new(input.exported, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    };
    builder.emit_symbol(pending).map(|_| ())
}

fn capture_command(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name) = shell_command_name(builder, node)? else {
        return Ok(());
    };
    emit_call(builder, node, name)
}

fn capture_powershell_command(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let name = powershell_command_name(builder, node)?;
    if name.is_empty() || powershell_control_name(&name) {
        return Ok(());
    }
    emit_call(builder, node, name)
}

fn capture_powershell_member_invocation(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(member) = descendants_including_root(node).find(|child| child.kind() == "member_name")
    else {
        return Ok(());
    };
    let name = powershell_name(builder, member)?;
    emit_call(builder, member, name)
}

fn emit_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    name: String,
) -> Result<(), ExtractError> {
    if name.is_empty() {
        return Ok(());
    }
    references::push_reference(
        builder,
        PendingReference {
            owner: builder.owners.last().cloned(),
            name,
            kind: ReferenceKind::Calls,
            node,
        },
    )
}

fn shell_command_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(name) = node
        .child_by_field_name("name")
        .or_else(|| named_children(node).next())
    else {
        return Ok(None);
    };
    let leaf = named_children(name).next().unwrap_or(name);
    builder.context.owned_text(leaf).map(Some)
}

fn shell_command_arguments(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    let mut seen_name = false;
    named_children(node).filter(move |_| {
        if seen_name {
            true
        } else {
            seen_name = true;
            false
        }
    })
}

fn powershell_command_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<String, ExtractError> {
    let name = node
        .child_by_field_name("command_name")
        .or_else(|| named_children(node).find(|child| child.kind() == "command_name"))
        .map(|name| builder.context.owned_text(name))
        .transpose()?
        .unwrap_or_default();
    builder.context.copy_text(name.trim_start_matches('$'))
}

fn powershell_control_name(name: &str) -> bool {
    [
        "begin",
        "break",
        "catch",
        "continue",
        "dynamicparam",
        "else",
        "elseif",
        "end",
        "exit",
        "finally",
        "for",
        "foreach",
        "if",
        "param",
        "process",
        "return",
        "switch",
        "throw",
        "trap",
        "try",
        "using",
        "while",
    ]
    .into_iter()
    .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn powershell_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<String, ExtractError> {
    let raw = builder.context.owned_text(node)?;
    builder
        .context
        .copy_text(raw.trim_start_matches('$').trim())
}

fn safe_function_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    raw: &str,
) -> Result<Option<String>, ExtractError> {
    if raw.len() > MAX_SIGNATURE_BYTES || !callable_signature_is_literal_free(raw) {
        return Ok(None);
    }
    builder.context.copy_text(raw).map(Some)
}

fn safe_type_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let raw = builder.context.text(node).trim();
    if raw.is_empty() || raw.len() > MAX_SIGNATURE_BYTES || !callable_signature_is_literal_free(raw)
    {
        return Ok(None);
    }
    builder.context.copy_text(raw).map(Some)
}

fn literal_shell_node<'source>(
    builder: &'source ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Option<&'source str> {
    let raw = builder.context.text(node).trim();
    literal_shell_text(raw)
}

fn literal_shell_text(raw: &str) -> Option<&str> {
    let unquoted = raw
        .strip_circumfix('"', '"')
        .or_else(|| raw.strip_circumfix('\'', '\''))
        .unwrap_or(raw);
    (!unquoted.is_empty()
        && !unquoted.contains('$')
        && !unquoted.contains('(')
        && !unquoted.contains('`'))
    .then_some(unquoted)
}
