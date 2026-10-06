//! HCL, Terraform, and `OpenTofu` structural extraction.
//!
//! Terraform has no functions or classes: its unit of structure is the block
//! `kind "label"... { body }`. Every top-level block becomes one symbol whose
//! qualified name is the exact Terraform address other blocks use to refer to
//! it (`var.x`, `local.x`, `module.x`, `data.TYPE.NAME`, `TYPE.NAME`, ...).
//! Attribute expressions emit `references` by that same address, so they
//! resolve through ordinary exact qualified-name lookup without any naming
//! heuristic.

use cartograph_domain::{ReferenceKind, SourcePosition, SourceSpan, SymbolId, SymbolKind};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedReference, SymbolExportFlags};

use super::{
    ExtractionBuilder, PendingSymbol,
    specifier_safety::specifier_may_carry_credential,
    syntax::{named_children, span_for, unquote},
};

/// Heads that look like addresses but name Terraform built-ins or pseudo
/// variables rather than declarations.
const RESERVED_HEADS: [&str; 8] = [
    "count",
    "each",
    "self",
    "path",
    "terraform",
    "null",
    "true",
    "false",
];
/// `data.TYPE.NAME` needs two attribute segments after its head.
const DATA_ADDRESS_SEGMENTS: usize = 2;
/// Every other address (`var.x`, `local.x`, `module.x`, `TYPE.NAME`) needs one.
const NAMED_ADDRESS_SEGMENTS: usize = 1;
/// Typed blocks (`resource`, `data`) need their type and name labels.
const TYPED_BLOCK_LABELS: usize = 2;
/// Longest block label retained as part of a declaration name.
const MAXIMUM_LABEL_BYTES: usize = 256;
/// Longest static module `source` retained as an import specifier.
const MAXIMUM_MODULE_SOURCE_BYTES: usize = 512;
/// Module-source query keys that select a revision rather than carry secrets.
const SAFE_SOURCE_QUERY_KEYS: [&str; 3] = ["ref", "depth", "archive"];
/// Visit the configuration root; every Terraform fact is derived from its
/// top-level blocks, so nothing else is handled by the generic walker.
pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    if node.kind() != "config_file" {
        return Ok(false);
    }
    let block_depth = depth.saturating_add(2);
    for body in named_children(node).filter(|child| child.kind() == "body") {
        for block in named_children(body).filter(|child| child.kind() == "block") {
            builder.context.ensure_active()?;
            visit_top_level_block(builder, NodeAt::new(block, block_depth))?;
        }
    }
    Ok(true)
}

/// A syntax node paired with its depth below the configuration root.
#[derive(Clone, Copy)]
struct NodeAt<'tree> {
    node: Node<'tree>,
    depth: usize,
}

impl<'tree> NodeAt<'tree> {
    const fn new(node: Node<'tree>, depth: usize) -> Self {
        Self { node, depth }
    }

    fn child(self, node: Node<'tree>) -> Self {
        Self::new(node, self.depth.saturating_add(1))
    }
}

/// The head keyword, static labels, and optional body of one block.
struct BlockHeader<'tree> {
    block: NodeAt<'tree>,
    kind: String,
    labels: Vec<String>,
    body: Option<Node<'tree>>,
}

/// The symbol a labelled top-level block declares.
struct BlockDeclaration {
    kind: SymbolKind,
    name: String,
    qualified_name: String,
}

fn visit_top_level_block(
    builder: &mut ExtractionBuilder<'_, '_>,
    block: NodeAt<'_>,
) -> Result<(), ExtractError> {
    let Some(header) = block_header(builder, block)? else {
        return Ok(());
    };
    match header.kind.as_str() {
        "locals" => visit_locals(builder, &header),
        "terraform" => {
            let declaration = BlockDeclaration {
                kind: SymbolKind::Module,
                name: "terraform".to_owned(),
                qualified_name: "terraform".to_owned(),
            };
            emit_block_symbol(builder, &header, declaration).map(drop)
        }
        _ => visit_labelled_block(builder, &header),
    }
}

