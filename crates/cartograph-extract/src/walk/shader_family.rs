//! WGSL shader declarations, references, and `naga_oil` module imports.
//!
//! Shaders are where a large share of a renderer's logic lives, and they are
//! exactly the code that is hardest to navigate by text search. Entry points are
//! the top of a real call stack that otherwise stops at the host-language
//! boundary, and `@group`/`@binding` declarations are a contract between host
//! code and shader code.
//!
//! Stage attributes are retained as an explicit symbol role rather than being
//! flattened into "some function", so `entry-points` can report a vertex,
//! fragment, or compute entry as what it is.

use cartograph_domain::{ReferenceKind, SourceLanguage, SourceSpan, SymbolKind, Visibility};
use tree_sitter::Node;

use super::{
    ExtractionBuilder, PendingReference, PendingSymbol, references, syntax::named_children,
};
use crate::source_lines::{LineMap, SourceByteRange};
use crate::{ExtractError, SymbolExportFlags};
use crate::{ExtractedImportBinding, ExtractedReference, ImportBindingKind};

/// Attribute names that make a WGSL function a pipeline entry point.
const STAGE_ATTRIBUTES: [&str; 3] = ["vertex", "fragment", "compute"];
const MAXIMUM_WESL_IMPORT_STATEMENTS: usize = 256;
const MAXIMUM_WESL_IMPORT_BYTES: usize = 4_096;
const MAXIMUM_WESL_IMPORT_PATHS: usize = 512;

pub(super) fn collect_wesl_imports(
    builder: &mut ExtractionBuilder<'_, '_>,
) -> Result<(), ExtractError> {
    if builder.context.snapshot.language() != SourceLanguage::Wesl {
        return Ok(());
    }
    let imports = parse_wesl_imports(builder.context.source())?;
    for (span, module_specifier, local_name) in imports {
        builder.emit_reference(ExtractedReference {
            owner: None,
            name: module_specifier.clone(),
            resolution_name: None,
            kind: ReferenceKind::Imports,
            span,
        })?;
        builder.emit_import_binding(ExtractedImportBinding {
            kind: ImportBindingKind::Namespace,
            module_specifier,
            imported_name: "*".to_owned(),
            local_name,
            span,
        })?;
    }
    Ok(())
}

struct WeslImportStatement<'source> {
    start: usize,
    end: usize,
    payload: &'source str,
}

fn parse_wesl_imports(source: &str) -> Result<Vec<(SourceSpan, String, String)>, ExtractError> {
    let line_map = LineMap::new(source)?;
    let mut imports = Vec::new();
    imports
        .try_reserve_exact(MAXIMUM_WESL_IMPORT_PATHS)
        .map_err(|_| ExtractError::OutputLimit)?;
    let mut cursor = 0_usize;
    let mut statements = 0_usize;
    while cursor < source.len()
        && statements < MAXIMUM_WESL_IMPORT_STATEMENTS
        && imports.len() < MAXIMUM_WESL_IMPORT_PATHS
    {
        let Some(statement) = next_wesl_import_statement(source, cursor) else {
            break;
        };
        let span = line_map.span(SourceByteRange::new(
            statement.start,
            statement.end,
            source.len(),
        ))?;
        append_wesl_import_paths(statement.payload, span, &mut imports)?;
        statements = statements.saturating_add(1);
        cursor = statement.end;
    }
    Ok(imports)
}

fn next_wesl_import_statement(source: &str, cursor: usize) -> Option<WeslImportStatement<'_>> {
    let mut search = cursor;
    while search < source.len() {
        let start = find_wesl_import_start(source, search)?;
        let head = wesl_import_head(source, start)?;
        let payload_start = skip_wesl_trivia(source, head + "import".len());
        if let Some(end) = wesl_import_statement_end(source, payload_start) {
            return Some(WeslImportStatement {
                start,
                end,
                payload: &source[payload_start..end - 1],
            });
        }
        search = head.saturating_add("import".len());
    }
    None
}

fn wesl_import_head(source: &str, start: usize) -> Option<usize> {
    if wesl_keyword_at(source, start, "import") {
        return Some(start);
    }
    if !wesl_keyword_at(source, start, "public") {
        return None;
    }
    let head = skip_wesl_trivia(source, start + "public".len());
    wesl_keyword_at(source, head, "import").then_some(head)
}

fn find_wesl_import_start(source: &str, mut cursor: usize) -> Option<usize> {
    let mut nesting = 0_usize;
    while cursor < source.len() {
        if source[cursor..].starts_with("//") {
            cursor = skip_wesl_line_comment(source, cursor);
            continue;
        }
        if source[cursor..].starts_with("/*") {
            cursor = skip_wesl_block_comment(source, cursor);
            continue;
        }
        match source.as_bytes()[cursor] {
            b'"' | b'\'' => {
                cursor = skip_wesl_quoted(source, cursor);
                continue;
            }
            b'{' | b'(' | b'[' => nesting = nesting.saturating_add(1),
            b'}' | b')' | b']' => nesting = nesting.saturating_sub(1),
            _ if nesting == 0
                && (wesl_keyword_at(source, cursor, "import")
                    || (wesl_keyword_at(source, cursor, "public")
                        && wesl_import_head(source, cursor).is_some())) =>
            {
                return Some(cursor);
            }
            _ => {}
        }
        cursor = next_wesl_cursor(source, cursor);
    }
    None
}

fn wesl_import_statement_end(source: &str, payload_start: usize) -> Option<usize> {
    let limit = payload_start
        .saturating_add(MAXIMUM_WESL_IMPORT_BYTES)
        .min(source.len());
    let mut cursor = payload_start;
    let mut brace_depth = 0_usize;
    while cursor < limit {
        if source[cursor..].starts_with("//") {
            cursor = skip_wesl_line_comment(source, cursor);
            continue;
        }
        if source[cursor..].starts_with("/*") {
            cursor = skip_wesl_block_comment(source, cursor);
            continue;
        }
        match source.as_bytes()[cursor] {
            b'"' | b'\'' => {
                cursor = skip_wesl_quoted(source, cursor);
                continue;
            }
            b'{' => brace_depth = brace_depth.saturating_add(1),
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            b';' if brace_depth == 0 => return Some(cursor + 1),
            _ => {}
        }
        cursor = next_wesl_cursor(source, cursor);
    }
    None
}

fn wesl_keyword_at(source: &str, cursor: usize, keyword: &str) -> bool {
    source
        .get(cursor..)
        .is_some_and(|suffix| suffix.starts_with(keyword))
        && (cursor == 0
            || source
                .as_bytes()
                .get(cursor - 1)
                .is_none_or(|byte| !wesl_identifier_byte(*byte)))
        && source
            .as_bytes()
            .get(cursor + keyword.len())
            .is_none_or(|byte| !wesl_identifier_byte(*byte))
}

const fn wesl_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn next_wesl_cursor(source: &str, cursor: usize) -> usize {
    source[cursor..]
        .chars()
        .next()
        .map_or(source.len(), |character| cursor + character.len_utf8())
}

fn skip_wesl_line_comment(source: &str, cursor: usize) -> usize {
    source[cursor..]
        .find('\n')
        .map_or(source.len(), |offset| cursor + offset + 1)
}

fn skip_wesl_block_comment(source: &str, cursor: usize) -> usize {
    let mut cursor = cursor.saturating_add(2);
    let mut depth = 1_usize;
    while cursor < source.len() {
        if source[cursor..].starts_with("/*") {
            depth = depth.saturating_add(1);
            cursor = cursor.saturating_add(2);
            continue;
        }
        if source[cursor..].starts_with("*/") {
            depth = depth.saturating_sub(1);
            cursor = cursor.saturating_add(2);
            if depth == 0 {
                return cursor;
            }
            continue;
        }
        cursor = next_wesl_cursor(source, cursor);
    }
    source.len()
}