fn block_header<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    block: NodeAt<'tree>,
) -> Result<Option<BlockHeader<'tree>>, ExtractError> {
    let Some(head) = named_children(block.node).find(|child| child.kind() == "identifier") else {
        return Ok(None);
    };
    let kind = builder.context.owned_text(head)?;
    let mut labels = Vec::new();
    for label in named_children(block.node).filter(|child| child.kind() == "string_lit") {
        if labels.len() == TYPED_BLOCK_LABELS {
            break;
        }
        let value = unquote(builder.context.text(label));
        if !is_terraform_label(value) {
            return Ok(None);
        }
        labels.push(builder.context.copy_text(value)?);
    }
    Ok(Some(BlockHeader {
        block,
        kind,
        labels,
        body: named_children(block.node).find(|child| child.kind() == "body"),
    }))
}

/// Terraform names are Unicode identifiers (including combining marks) that
/// may also contain `-`; `.` appears in provider-specific labels. ASCII
/// punctuation, whitespace, and control characters are never part of a name.
fn is_terraform_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAXIMUM_LABEL_BYTES
        && !specifier_may_carry_credential(value)
        && value.chars().all(|character| {
            character.is_alphanumeric()
                || matches!(character, '_' | '-' | '.')
                || !(character.is_ascii() || character.is_whitespace() || character.is_control())
        })
}

fn visit_labelled_block(
    builder: &mut ExtractionBuilder<'_, '_>,
    header: &BlockHeader<'_>,
) -> Result<(), ExtractError> {
    let Some(declaration) = labelled_declaration(builder, header)? else {
        return Ok(());
    };
    let module_block = header.kind == "module";
    let owner = emit_block_symbol(builder, header, declaration)?;
    let Some(body) = header.body else {
        return Ok(());
    };
    if module_block {
        emit_module_source(builder, body, &owner)?;
    }
    let mut scan = ReferenceScan::new(&owner);
    scan_node(builder, &mut scan, header.block.child(body))
}

fn labelled_declaration(
    builder: &ExtractionBuilder<'_, '_>,
    header: &BlockHeader<'_>,
) -> Result<Option<BlockDeclaration>, ExtractError> {
    let declaration = match header.kind.as_str() {
        "resource" | "data" => {
            let [resource_type, name] = header.labels.as_slice() else {
                return Ok(None);
            };
            let local_name = join_address(builder, &[resource_type.as_str(), name.as_str()])?;
            let qualified_name = if header.kind == "data" {
                join_address(builder, &["data", resource_type.as_str(), name.as_str()])?
            } else {
                builder.context.copy_text(&local_name)?
            };
            BlockDeclaration {
                kind: SymbolKind::Resource,
                name: local_name,
                qualified_name,
            }
        }
        kind => {
            let Some(label) = header.labels.first() else {
                return Ok(None);
            };
            let (symbol_kind, prefix) = named_block_shape(kind);
            BlockDeclaration {
                kind: symbol_kind,
                name: builder.context.copy_text(label)?,
                qualified_name: join_address(builder, &[prefix, label])?,
            }
        }
    };
    Ok(Some(declaration))
}

/// Symbol kind and address prefix of a single-label block. Unknown block
/// kinds (`check`, `moved`, provider-specific blocks) stay addressable as a
/// namespace under their own keyword.
fn named_block_shape(kind: &str) -> (SymbolKind, &str) {
    match kind {
        "module" => (SymbolKind::Module, "module"),
        "variable" => (SymbolKind::Variable, "var"),
        "output" => (SymbolKind::Export, "output"),
        "provider" => (SymbolKind::Namespace, "provider"),
        other => (SymbolKind::Namespace, other),
    }
}

fn emit_block_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    header: &BlockHeader<'_>,
    declaration: BlockDeclaration,
) -> Result<SymbolId, ExtractError> {
    let pending = PendingSymbol {
        kind: declaration.kind,
        name: declaration.name,
        span_node: header.block.node,
        structural_node: header.block.node,
        doc_anchor: header.block.node,
        body_node: header.body,
        declaration_only: false,
        signature: None,
        export: SymbolExportFlags::named(true),
        async_symbol: false,
        static_member: false,
        visibility: None,
    };
    builder.emit_symbol_with_qualified_name(pending, declaration.qualified_name)
}