fn skip_wesl_quoted(source: &str, cursor: usize) -> usize {
    let quote = source.as_bytes()[cursor];
    let mut cursor = cursor.saturating_add(1);
    while cursor < source.len() {
        match source.as_bytes()[cursor] {
            b'\\' => {
                cursor = cursor.saturating_add(1);
                if cursor < source.len() {
                    cursor = next_wesl_cursor(source, cursor);
                }
            }
            byte if byte == quote => return cursor.saturating_add(1),
            _ => cursor = next_wesl_cursor(source, cursor),
        }
    }
    source.len()
}

fn append_wesl_import_paths(
    payload: &str,
    span: SourceSpan,
    imports: &mut Vec<(SourceSpan, String, String)>,
) -> Result<(), ExtractError> {
    if super::specifier_safety::specifier_may_carry_credential(payload) {
        return Ok(());
    }
    let mut paths = Vec::new();
    flatten_wesl_imports(
        payload,
        WeslImportBranch {
            prefix: "",
            depth: 0,
        },
        &mut paths,
    )?;
    for raw in paths {
        if imports.len() >= MAXIMUM_WESL_IMPORT_PATHS {
            break;
        }
        let Some((module_specifier, local_name)) = normalize_wesl_import_path(&raw) else {
            continue;
        };
        imports.push((span, module_specifier, local_name));
    }
    Ok(())
}

fn normalize_wesl_import_path(raw: &str) -> Option<(String, String)> {
    let (path, alias) = split_wesl_alias(raw);
    let path = path.trim().trim_end_matches("::*").trim();
    if path.is_empty() || path.len() > 512 || !valid_wesl_path(path) {
        return None;
    }
    let local_name = alias.unwrap_or_else(|| path.rsplit("::").next().unwrap_or("*").to_owned());
    Some((path.to_owned(), local_name))
}

fn skip_wesl_whitespace(source: &str, mut cursor: usize) -> usize {
    while source
        .as_bytes()
        .get(cursor)
        .is_some_and(u8::is_ascii_whitespace)
    {
        cursor = cursor.saturating_add(1);
    }
    cursor
}

fn skip_wesl_trivia(source: &str, mut cursor: usize) -> usize {
    loop {
        cursor = skip_wesl_whitespace(source, cursor);
        if source[cursor..].starts_with("//") {
            cursor = skip_wesl_line_comment(source, cursor);
            continue;
        }
        if source[cursor..].starts_with("/*") {
            cursor = skip_wesl_block_comment(source, cursor);
            continue;
        }
        return cursor;
    }
}

#[derive(Clone, Copy)]
struct WeslImportBranch<'source> {
    prefix: &'source str,
    depth: usize,
}

fn flatten_wesl_imports(
    raw: &str,
    branch: WeslImportBranch<'_>,
    output: &mut Vec<String>,
) -> Result<(), ExtractError> {
    if branch.depth > 16 || output.len() >= MAXIMUM_WESL_IMPORT_PATHS {
        return Err(ExtractError::OutputLimit);
    }
    let raw = raw.trim().trim_end_matches(',').trim();
    let Some(open) = top_level_wesl_brace(raw) else {
        let joined = join_wesl_path(branch.prefix, raw)?;
        if !joined.is_empty() {
            output.push(joined);
        }
        return Ok(());
    };
    let Some(close) = matching_wesl_brace(raw, open) else {
        return Ok(());
    };
    let branch_prefix = raw[..open].trim().trim_end_matches("::");
    let joined_prefix = join_wesl_path(branch.prefix, branch_prefix)?;
    for member in split_top_level_wesl(&raw[open + 1..close]) {
        flatten_wesl_imports(
            member,
            WeslImportBranch {
                prefix: &joined_prefix,
                depth: branch.depth.saturating_add(1),
            },
            output,
        )?;
    }
    Ok(())
}

fn top_level_wesl_brace(raw: &str) -> Option<usize> {
    raw.char_indices()
        .find_map(|(offset, character)| (character == '{').then_some(offset))
}

fn matching_wesl_brace(raw: &str, open: usize) -> Option<usize> {
    let mut depth = 0_usize;
    for (relative, character) in raw[open..].char_indices() {
        match character {
            '{' => depth = depth.saturating_add(1),
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + relative);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_wesl(raw: &str) -> Vec<&str> {
    let mut members = Vec::new();
    let mut start = 0_usize;
    let mut depth = 0_usize;
    for (offset, character) in raw.char_indices() {
        match character {
            '{' => depth = depth.saturating_add(1),
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                members.push(&raw[start..offset]);
                start = offset + 1;
            }
            _ => {}
        }
    }
    members.push(&raw[start..]);
    members
}

fn join_wesl_path(prefix: &str, suffix: &str) -> Result<String, ExtractError> {
    let prefix = prefix.trim().trim_end_matches("::");
    let suffix = suffix.trim().trim_start_matches("::");
    let separator = if prefix.is_empty() || suffix.is_empty() {
        ""
    } else {
        "::"
    };
    let capacity = prefix
        .len()
        .checked_add(separator.len())
        .and_then(|value| value.checked_add(suffix.len()))
        .ok_or(ExtractError::OutputLimit)?;
    if capacity > 512 {
        return Err(ExtractError::OutputLimit);
    }
    let mut joined = String::new();
    joined
        .try_reserve_exact(capacity)
        .map_err(|_| ExtractError::OutputLimit)?;
    joined.push_str(prefix);
    joined.push_str(separator);
    joined.push_str(suffix);
    Ok(joined)
}

fn split_wesl_alias(raw: &str) -> (&str, Option<String>) {
    raw.rsplit_once(" as ")
        .map_or((raw, None), |(path, alias)| {
            (
                path,
                (!alias.trim().is_empty()).then(|| alias.trim().to_owned()),
            )
        })
}

fn valid_wesl_path(path: &str) -> bool {
    path.split("::").all(|component| {
        component == "*"
            || !component.is_empty()
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    })
}

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "function_declaration" => visit_function(builder, node, depth),
        "struct_declaration" => visit_struct(builder, node, depth),
        "global_variable_declaration" | "global_constant_declaration" => {
            visit_global(builder, node)
        }
        "type_alias_declaration" => visit_type_alias(builder, node),
        "preproc_import" => visit_import(builder, node).map(|()| true),
        "define_import_path" => visit_define_import_path(builder, node).map(|()| true),
        _ => Ok(false),
    }
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() != "type_constructor_or_function_call_expression" {
        return Ok(());
    }
    // The grammar cannot tell a call from a type construction, so the reference
    // stays a call with the exact spelled name and resolution decides later.
    let Some(target) = node
        .child_by_field_name("type")
        .or_else(|| named_children(node).find(|child| child.kind() == "type_declaration"))
    else {
        return Ok(());
    };
    let name = builder.context.owned_text(target)?;
    if name.is_empty() {
        return Ok(());
    }
    let owner = super::owner_for_node(builder, node);
    references::push_reference(
        builder,
        PendingReference {
            owner,
            name,
            kind: ReferenceKind::Calls,
            node,
        },
    )
}

fn visit_function(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(false);
    };
    let name = builder.context.owned_text(name_node)?;
    let stage = entry_point_stage(builder, node)?;
    let signature = function_signature(builder, node, stage.as_deref())?;
    let pending = PendingSymbol {
        kind: SymbolKind::Function,
        name,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: node.child_by_field_name("body"),
        declaration_only: false,
        signature: Some(signature),
        // A stage entry point is reachable from the host pipeline rather than
        // from shader code, so it is an exported boundary.
        export: SymbolExportFlags::new(stage.is_some(), false),
        async_symbol: false,
        static_member: false,
        visibility: Some(if stage.is_some() {
            Visibility::Public
        } else {
            Visibility::Internal
        }),
    };
    let symbol = builder.emit_symbol(pending)?;
    builder.owners.push(symbol);
    let result = builder.visit_named_children(node, depth);
    builder.owners.pop();
    result?;
    Ok(true)
}