/// `locals { a = ..; b = .. }` declares one constant per attribute.
fn visit_locals(
    builder: &mut ExtractionBuilder<'_, '_>,
    header: &BlockHeader<'_>,
) -> Result<(), ExtractError> {
    let Some(body) = header.body else {
        return Ok(());
    };
    for attribute in named_children(body).filter(|child| child.kind() == "attribute") {
        builder.context.ensure_active()?;
        let Some(name_node) = named_children(attribute).find(|child| child.kind() == "identifier")
        else {
            continue;
        };
        let name = builder.context.owned_text(name_node)?;
        let qualified_name = join_address(builder, &["local", &name])?;
        let expression = named_children(attribute).find(|child| child.kind() == "expression");
        let owner = builder.emit_symbol_with_qualified_name(
            PendingSymbol {
                kind: SymbolKind::Constant,
                name,
                span_node: attribute,
                structural_node: attribute,
                doc_anchor: attribute,
                body_node: expression,
                declaration_only: false,
                signature: None,
                export: SymbolExportFlags::named(true),
                async_symbol: false,
                static_member: false,
                visibility: None,
            },
            qualified_name,
        )?;
        if let Some(expression) = expression {
            let mut scan = ReferenceScan::new(&owner);
            let at = header.block.child(body).child(attribute).child(expression);
            scan_node(builder, &mut scan, at)?;
        }
    }
    Ok(())
}

/// Emit the `imports` edge of `module "x" { source = "..." }` when the source
/// is a static, credential-free string.
fn emit_module_source(
    builder: &mut ExtractionBuilder<'_, '_>,
    body: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    let Some(attribute) = named_children(body).find(|attribute| {
        attribute.kind() == "attribute"
            && named_children(*attribute)
                .find(|child| child.kind() == "identifier")
                .is_some_and(|name| builder.context.text(name) == "source")
    }) else {
        return Ok(());
    };
    let Some(expression) = named_children(attribute).find(|child| child.kind() == "expression")
    else {
        return Ok(());
    };
    let Some(source) = static_string(builder, expression)? else {
        return Ok(());
    };
    if !module_source_is_safe(&source) {
        return Ok(());
    }
    builder.emit_reference(ExtractedReference {
        owner: Some(owner.clone()),
        name: source,
        resolution_name: None,
        kind: ReferenceKind::Imports,
        span: span_for(expression)?,
    })
}

/// The literal text of a quoted string expression, or `None` when any part is
/// interpolated or the expression is not a string.
fn static_string(
    builder: &ExtractionBuilder<'_, '_>,
    expression: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(value) = expression.named_child(0) else {
        return Ok(None);
    };
    let container = match value.kind() {
        "literal_value" => named_children(value).find(|child| child.kind() == "string_lit"),
        "template_expr" => named_children(value).find(|child| child.kind() == "quoted_template"),
        _ => None,
    };
    let Some(container) = container else {
        return Ok(None);
    };
    let mut literal = String::new();
    for part in named_children(container) {
        match part.kind() {
            "template_literal" => {
                let text = builder.context.text(part);
                builder
                    .context
                    .budget
                    .ensure_string_length(literal.len().saturating_add(text.len()))?;
                literal
                    .try_reserve(text.len())
                    .map_err(|_| ExtractError::OutputLimit)?;
                literal.push_str(text);
            }
            "template_interpolation" | "template_directive" => return Ok(None),
            _ => {}
        }
    }
    Ok(Some(literal))
}

/// Accept registry, local, and VCS module sources while abstaining from any
/// that could carry a credential: the shared specifier screen (user info other
/// than the conventional `git` SSH user, provider-key-shaped segments, and
/// credential words or high-entropy tokens in URLs) plus non-revision query
/// parameters.
fn module_source_is_safe(source: &str) -> bool {
    if source.is_empty()
        || source.len() > MAXIMUM_MODULE_SOURCE_BYTES
        || !source.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'.' | b'_' | b'-' | b'/' | b':' | b'@' | b'~' | b'+' | b'?' | b'=' | b'&'
                )
        })
    {
        return false;
    }
    let (_, query) = source.split_once('?').unwrap_or((source, ""));
    query_is_revision_only(query) && !specifier_may_carry_credential(source)
}

fn query_is_revision_only(query: &str) -> bool {
    query.is_empty()
        || query.split('&').all(|pair| {
            pair.split_once('=')
                .is_some_and(|(key, _)| SAFE_SOURCE_QUERY_KEYS.contains(&key))
        })
}