fn visit_struct(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(false);
    };
    let name = builder.context.owned_text(name_node)?;
    let pending = PendingSymbol {
        kind: SymbolKind::Struct,
        name,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: SymbolExportFlags::new(true, false),
        async_symbol: false,
        static_member: false,
        visibility: Some(Visibility::Public),
    };
    let symbol = builder.emit_symbol(pending)?;
    builder.owners.push(symbol);
    let result = visit_struct_members(builder, node, depth);
    builder.owners.pop();
    result?;
    Ok(true)
}

fn visit_struct_members(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    for member in named_children(node) {
        builder.context.ensure_active()?;
        if member.kind() != "struct_member" {
            continue;
        }
        let Some(declaration) =
            named_children(member).find(|child| child.kind() == "variable_identifier_declaration")
        else {
            continue;
        };
        let Some(name_node) = declaration.child_by_field_name("name") else {
            continue;
        };
        let name = builder.context.owned_text(name_node)?;
        // Struct layouts are duplicated on both sides of the host boundary, so
        // members carry their declared type for pairing.
        let signature = declaration
            .child_by_field_name("type")
            .map(|node| builder.context.owned_text(node))
            .transpose()?;
        builder.emit_symbol(PendingSymbol {
            kind: SymbolKind::Field,
            name,
            span_node: member,
            structural_node: member,
            doc_anchor: member,
            body_node: None,
            declaration_only: true,
            signature,
            export: SymbolExportFlags::new(true, false),
            async_symbol: false,
            static_member: false,
            visibility: Some(Visibility::Public),
        })?;
        capture_member_type_reference(builder, declaration)?;
    }
    builder.visit_named_children(node, depth)
}

fn capture_member_type_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(type_node) = declaration.child_by_field_name("type") else {
        return Ok(());
    };
    let name = builder.context.owned_text(type_node)?;
    if name.is_empty() {
        return Ok(());
    }
    let owner = super::owner_for_node(builder, declaration);
    references::push_reference(
        builder,
        PendingReference {
            owner,
            name,
            kind: ReferenceKind::TypeOf,
            node: type_node,
        },
    )
}

fn visit_global(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let Some(declaration) = named_children(node).find_map(|child| match child.kind() {
        "variable_declaration" => {
            named_children(child).find(|inner| inner.kind() == "variable_identifier_declaration")
        }
        "variable_identifier_declaration" => Some(child),
        _ => None,
    }) else {
        return Ok(false);
    };
    let Some(name_node) = declaration.child_by_field_name("name") else {
        return Ok(false);
    };
    let name = builder.context.owned_text(name_node)?;
    // A bound resource is the shader half of a host-side layout entry, so the
    // binding attributes are retained in the signature.
    let signature = binding_signature(builder, node, declaration)?;
    builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Variable,
        name,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: true,
        signature,
        export: SymbolExportFlags::new(true, false),
        async_symbol: false,
        static_member: false,
        visibility: Some(Visibility::Public),
    })?;
    capture_member_type_reference(builder, declaration)?;
    Ok(true)
}

fn visit_type_alias(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let Some(name_node) = named_children(node).find(|child| child.kind() == "identifier") else {
        return Ok(false);
    };
    let name = builder.context.owned_text(name_node)?;
    builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::TypeAlias,
        name,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: true,
        signature: None,
        export: SymbolExportFlags::new(true, false),
        async_symbol: false,
        static_member: false,
        visibility: Some(Visibility::Public),
    })?;
    Ok(true)
}

/// `naga_oil` `#import "path"` forms the shader module graph.
fn visit_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if super::specifier_safety::specifier_may_carry_credential(builder.context.text(node)) {
        return Ok(());
    }
    let Some(path_node) = node.child_by_field_name("path") else {
        return Ok(());
    };
    let path = builder.context.owned_text(path_node)?;
    let module = path.trim_matches('"').to_owned();
    if module.is_empty() {
        return Ok(());
    }
    builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Import,
        name: module.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: true,
        signature: None,
        export: SymbolExportFlags::new(false, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })?;
    references::push_reference(
        builder,
        PendingReference {
            owner: None,
            name: module,
            kind: ReferenceKind::Imports,
            node,
        },
    )
}

/// `naga_oil` `#define_import_path` names the module other shaders import.
fn visit_define_import_path(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if super::specifier_safety::specifier_may_carry_credential(builder.context.text(node)) {
        return Ok(());
    }
    let Some(path_node) = node.child_by_field_name("path") else {
        return Ok(());
    };
    let module = builder.context.owned_text(path_node)?.trim().to_owned();
    if module.is_empty() {
        return Ok(());
    }
    builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Module,
        name: module,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: true,
        signature: None,
        export: SymbolExportFlags::new(true, false),
        async_symbol: false,
        static_member: false,
        visibility: Some(Visibility::Public),
    })?;
    Ok(())
}

/// Pipeline stage for a function carrying `@vertex`, `@fragment`, or `@compute`.
fn entry_point_stage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    for attribute in named_children(node).filter(|child| child.kind() == "attribute") {
        builder.context.ensure_active()?;
        for identifier in named_children(attribute).filter(|child| child.kind() == "identifier") {
            let name = builder.context.owned_text(identifier)?;
            if STAGE_ATTRIBUTES.contains(&name.as_str()) {
                return Ok(Some(name));
            }
        }
    }
    Ok(None)
}

/// Literal-free callable signature naming the stage and declared return type.
fn function_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    stage: Option<&str>,
) -> Result<String, ExtractError> {
    let mut signature = String::new();
    if let Some(stage) = stage {
        signature.push('@');
        signature.push_str(stage);
        signature.push(' ');
    }
    signature.push_str("fn");
    // A return type can carry `@location(0)`, and a literal-bearing signature is
    // rejected before persistence, so only the declared type is retained.
    if let Some(declared) = node.child_by_field_name("type").and_then(|returns| {
        named_children(returns).find(|child| child.kind() == "type_declaration")
    }) {
        let declared = builder.context.owned_text(declared)?;
        let declared = declared.trim();
        if !declared.is_empty() {
            signature.push_str(" -> ");
            signature.push_str(declared);
        }
    }
    Ok(signature)
}

/// Literal-free declaration signature for a module-scope binding.
///
/// The `@group(N)`/`@binding(N)` indices are deliberately not spelled here: a
/// literal-bearing signature is rejected before persistence, which would blank
/// the declared type as well. Correlating those indices with a host-side layout
/// entry needs a structured channel rather than a signature string.
fn binding_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    declaration: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let mut signature = String::new();
    if let Some(qualifier) = named_children(node)
        .find(|child| child.kind() == "variable_declaration")
        .and_then(|declaration| {
            named_children(declaration).find(|child| child.kind() == "variable_qualifier")
        })
    {
        let qualifier = builder.context.owned_text(qualifier)?;
        let qualifier = qualifier.trim();
        if !qualifier.is_empty() {
            signature.push_str("var");
            signature.push_str(qualifier);
        }
    }
    if let Some(declared) = declaration.child_by_field_name("type") {
        let declared = builder.context.owned_text(declared)?;
        let declared = declared.trim();
        if !declared.is_empty() {
            if !signature.is_empty() {
                signature.push_str(": ");
            }
            signature.push_str(declared);
        }
    }
    if signature.is_empty() {
        return Ok(None);
    }
    Ok(Some(signature))
}