/// Reference-scan state for one declaring block: its owner and the names
/// bound by enclosing `for` expressions and `dynamic` blocks.
struct ReferenceScan<'owner> {
    owner: &'owner SymbolId,
    bindings: Vec<String>,
}

impl<'owner> ReferenceScan<'owner> {
    const fn new(owner: &'owner SymbolId) -> Self {
        Self {
            owner,
            bindings: Vec::new(),
        }
    }

    fn binds(&self, name: &str) -> bool {
        self.bindings.iter().any(|binding| binding == name)
    }
}

fn scan_node(
    builder: &mut ExtractionBuilder<'_, '_>,
    scan: &mut ReferenceScan<'_>,
    at: NodeAt<'_>,
) -> Result<(), ExtractError> {
    builder.context.ensure_active()?;
    if at.depth > builder.maximum_ast_depth {
        return Err(ExtractError::NestingLimit);
    }
    match at.node.kind() {
        "for_tuple_expr" | "for_object_expr" => scan_for_expression(builder, scan, at),
        "block" => scan_nested_block(builder, scan, at),
        "expression" => {
            emit_address_reference(builder, scan, at.node)?;
            scan_children(builder, scan, at)
        }
        _ => scan_children(builder, scan, at),
    }
}

fn scan_children(
    builder: &mut ExtractionBuilder<'_, '_>,
    scan: &mut ReferenceScan<'_>,
    at: NodeAt<'_>,
) -> Result<(), ExtractError> {
    for child in named_children(at.node) {
        scan_node(builder, scan, at.child(child))?;
    }
    Ok(())
}

/// `[for k, v in xs : ...]` binds `k`/`v` for its body and condition; the
/// iterable itself is evaluated in the enclosing scope.
fn scan_for_expression(
    builder: &mut ExtractionBuilder<'_, '_>,
    scan: &mut ReferenceScan<'_>,
    at: NodeAt<'_>,
) -> Result<(), ExtractError> {
    let mark = scan.bindings.len();
    let result = scan_for_parts(builder, scan, at);
    scan.bindings.truncate(mark);
    result
}

fn scan_for_parts(
    builder: &mut ExtractionBuilder<'_, '_>,
    scan: &mut ReferenceScan<'_>,
    at: NodeAt<'_>,
) -> Result<(), ExtractError> {
    for child in named_children(at.node) {
        if child.kind() != "for_intro" {
            scan_node(builder, scan, at.child(child))?;
            continue;
        }
        let intro = at.child(child);
        for part in named_children(child).filter(|part| part.kind() == "expression") {
            scan_node(builder, scan, intro.child(part))?;
        }
        for binding in named_children(child).filter(|part| part.kind() == "identifier") {
            let name = builder.context.owned_text(binding)?;
            scan.bindings.push(name);
        }
    }
    Ok(())
}

/// Nested blocks declare nothing, but their attributes still reference other
/// blocks. A `dynamic "x"` block binds its iterator (`x`, or `iterator = y`)
/// only inside its `content` blocks; `for_each` sees the enclosing scope.
fn scan_nested_block(
    builder: &mut ExtractionBuilder<'_, '_>,
    scan: &mut ReferenceScan<'_>,
    at: NodeAt<'_>,
) -> Result<(), ExtractError> {
    let Some(iterator) = dynamic_iterator(builder, at.node)? else {
        return scan_children(builder, scan, at);
    };
    for child in named_children(at.node) {
        if child.kind() != "body" {
            scan_node(builder, scan, at.child(child))?;
            continue;
        }
        let body = at.child(child);
        for item in named_children(child) {
            if block_head_is(builder, item, "content") {
                let mark = scan.bindings.len();
                scan.bindings.push(builder.context.copy_text(&iterator)?);
                let result = scan_node(builder, scan, body.child(item));
                scan.bindings.truncate(mark);
                result?;
            } else {
                scan_node(builder, scan, body.child(item))?;
            }
        }
    }
    Ok(())
}

fn block_head_is(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>, keyword: &str) -> bool {
    node.kind() == "block"
        && named_children(node)
            .find(|child| child.kind() == "identifier")
            .is_some_and(|head| builder.context.text(head) == keyword)
}

fn dynamic_iterator(
    builder: &ExtractionBuilder<'_, '_>,
    block: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    if !block_head_is(builder, block, "dynamic") {
        return Ok(None);
    }
    let explicit = named_children(block)
        .filter(|child| child.kind() == "body")
        .flat_map(named_children)
        .filter(|child| child.kind() == "attribute")
        .find_map(|attribute| iterator_attribute(builder, attribute));
    let label = named_children(block)
        .find(|child| child.kind() == "string_lit")
        .map(|label| unquote(builder.context.text(label)));
    explicit
        .or(label)
        .filter(|name| is_terraform_label(name))
        .map(|name| builder.context.copy_text(name))
        .transpose()
}

fn iterator_attribute<'builder>(
    builder: &'builder ExtractionBuilder<'_, '_>,
    attribute: Node<'_>,
) -> Option<&'builder str> {
    let name = named_children(attribute).find(|child| child.kind() == "identifier")?;
    if builder.context.text(name) != "iterator" {
        return None;
    }
    let value = named_children(attribute).find(|child| child.kind() == "expression")?;
    let variable = value
        .named_child(0)
        .filter(|child| child.kind() == "variable_expr" && value.named_child_count() == 1)?;
    let identifier = named_children(variable).find(|child| child.kind() == "identifier")?;
    Some(builder.context.text(identifier))
}

/// Emit the Terraform address an `expression` starts with, if any.
fn emit_address_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    scan: &ReferenceScan<'_>,
    expression: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(head_node) = expression
        .named_child(0)
        .filter(|child| child.kind() == "variable_expr")
    else {
        return Ok(());
    };
    let Some(head_identifier) =
        named_children(head_node).find(|child| child.kind() == "identifier")
    else {
        return Ok(());
    };
    let head = builder.context.text(head_identifier);
    if RESERVED_HEADS.contains(&head) || scan.binds(head) {
        return Ok(());
    }
    let wanted = if head == "data" {
        DATA_ADDRESS_SEGMENTS
    } else {
        NAMED_ADDRESS_SEGMENTS
    };
    let segments = address_segments(expression, wanted);
    let Some(last) = segments
        .last()
        .copied()
        .filter(|_| segments.len() == wanted)
    else {
        return Ok(());
    };
    let mut parts = Vec::with_capacity(wanted.saturating_add(1));
    parts.push(head);
    parts.extend(
        segments
            .iter()
            .filter_map(|segment| {
                named_children(*segment).find(|child| child.kind() == "identifier")
            })
            .map(|identifier| builder.context.text(identifier)),
    );
    let name = join_address(builder, &parts)?;
    builder.emit_reference(ExtractedReference {
        owner: Some(scan.owner.clone()),
        name,
        resolution_name: None,
        kind: ReferenceKind::References,
        span: address_span(head_node, last)?,
    })
}

/// The first `wanted` attribute accesses (`.name`) directly following the
/// address head; indexing or splats end the address.
fn address_segments(expression: Node<'_>, wanted: usize) -> Vec<Node<'_>> {
    named_children(expression)
        .skip(1)
        .take_while(|child| {
            child.kind() == "get_attr"
                && named_children(*child).any(|part| part.kind() == "identifier")
        })
        .take(wanted)
        .collect()
}

fn join_address(
    builder: &ExtractionBuilder<'_, '_>,
    parts: &[&str],
) -> Result<String, ExtractError> {
    let length = parts
        .iter()
        .try_fold(parts.len().saturating_sub(1), |length, part| {
            length.checked_add(part.len())
        })
        .ok_or(ExtractError::OutputLimit)?;
    builder.context.budget.ensure_string_length(length)?;
    let mut address = String::new();
    address
        .try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            address.push('.');
        }
        address.push_str(part);
    }
    Ok(address)
}

fn address_span(first: Node<'_>, last: Node<'_>) -> Result<SourceSpan, ExtractError> {
    let start = span_for(first)?;
    let end = span_for(last)?;
    let start = SourcePosition::new(start.start_byte(), start.start_line(), start.start_column())
        .map_err(|_| ExtractError::InvalidSpan)?;
    let end = SourcePosition::new(end.end_byte(), end.end_line(), end.end_column())
        .map_err(|_| ExtractError::InvalidSpan)?;
    SourceSpan::new(start, end).map_err(|_| ExtractError::InvalidSpan)
}
