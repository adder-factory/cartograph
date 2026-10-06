use cartograph_domain::{
    ReferenceKind, SourceLanguage, SymbolId, SymbolKind, Visibility,
    callable_signature_is_literal_free,
};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind, code_scan::CodeScan};

use super::{
    ExtractionBuilder, PendingReference, PendingSymbol, current_owner_kind_in,
    family_support::screened_name,
    references,
    syntax::{children, descendants_including_root, named_children, span_for},
    with_root_scope,
};

mod annotations;
mod kotlin_callables;

const MAX_SIGNATURE_BYTES: usize = 512;
const MAX_REFERENCE_TARGET_BYTES: usize = 512;
const MAX_TYPE_DEPTH: usize = 64;
const MAX_DOC_BYTES: usize = 16 * 1024;
/// Groovy grammar node for a juxtaposed (parenthesis-free) command call.
const GROOVY_COMMAND_CALL: &str = "juxt_function_call";
/// Groovy declarations that may follow the constants of a recovered enum body.
const GROOVY_ENUM_MEMBER_DECLARATIONS: &[&str] =
    &["function_definition", "function_declaration", "declaration"];
/// Most bytes of one error-recovered line scanned for an open literal before a call.
const MAX_LINE_PREFIX_SCAN_BYTES: usize = 4_096;
/// Most ancestors and preceding siblings inspected for one recovered use.
const MAX_PARSE_ERROR_LOOKBACK_NODES: usize = 128;
const JVM_TYPE_OWNER_KINDS: &[SymbolKind] = &[
    SymbolKind::Class,
    SymbolKind::Struct,
    SymbolKind::Interface,
    SymbolKind::Trait,
    SymbolKind::Enum,
];

#[derive(Clone, Copy)]
struct ImportBindingEmission<'tree, 'text> {
    node: Node<'tree>,
    kind: ImportBindingKind,
    module: &'text str,
    imported: &'text str,
    local: &'text str,
}

struct ContainerEmission<'tree> {
    node: Node<'tree>,
    kind: SymbolKind,
    name: String,
    visibility: Option<Visibility>,
}

struct ConstructorEmission<'tree, 'text> {
    node: Node<'tree>,
    name: &'text str,
    signature: Option<String>,
    visibility: Option<Visibility>,
}

#[derive(Clone, Copy)]
struct PrimaryConstructor<'tree, 'text> {
    parameters: Node<'tree>,
    name: &'text str,
    visibility: Option<Visibility>,
}

#[derive(Clone, Copy)]
struct ScalaPrimaryConstructor<'tree, 'text> {
    parameters: Node<'tree>,
    name: &'text str,
    visibility: Option<Visibility>,
    container: Node<'tree>,
}

struct OwnedBody<'tree> {
    id: SymbolId,
    kind: SymbolKind,
    name: String,
    body: Option<Node<'tree>>,
    depth: usize,
}

#[derive(Clone, Copy)]
struct TypeAliasEmission<'tree> {
    node: Node<'tree>,
    name_node: Node<'tree>,
    target: Option<Node<'tree>>,
}

#[derive(Clone, Copy)]
struct EnumMemberVisit<'tree> {
    node: Node<'tree>,
    depth: usize,
    name_node: Option<Node<'tree>>,
}

#[derive(Clone, Copy)]
struct InheritanceCapture<'tree, 'owner> {
    node: Node<'tree>,
    owner: &'owner SymbolId,
    owner_kind: SymbolKind,
}

#[derive(Clone, Copy)]
struct TypeReferenceCapture<'tree, 'owner> {
    node: Node<'tree>,
    owner: &'owner SymbolId,
    kind: ReferenceKind,
    depth: usize,
}

struct ParameterSignatureInput<'tree, Parameters> {
    parameters: Parameters,
    style: ParameterStyle,
    return_type: Option<Node<'tree>>,
}

#[derive(Clone, Copy)]
struct TypedSignatureInput<'tree, 'text> {
    keyword: &'text str,
    name: &'text str,
    type_node: Option<Node<'tree>>,
}

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match builder.context.snapshot.language() {
        SourceLanguage::Kotlin => visit_kotlin_declaration(builder, node, depth),
        SourceLanguage::Scala => visit_scala_declaration(builder, node, depth),
        SourceLanguage::Groovy => visit_groovy_declaration(builder, node, depth),
        _ => Ok(false),
    }
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match builder.context.snapshot.language() {
        SourceLanguage::Kotlin => capture_kotlin_usage(builder, node),
        SourceLanguage::Scala => capture_scala_usage(builder, node),
        SourceLanguage::Groovy => capture_groovy_usage(builder, node),
        _ => Ok(()),
    }
}

fn visit_kotlin_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "package_header" => visit_persistent_namespace(builder, node, None)?,
        "import_header" => visit_kotlin_import(builder, node)?,
        "class_declaration" | "object_declaration" => {
            visit_kotlin_container(builder, node, depth)?;
        }
        "function_declaration" => visit_kotlin_callable(builder, node, depth)?,
        "secondary_constructor" => visit_kotlin_secondary_constructor(builder, node, depth)?,
        "property_declaration" => visit_kotlin_property(builder, node, depth)?,
        "type_alias" => visit_kotlin_type_alias(builder, node)?,
        "enum_entry" => visit_enum_member(
            builder,
            EnumMemberVisit {
                node,
                depth,
                name_node: kotlin_direct_name(node),
            },
        )?,
        _ => return Ok(false),
    }
    Ok(true)
}

fn visit_scala_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "package_clause" => visit_scala_package(builder, node, depth)?,
        "import_declaration" => visit_scala_import(builder, node)?,
        "class_definition" | "object_definition" | "trait_definition" | "enum_definition" => {
            visit_scala_container(builder, node, depth)?;
        }
        "function_definition" | "function_declaration" => {
            visit_scala_callable(builder, node, depth)?;
        }
        "val_definition" | "var_definition" => visit_scala_binding(builder, node, depth)?,
        "simple_enum_case" | "full_enum_case" => {
            visit_enum_member(
                builder,
                EnumMemberVisit {
                    node,
                    depth,
                    name_node: node.child_by_field_name("name"),
                },
            )?;
        }
        "type_definition" => visit_scala_type_alias(builder, node)?,
        "extension_definition" => visit_scala_extension(builder, node, depth)?,
        _ => return Ok(false),
    }
    Ok(true)
}

fn visit_groovy_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    if node.kind() == "identifier"
        && builder.context.text(node).trim() == "enum"
        && visit_groovy_recovered_enum(builder, node, depth)?
    {
        return Ok(true);
    }
    if node.kind() == "closure" && is_groovy_recovered_enum_body(builder, node) {
        return Ok(true);
    }
    match node.kind() {
        "groovy_package" => {
            let name = named_children(node).find(|child| child.kind() == "qualified_name");
            visit_persistent_namespace(builder, node, name)?;
        }
        "groovy_import" => visit_groovy_import(builder, node)?,
        "class_definition" => visit_groovy_container(builder, node, depth)?,
        "function_definition" | "function_declaration" => {
            visit_groovy_callable(builder, node, depth)?;
        }
        "declaration" => visit_groovy_binding(builder, node, depth)?,
        "function_call" if is_groovy_constructor_node(builder, node) => {
            visit_groovy_constructor(builder, node, depth)?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn visit_groovy_recovered_enum(
    builder: &mut ExtractionBuilder<'_, '_>,
    keyword: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let Some(name_node) = keyword
        .next_named_sibling()
        .filter(|sibling| sibling.kind() == "identifier")
    else {
        return Ok(false);
    };
    let Some(body) = name_node
        .next_named_sibling()
        .filter(|sibling| sibling.kind() == "closure")
    else {
        return Ok(false);
    };
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(false);
    };
    let id = emit_jvm_symbol(
        builder,
        PendingSymbol {
            kind: SymbolKind::Enum,
            name: name.clone(),
            span_node: name_node,
            structural_node: body,
            doc_anchor: keyword,
            body_node: None,
            declaration_only: false,
            signature: None,
            export: crate::SymbolExportFlags::new(true, false),
            async_symbol: false,
            static_member: false,
            visibility: Some(Visibility::Public),
        },
    )?;
    builder.owners.push(id);
    builder.native_owner_kinds.push(SymbolKind::Enum);
    builder.qualifiers.push(name);
    for error in named_children(body).filter(|child| child.kind() == "ERROR") {
        for parameter in descendants_including_root(error).filter(|child| {
            child.kind() == "parameter"
                && child
                    .parent()
                    .is_some_and(|parent| parent.kind() == "parameter_list")
        }) {
            builder.context.ensure_active()?;
            visit_enum_member(
                builder,
                EnumMemberVisit {
                    node: parameter,
                    depth: depth.saturating_add(1),
                    name_node: parameter.child_by_field_name("name"),
                },
            )?;
        }
    }
    // Declarations after the constants (methods, fields) parse cleanly inside
    // the recovered body and belong to the enum. A constant-specific body
    // (`FORMAL { String foo() {} }`) parses as a call with a closure argument
    // and is not an enum member declaration, so it is left alone.
    for member in
        named_children(body).filter(|child| GROOVY_ENUM_MEMBER_DECLARATIONS.contains(&child.kind()))
    {
        builder.visit(member, depth.saturating_add(1))?;
    }
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    Ok(true)
}

fn is_groovy_recovered_enum_body(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> bool {
    node.prev_named_sibling()
        .filter(|name| name.kind() == "identifier")
        .and_then(|name| name.prev_named_sibling())
        .is_some_and(|keyword| {
            keyword.kind() == "identifier" && builder.context.text(keyword).trim() == "enum"
        })
}

fn visit_persistent_namespace(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    explicit_name: Option<Node<'_>>,
) -> Result<(), ExtractError> {
    let name_node = explicit_name.or_else(|| {
        named_children(node).find(|child| {
            matches!(
                child.kind(),
                "identifier" | "package_identifier" | "qualified_name"
            )
        })
    });
    let Some(name_node) = name_node else {
        return Ok(());
    };
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(());
    };
    let id = emit_namespace(builder, node, name.clone())?;
    builder.owners.push(id);
    builder.native_owner_kinds.push(SymbolKind::Namespace);
    builder.qualifiers.push(name);
    Ok(())
}

fn visit_scala_package(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let Some(body) = node.child_by_field_name("body") else {
        return visit_persistent_namespace(builder, node, Some(name_node));
    };
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(());
    };
    let id = emit_namespace(builder, node, name.clone())?;
    builder.owners.push(id);
    builder.native_owner_kinds.push(SymbolKind::Namespace);
    builder.qualifiers.push(name);
    let result = builder.visit(body, depth.saturating_add(1));
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn emit_namespace(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    name: String,
) -> Result<SymbolId, ExtractError> {
    emit_jvm_symbol(builder, PendingSymbol::namespace(node, name))
}

fn visit_kotlin_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(target_node) = named_children(node).find(|child| child.kind() == "identifier") else {
        return Ok(());
    };
    let target = builder.context.owned_text(target_node)?;
    if !safe_import_target(&target) {
        return Ok(());
    }
    let alias = named_children(node)
        .find(|child| child.kind() == "import_alias")
        .and_then(|alias| named_children(alias).next())
        .map(|alias| builder.context.owned_text(alias))
        .transpose()?;
    let wildcard = named_children(node).any(|child| child.kind() == "wildcard_import");
    emit_import(builder, node, target.clone())?;
    if wildcard {
        emit_binding(
            builder,
            ImportBindingEmission {
                node,
                kind: ImportBindingKind::Namespace,
                module: &target,
                imported: "*",
                local: alias.as_deref().unwrap_or("*"),
            },
        )?;
    } else if let Some((module, imported)) = target.rsplit_once('.') {
        emit_binding(
            builder,
            ImportBindingEmission {
                node,
                kind: ImportBindingKind::Named,
                module,
                imported,
                local: alias.as_deref().unwrap_or(imported),
            },
        )?;
    }
    Ok(())
}

fn visit_scala_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let raw = builder.context.text(node).trim();
    let Some(body) = raw.strip_prefix("import").map(str::trim) else {
        return Ok(());
    };
    let target = body
        .split(['{', ' '])
        .next()
        .unwrap_or_default()
        .trim_end_matches(".*")
        .trim_end_matches("._")
        .trim_end_matches('.');
    if !safe_import_target(target) {
        return Ok(());
    }
    let target = builder.context.copy_text(target)?;
    emit_import(builder, node, target.clone())?;
    if named_children(node).any(|child| child.kind() == "namespace_wildcard") {
        emit_binding(
            builder,
            ImportBindingEmission {
                node,
                kind: ImportBindingKind::Namespace,
                module: &target,
                imported: "*",
                local: "*",
            },
        )?;
    } else if let Some((module, imported)) = target.rsplit_once('.') {
        emit_binding(
            builder,
            ImportBindingEmission {
                node,
                kind: ImportBindingKind::Named,
                module,
                imported,
                local: imported,
            },
        )?;
    }
    Ok(())
}

fn visit_groovy_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(target_node) = node.child_by_field_name("import") else {
        return Ok(());
    };
    let target = builder.context.owned_text(target_node)?;
    if !safe_import_target(&target) {
        return Ok(());
    }
    let alias = node
        .child_by_field_name("import_alias")
        .map(|alias| builder.context.owned_text(alias))
        .transpose()?;
    let wildcard = named_children(node).any(|child| child.kind() == "wildcard_import");
    emit_import(builder, node, target.clone())?;
    if wildcard {
        emit_binding(
            builder,
            ImportBindingEmission {
                node,
                kind: ImportBindingKind::Namespace,
                module: &target,
                imported: "*",
                local: alias.as_deref().unwrap_or("*"),
            },
        )?;
    } else if let Some((module, imported)) = target.rsplit_once('.') {
        emit_binding(
            builder,
            ImportBindingEmission {
                node,
                kind: ImportBindingKind::Named,
                module,
                imported,
                local: alias.as_deref().unwrap_or(imported),
            },
        )?;
    }
    Ok(())
}

fn safe_import_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= MAX_REFERENCE_TARGET_BYTES
        && !super::specifier_safety::specifier_may_carry_credential(target)
        && target
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'$'))
}

fn emit_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    name: String,
) -> Result<(), ExtractError> {
    let raw = builder.context.text(node).trim();
    let signature = (raw.len() <= MAX_SIGNATURE_BYTES && callable_signature_is_literal_free(raw))
        .then(|| builder.context.copy_text(raw))
        .transpose()?;
    let reference_name = name.clone();
    emit_root_symbol(
        builder,
        PendingSymbol {
            kind: SymbolKind::Import,
            name,
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: None,
            declaration_only: false,
            signature,
            export: crate::SymbolExportFlags::new(false, false),
            async_symbol: false,
            static_member: false,
            visibility: None,
        },
    )?;
    references::push_reference(
        builder,
        PendingReference {
            owner: None,
            name: reference_name,
            kind: ReferenceKind::Imports,
            node,
        },
    )
}

fn emit_root_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    pending: PendingSymbol<'_>,
) -> Result<SymbolId, ExtractError> {
    with_root_scope(builder, |builder| emit_jvm_symbol(builder, pending))
}

fn emit_jvm_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    pending: PendingSymbol<'_>,
) -> Result<SymbolId, ExtractError> {
    let doc_anchor = pending.doc_anchor;
    let structural_node = pending.structural_node;
    let custom_doc = preceding_jvm_doc(builder, doc_anchor)?;
    let id = builder.emit_symbol(pending)?;
    annotations::capture_annotations(builder, structural_node, &id)?;
    let needs_override = builder
        .facts
        .symbols
        .last()
        .is_some_and(|symbol| symbol.id == id && symbol.docstring.is_none());
    if needs_override && let Some(docstring) = custom_doc {
        builder.context.budget.reserve_fact(
            u64::try_from(docstring.len()).map_err(|_| ExtractError::OutputLimit)?,
            [docstring.as_str()],
        )?;
        if let Some(symbol) = builder.facts.symbols.last_mut() {
            symbol.docstring = Some(docstring);
        }
    }
    Ok(id)
}

fn preceding_jvm_doc(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    builder.context.ensure_active()?;
    let source = builder.context.source();
    let Some((prefix, lower_bound)) = jvm_doc_prefix(source, node.start_byte()) else {
        return Ok(None);
    };
    let Some(raw) = raw_jvm_doc(prefix, lower_bound) else {
        return Ok(None);
    };
    if raw
        .split_whitespace()
        .any(super::specifier_safety::specifier_may_carry_credential)
    {
        return Ok(None);
    }
    let raw = builder.context.copy_text(raw)?;
    normalize_doc(builder, &raw)
}

fn jvm_doc_prefix(source: &str, upper_bound: usize) -> Option<(&str, usize)> {
    let lower_bound = source
        .ceil_char_boundary(upper_bound.saturating_sub(MAX_DOC_BYTES.saturating_add(4)))
        .min(upper_bound);
    let before = source.get(lower_bound..upper_bound)?;
    let trimmed_end = before.trim_end_matches(char::is_whitespace).len();
    let gap = before.get(trimmed_end..).unwrap_or_default();
    if gap.bytes().filter(|byte| *byte == b'\n').count() > 1 {
        return None;
    }
    let prefix = before.get(..trimmed_end).unwrap_or_default();
    Some((prefix, lower_bound))
}

fn raw_jvm_doc(prefix: &str, lower_bound: usize) -> Option<&str> {
    if prefix.ends_with("*/") {
        let relative_start = prefix.rfind("/*")?;
        if lower_bound > 0 && relative_start == 0 {
            return None;
        }
        return Some(prefix.get(relative_start..).unwrap_or_default());
    }
    preceding_jvm_line_doc(prefix)
}

fn preceding_jvm_line_doc(prefix: &str) -> Option<&str> {
    let mut start = prefix.len();
    let mut found = false;
    for line in prefix.lines().rev() {
        let line_start = start.saturating_sub(line.len());
        if !line.trim_start().starts_with("//") {
            break;
        }
        found = true;
        start = line_start.saturating_sub(1);
        if prefix.len().saturating_sub(start) > MAX_DOC_BYTES {
            return None;
        }
    }
    found.then(|| prefix.get(start..).unwrap_or_default().trim_start())
}

fn normalize_doc(
    builder: &mut ExtractionBuilder<'_, '_>,
    raw: &str,
) -> Result<Option<String>, ExtractError> {
    if raw.is_empty() || raw.len() > MAX_DOC_BYTES {
        return Ok(None);
    }
    let block = raw.trim().starts_with("/*");
    let mut normalized = String::new();
    normalized
        .try_reserve(raw.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    for line in raw.lines() {
        builder.context.ensure_active()?;
        let mut cleaned = line.trim();
        if block {
            cleaned = cleaned
                .strip_prefix("/**")
                .or_else(|| cleaned.strip_prefix("/*!"))
                .or_else(|| cleaned.strip_prefix("/*"))
                .unwrap_or(cleaned)
                .trim();
            cleaned = cleaned.strip_suffix("*/").unwrap_or(cleaned).trim();
            cleaned = cleaned.strip_prefix('*').unwrap_or(cleaned).trim();
        } else {
            cleaned = cleaned
                .trim_start_matches('/')
                .trim_start_matches('!')
                .trim();
        }
        if cleaned.is_empty() {
            continue;
        }
        if !normalized.is_empty() {
            normalized.push('\n');
        }
        normalized.push_str(cleaned);
    }
    if normalized.is_empty() {
        return Ok(None);
    }
    builder
        .context
        .budget
        .ensure_string_length(normalized.len())?;
    Ok(Some(normalized))
}

fn emit_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ImportBindingEmission<'_, '_>,
) -> Result<(), ExtractError> {
    let ImportBindingEmission {
        node,
        kind,
        module,
        imported,
        local,
    } = input;
    if [module, imported, local]
        .into_iter()
        .any(super::specifier_safety::specifier_may_carry_credential)
    {
        return Ok(());
    }
    builder.emit_import_binding(ExtractedImportBinding {
        kind,
        module_specifier: builder.context.copy_text(module)?,
        imported_name: builder.context.copy_text(imported)?,
        local_name: builder.context.copy_text(local)?,
        span: span_for(node)?,
    })
}

fn visit_kotlin_container(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = named_children(node).find(|child| child.kind() == "type_identifier")
    else {
        return builder.visit_named_children(node, depth);
    };
    let kind = if node.kind() == "object_declaration" {
        SymbolKind::Class
    } else if direct_keyword(builder, node, "interface")? {
        SymbolKind::Interface
    } else if direct_keyword(builder, node, "enum")? {
        SymbolKind::Enum
    } else {
        SymbolKind::Class
    };
    let body =
        named_children(node).find(|child| matches!(child.kind(), "class_body" | "enum_class_body"));
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(());
    };
    let visibility = jvm_visibility(builder, node)?;
    let id = emit_container(
        builder,
        ContainerEmission {
            node,
            kind,
            name: name.clone(),
            visibility,
        },
    )?;
    capture_kotlin_inheritance(
        builder,
        InheritanceCapture {
            node,
            owner: &id,
            owner_kind: kind,
        },
    )?;

    builder.owners.push(id);
    builder.native_owner_kinds.push(kind);
    builder.qualifiers.push(name.clone());
    if let Some(primary) = named_children(node).find(|child| child.kind() == "primary_constructor")
    {
        emit_kotlin_primary_constructor(
            builder,
            PrimaryConstructor {
                parameters: primary,
                name: &name,
                visibility,
            },
        )?;
    }
    let result = match body {
        Some(body) => builder.visit(body, depth.saturating_add(1)),
        None => Ok(()),
    };
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn visit_scala_container(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let kind = match node.kind() {
        "trait_definition" => SymbolKind::Trait,
        "enum_definition" => SymbolKind::Enum,
        "class_definition" | "object_definition" => SymbolKind::Class,
        _ => return builder.visit_named_children(node, depth),
    };
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(());
    };
    let visibility = jvm_visibility(builder, node)?;
    let id = emit_container(
        builder,
        ContainerEmission {
            node,
            kind,
            name: name.clone(),
            visibility,
        },
    )?;
    capture_scala_inheritance(
        builder,
        InheritanceCapture {
            node,
            owner: &id,
            owner_kind: kind,
        },
    )?;

    builder.owners.push(id);
    builder.native_owner_kinds.push(kind);
    builder.qualifiers.push(name.clone());
    if matches!(node.kind(), "class_definition" | "enum_definition") {
        for parameters in named_children(node).filter(|child| child.kind() == "class_parameters") {
            emit_scala_primary_constructor(
                builder,
                ScalaPrimaryConstructor {
                    parameters,
                    name: &name,
                    visibility,
                    container: node,
                },
            )?;
        }
    }
    let result = if let Some(body) = node.child_by_field_name("body") {
        builder.visit(body, depth.saturating_add(1))
    } else {
        Ok(())
    };
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn visit_groovy_container(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let kind = if direct_keyword(builder, node, "interface")?
        || direct_keyword(builder, node, "@interface")?
    {
        SymbolKind::Interface
    } else {
        SymbolKind::Class
    };
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(());
    };
    let visibility = jvm_visibility(builder, node)?;
    let id = emit_container(
        builder,
        ContainerEmission {
            node,
            kind,
            name: name.clone(),
            visibility,
        },
    )?;
    if let Some(superclass) = node.child_by_field_name("superclass") {
        capture_outer_type_reference(
            builder,
            TypeReferenceCapture {
                node: superclass,
                owner: &id,
                kind: ReferenceKind::Extends,
                depth: 0,
            },
        )?;
    }
    capture_groovy_implements(builder, node, &id)?;

    builder.owners.push(id);
    builder.native_owner_kinds.push(kind);
    builder.qualifiers.push(name);
    let result = match node.child_by_field_name("body") {
        Some(body) => builder.visit(body, depth.saturating_add(1)),
        None => Ok(()),
    };
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn emit_container(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ContainerEmission<'_>,
) -> Result<SymbolId, ExtractError> {
    let ContainerEmission {
        node,
        kind,
        name,
        visibility,
    } = input;
    emit_jvm_symbol(
        builder,
        PendingSymbol {
            kind,
            name,
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: None,
            declaration_only: false,
            signature: None,
            export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
            async_symbol: false,
            static_member: false,
            visibility,
        },
    )
}

fn emit_kotlin_primary_constructor(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: PrimaryConstructor<'_, '_>,
) -> Result<(), ExtractError> {
    let PrimaryConstructor {
        parameters,
        name,
        visibility,
    } = input;
    let signature = kotlin_parameters_signature(builder, parameters)?;
    let constructor_visibility = jvm_visibility(builder, parameters)?.or(visibility);
    let constructor_id = emit_constructor(
        builder,
        ConstructorEmission {
            node: parameters,
            name,
            signature,
            visibility: constructor_visibility,
        },
    )?;
    for parameter in named_children(parameters).filter(|child| child.kind() == "class_parameter") {
        builder.context.ensure_active()?;
        if let Some(type_node) = kotlin_type_node(parameter) {
            capture_type_references(
                builder,
                TypeReferenceCapture {
                    node: type_node,
                    owner: &constructor_id,
                    kind: ReferenceKind::TypeOf,
                    depth: 0,
                },
            )?;
        }
        if !named_children(parameter).any(|child| child.kind() == "binding_pattern_kind") {
            continue;
        }
        emit_kotlin_property_symbol(builder, parameter, true)?;
    }
    Ok(())
}

fn emit_scala_primary_constructor(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ScalaPrimaryConstructor<'_, '_>,
) -> Result<(), ExtractError> {
    let ScalaPrimaryConstructor {
        parameters,
        name,
        visibility,
        container,
    } = input;
    let signature = scala_parameters_signature(builder, std::iter::once(parameters))?;
    let constructor_id = emit_constructor(
        builder,
        ConstructorEmission {
            node: parameters,
            name,
            signature,
            visibility,
        },
    )?;
    let case_class = direct_keyword(builder, container, "case")?;
    for parameter in named_children(parameters).filter(|child| child.kind() == "class_parameter") {
        builder.context.ensure_active()?;
        if let Some(type_node) = parameter.child_by_field_name("type") {
            capture_type_references(
                builder,
                TypeReferenceCapture {
                    node: type_node,
                    owner: &constructor_id,
                    kind: ReferenceKind::TypeOf,
                    depth: 0,
                },
            )?;
        }
        if case_class
            || direct_keyword(builder, parameter, "val")?
            || direct_keyword(builder, parameter, "var")?
        {
            emit_scala_class_parameter(builder, parameter)?;
        }
    }
    Ok(())
}

fn emit_constructor(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ConstructorEmission<'_, '_>,
) -> Result<SymbolId, ExtractError> {
    let ConstructorEmission {
        node,
        name,
        signature,
        visibility,
    } = input;
    emit_jvm_symbol(
        builder,
        PendingSymbol {
            kind: SymbolKind::Method,
            name: builder.context.copy_text(name)?,
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: None,
            declaration_only: false,
            signature,
            export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
            async_symbol: false,
            static_member: false,
            visibility,
        },
    )
}

fn visit_kotlin_secondary_constructor(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name) = builder.qualifiers.last().cloned() else {
        return builder.visit_named_children(node, depth);
    };
    let Some(parameters) =
        named_children(node).find(|child| child.kind() == "function_value_parameters")
    else {
        return builder.visit_named_children(node, depth);
    };
    let body = named_children(node).find(|child| child.kind() == "statements");
    let visibility = jvm_visibility(builder, node)?;
    let signature = kotlin_parameters_signature(builder, parameters)?;
    let id = emit_jvm_symbol(
        builder,
        PendingSymbol {
            kind: SymbolKind::Method,
            name: name.clone(),
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: body,
            declaration_only: false,
            signature,
            export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
            async_symbol: false,
            static_member: false,
            visibility,
        },
    )?;
    capture_kotlin_parameter_types(builder, parameters, &id)?;
    visit_owned_body(
        builder,
        OwnedBody {
            id,
            kind: SymbolKind::Method,
            name,
            body,
            depth,
        },
    )
}

fn visit_kotlin_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = kotlin_direct_name(node) else {
        return builder.visit_named_children(node, depth);
    };
    let Some(parameters) =
        named_children(node).find(|child| child.kind() == "function_value_parameters")
    else {
        return builder.visit_named_children(node, depth);
    };
    // A top-level extension function keeps the `Function` kind (Kotlin compiles
    // it to a static function, and v2 member resolution would otherwise stop
    // binding its bare `shout()` calls); its receiver qualifies its identity.
    let receiver = kotlin_callables::extension_receiver(builder, node)?;
    let kind = if current_owner_kind_in(builder, JVM_TYPE_OWNER_KINDS) {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    };
    let qualifier_depth = builder.qualifiers.len();
    builder.qualifiers.extend(receiver);
    let result = emit_kotlin_callable(
        builder,
        KotlinCallable {
            node,
            name_node,
            parameters,
            kind,
            depth,
        },
    );
    builder.qualifiers.truncate(qualifier_depth);
    result
}

#[derive(Clone, Copy)]
struct KotlinCallable<'tree> {
    node: Node<'tree>,
    name_node: Node<'tree>,
    parameters: Node<'tree>,
    kind: SymbolKind,
    depth: usize,
}

fn emit_kotlin_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: KotlinCallable<'_>,
) -> Result<(), ExtractError> {
    let KotlinCallable {
        node,
        name_node,
        parameters,
        kind,
        depth,
    } = input;
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(());
    };
    let return_type = kotlin_return_type(node, parameters);
    let body = named_children(node).find(|child| child.kind() == "function_body");
    let visibility = jvm_visibility(builder, node)?;
    let signature = kotlin_callable_signature(builder, parameters, return_type)?;
    let async_symbol = has_modifier(builder, node, "suspend")?;
    let id = emit_jvm_symbol(
        builder,
        PendingSymbol {
            kind,
            name: name.clone(),
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: body,
            declaration_only: body.is_none(),
            signature,
            export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
            async_symbol,
            static_member: false,
            visibility,
        },
    )?;
    capture_kotlin_parameter_types(builder, parameters, &id)?;
    if let Some(return_type) = return_type {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                node: return_type,
                owner: &id,
                kind: ReferenceKind::Returns,
                depth: 0,
            },
        )?;
    }
    if let Some(receiver) = node.child_by_field_name("receiver") {
        kotlin_callables::capture_receiver_types(builder, receiver, &id)?;
    }
    builder.owners.push(id);
    builder.native_owner_kinds.push(kind);
    builder.qualifiers.push(name);
    let result = kotlin_callables::emit_annotated_parameters(builder, parameters)
        .and_then(|()| body.map_or(Ok(()), |body| builder.visit(body, depth.saturating_add(1))));
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn visit_scala_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(());
    };
    let kind = if current_owner_kind_in(builder, JVM_TYPE_OWNER_KINDS) {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    };
    let parameters = named_children(node)
        .filter(|child| child.kind() == "parameters")
        .collect::<Vec<_>>();
    let return_type = node.child_by_field_name("return_type");
    let body = node.child_by_field_name("body");
    let visibility = jvm_visibility(builder, node)?;
    let signature = scala_callable_signature(builder, &parameters, return_type)?;
    let static_member = has_modifier(builder, node, "static")?;
    let id = emit_jvm_symbol(
        builder,
        PendingSymbol {
            kind,
            name: name.clone(),
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: body,
            declaration_only: body.is_none(),
            signature,
            export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
            async_symbol: false,
            static_member,
            visibility,
        },
    )?;
    for parameter_list in &parameters {
        capture_dynamic_parameter_types(builder, *parameter_list, &id)?;
    }
    if let Some(return_type) = return_type {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                node: return_type,
                owner: &id,
                kind: ReferenceKind::Returns,
                depth: 0,
            },
        )?;
    }
    visit_owned_body(
        builder,
        OwnedBody {
            id,
            kind,
            name,
            body,
            depth,
        },
    )
}

fn visit_groovy_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("function") else {
        return builder.visit_named_children(node, depth);
    };
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return builder.visit_named_children(node, depth);
    };
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(());
    };
    let kind = if current_owner_kind_in(builder, JVM_TYPE_OWNER_KINDS) {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    };
    let return_type = node.child_by_field_name("type");
    let body = node.child_by_field_name("body");
    let visibility = jvm_visibility(builder, node)?;
    let signature = groovy_callable_signature(builder, parameters, return_type)?;
    let static_member = has_modifier(builder, node, "static")?;
    let id = emit_jvm_symbol(
        builder,
        PendingSymbol {
            kind,
            name: name.clone(),
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: body,
            declaration_only: body.is_none(),
            signature,
            export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
            async_symbol: false,
            static_member,
            visibility,
        },
    )?;
    capture_dynamic_parameter_types(builder, parameters, &id)?;
    if let Some(return_type) = return_type {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                node: return_type,
                owner: &id,
                kind: ReferenceKind::Returns,
                depth: 0,
            },
        )?;
    }
    visit_owned_body(
        builder,
        OwnedBody {
            id,
            kind,
            name,
            body,
            depth,
        },
    )
}

fn is_groovy_constructor_node(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> bool {
    if !current_owner_kind_in(builder, JVM_TYPE_OWNER_KINDS) {
        return false;
    }
    let Some(function) = node.child_by_field_name("function") else {
        return false;
    };
    let Some(container_name) = builder.qualifiers.last() else {
        return false;
    };
    builder.context.text(function).trim() == container_name
        && descendants_including_root(node).any(|child| child.kind() == "closure")
}

fn visit_groovy_constructor(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("function") else {
        return Ok(());
    };
    let Some(arguments) = node.child_by_field_name("args") else {
        return Ok(());
    };
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(());
    };
    let signature = groovy_recovered_constructor_signature(builder, arguments)?;
    let visibility = jvm_visibility(builder, node)?;
    let body = descendants_including_root(arguments)
        .filter(|child| child.kind() == "closure")
        .last();
    let id = emit_jvm_symbol(
        builder,
        PendingSymbol {
            kind: SymbolKind::Method,
            name: name.clone(),
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: body,
            declaration_only: false,
            signature,
            export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
            async_symbol: false,
            static_member: false,
            visibility,
        },
    )?;
    capture_recovered_groovy_parameter_types(builder, arguments, &id)?;
    visit_owned_body(
        builder,
        OwnedBody {
            id,
            kind: SymbolKind::Method,
            name,
            body,
            depth,
        },
    )
}

fn groovy_recovered_constructor_signature(
    builder: &ExtractionBuilder<'_, '_>,
    arguments: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let raw = builder.context.text(arguments).trim();
    let Some(parameters) = bounded_groovy_parameters(raw) else {
        return Ok(None);
    };
    let mut signature = String::from("(");
    for (index, parameter) in parameters.split(',').enumerate() {
        let declaration = parameter.split('=').next().unwrap_or_default().trim();
        if declaration.is_empty() {
            continue;
        }
        if index > 0 {
            signature.push_str(", ");
        }
        signature.push_str(declaration);
        if signature.len().saturating_add(1) > MAX_SIGNATURE_BYTES {
            return Ok(None);
        }
    }
    signature.push(')');
    safe_signature(builder, signature)
}

fn capture_recovered_groovy_parameter_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    arguments: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    let raw = builder.context.text(arguments).trim();
    let Some(parameters) = bounded_groovy_parameters(raw) else {
        return Ok(());
    };
    let parameters = builder.context.copy_text(parameters)?;
    for parameter in parameters.split(',') {
        builder.context.ensure_active()?;
        let declaration = parameter.split('=').next().unwrap_or_default().trim();
        let mut tokens = declaration.split_whitespace();
        let Some(type_name) = tokens.next() else {
            continue;
        };
        if tokens.next().is_none() || is_jvm_builtin(type_name) || !safe_import_target(type_name) {
            continue;
        }
        let type_name = builder.context.copy_text(type_name)?;
        push_named_reference(
            builder,
            PendingReference {
                owner: Some(owner.clone()),
                name: type_name,
                kind: ReferenceKind::TypeOf,
                node: arguments,
            },
        )?;
    }
    Ok(())
}

fn bounded_groovy_parameters(raw: &str) -> Option<&str> {
    let open = raw.find('(')?;
    let start = open.checked_add(1)?;
    let bounded_end = start.checked_add(MAX_SIGNATURE_BYTES)?.min(raw.len());
    let bounded = raw.get(start..bounded_end)?;
    let relative_close = bounded.find(')')?;
    let parameters = bounded.get(..relative_close)?;
    (!parameters.contains(['(', ')'])).then_some(parameters)
}

fn visit_owned_body(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: OwnedBody<'_>,
) -> Result<(), ExtractError> {
    let OwnedBody {
        id,
        kind,
        name,
        body,
        depth,
    } = input;
    let Some(body) = body else {
        return Ok(());
    };
    builder.owners.push(id);
    builder.native_owner_kinds.push(kind);
    builder.qualifiers.push(name);
    let result = builder.visit(body, depth.saturating_add(1));
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn visit_kotlin_property(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(id) = emit_kotlin_property_symbol(
        builder,
        node,
        current_owner_kind_in(builder, JVM_TYPE_OWNER_KINDS),
    )?
    else {
        return builder.visit_named_children(node, depth);
    };
    let Some(variable) = named_children(node).find(|child| child.kind() == "variable_declaration")
    else {
        return Ok(());
    };
    for value in named_children(node).filter(|child| {
        child.start_byte() >= variable.end_byte() && child.kind() != "type_constraints"
    }) {
        builder.owners.push(id.clone());
        let result = builder.visit(value, depth.saturating_add(1));
        builder.owners.pop();
        result?;
    }
    Ok(())
}

fn emit_kotlin_property_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    class_scope: bool,
) -> Result<Option<SymbolId>, ExtractError> {
    let (name_node, type_node) = if node.kind() == "class_parameter" {
        (kotlin_direct_name(node), kotlin_type_node(node))
    } else {
        let Some(variable) =
            named_children(node).find(|child| child.kind() == "variable_declaration")
        else {
            return Ok(None);
        };
        (kotlin_direct_name(variable), kotlin_type_node(variable))
    };
    let Some(name_node) = name_node else {
        return Ok(None);
    };
    let immutable = named_children(node)
        .find(|child| child.kind() == "binding_pattern_kind")
        .map(|child| builder.context.text(child).trim())
        .and_then(|keyword| match keyword {
            "val" => Some(true),
            "var" => Some(false),
            _ => None,
        });
    let Some(immutable) = immutable else {
        return Ok(None);
    };
    let keyword = if immutable { "val" } else { "var" };
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(None);
    };
    let kind = if class_scope {
        SymbolKind::Field
    } else if immutable {
        SymbolKind::Constant
    } else {
        SymbolKind::Variable
    };
    let visibility = if class_scope {
        jvm_visibility(builder, node)?
    } else {
        None
    };
    let id = emit_jvm_symbol(
        builder,
        PendingSymbol {
            kind,
            name: name.clone(),
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: None,
            declaration_only: false,
            signature: kotlin_property_signature(
                builder,
                TypedSignatureInput {
                    keyword,
                    name: &name,
                    type_node,
                },
            )?,
            export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
            async_symbol: false,
            static_member: false,
            visibility,
        },
    )?;
    if let Some(type_node) = type_node {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                node: type_node,
                owner: &id,
                kind: ReferenceKind::TypeOf,
                depth: 0,
            },
        )?;
    }
    Ok(Some(id))
}

fn emit_scala_class_parameter(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(());
    };
    let type_node = node.child_by_field_name("type");
    let keyword = if direct_keyword(builder, node, "var")? {
        "var"
    } else {
        "val"
    };
    let visibility = jvm_visibility(builder, node)?;
    let id = emit_jvm_symbol(
        builder,
        PendingSymbol {
            kind: SymbolKind::Field,
            name: name.clone(),
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: None,
            declaration_only: false,
            signature: keyword_typed_signature(
                builder,
                TypedSignatureInput {
                    keyword,
                    name: &name,
                    type_node,
                },
            )?,
            export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
            async_symbol: false,
            static_member: false,
            visibility,
        },
    )?;
    if let Some(type_node) = type_node {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                node: type_node,
                owner: &id,
                kind: ReferenceKind::TypeOf,
                depth: 0,
            },
        )?;
    }
    Ok(())
}

fn visit_scala_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(pattern) = node.child_by_field_name("pattern") else {
        return builder.visit_named_children(node, depth);
    };
    let Some(name_node) = scala_pattern_name(pattern) else {
        return builder.visit_named_children(node, depth);
    };
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(());
    };
    let class_scope = current_owner_kind_in(builder, JVM_TYPE_OWNER_KINDS);
    let immutable = node.kind() == "val_definition";
    let kind = if class_scope {
        SymbolKind::Field
    } else if immutable {
        SymbolKind::Constant
    } else {
        SymbolKind::Variable
    };
    let type_node = node.child_by_field_name("type");
    let value = node.child_by_field_name("value");
    let visibility = if class_scope {
        jvm_visibility(builder, node)?
    } else {
        None
    };
    let static_member = has_modifier(builder, node, "static")?;
    let id = emit_jvm_symbol(
        builder,
        PendingSymbol {
            kind,
            name: name.clone(),
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: None,
            declaration_only: false,
            signature: keyword_typed_signature(
                builder,
                TypedSignatureInput {
                    keyword: if immutable { "val" } else { "var" },
                    name: &name,
                    type_node,
                },
            )?,
            export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
            async_symbol: false,
            static_member,
            visibility,
        },
    )?;
    if let Some(type_node) = type_node {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                node: type_node,
                owner: &id,
                kind: ReferenceKind::TypeOf,
                depth: 0,
            },
        )?;
    }
    if let Some(value) = value {
        builder.owners.push(id);
        let result = builder.visit(value, depth.saturating_add(1));
        builder.owners.pop();
        result?;
    }
    Ok(())
}

fn visit_groovy_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(());
    };
    let class_scope = current_owner_kind_in(builder, JVM_TYPE_OWNER_KINDS);
    let kind = if class_scope {
        SymbolKind::Field
    } else if has_modifier(builder, node, "final")? {
        SymbolKind::Constant
    } else {
        SymbolKind::Variable
    };
    let type_node = node.child_by_field_name("type");
    let value = node.child_by_field_name("value");
    let visibility = if class_scope {
        jvm_visibility(builder, node)?
    } else {
        None
    };
    let static_member = has_modifier(builder, node, "static")?;
    let id = emit_jvm_symbol(
        builder,
        PendingSymbol {
            kind,
            name: name.clone(),
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: None,
            declaration_only: false,
            signature: groovy_typed_signature(builder, type_node, &name)?,
            export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
            async_symbol: false,
            static_member,
            visibility,
        },
    )?;
    if let Some(type_node) = type_node {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                node: type_node,
                owner: &id,
                kind: ReferenceKind::TypeOf,
                depth: 0,
            },
        )?;
    }
    if let Some(value) = value {
        builder.owners.push(id);
        let result = builder.visit(value, depth.saturating_add(1));
        builder.owners.pop();
        result?;
    }
    Ok(())
}

fn visit_kotlin_type_alias(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let mut types = named_children(node).filter(|child| {
        matches!(
            child.kind(),
            "type_identifier"
                | "user_type"
                | "nullable_type"
                | "function_type"
                | "parenthesized_type"
        )
    });
    let Some(name_node) = types.next() else {
        return Ok(());
    };
    let target = types.next();
    emit_type_alias(
        builder,
        TypeAliasEmission {
            node,
            name_node,
            target,
        },
    )
}

fn visit_scala_type_alias(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(());
    };
    emit_type_alias(
        builder,
        TypeAliasEmission {
            node,
            name_node,
            target: node.child_by_field_name("type"),
        },
    )
}

fn emit_type_alias(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeAliasEmission<'_>,
) -> Result<(), ExtractError> {
    let TypeAliasEmission {
        node,
        name_node,
        target,
    } = input;
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(());
    };
    let visibility = jvm_visibility(builder, node)?;
    let id = emit_jvm_symbol(
        builder,
        PendingSymbol {
            kind: SymbolKind::TypeAlias,
            name,
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: None,
            declaration_only: false,
            signature: None,
            export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
            async_symbol: false,
            static_member: false,
            visibility,
        },
    )?;
    if let Some(target) = target {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                node: target,
                owner: &id,
                kind: ReferenceKind::TypeOf,
                depth: 0,
            },
        )?;
    }
    Ok(())
}

fn visit_enum_member(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: EnumMemberVisit<'_>,
) -> Result<(), ExtractError> {
    let EnumMemberVisit {
        node,
        depth,
        name_node,
    } = input;
    let Some(name_node) = name_node else {
        return builder.visit_named_children(node, depth);
    };
    let Some(name) = screened_name(builder, name_node)? else {
        return Ok(());
    };
    let id = emit_jvm_symbol(
        builder,
        PendingSymbol {
            kind: SymbolKind::EnumMember,
            name: name.clone(),
            span_node: node,
            structural_node: node,
            doc_anchor: node,
            body_node: None,
            declaration_only: false,
            signature: None,
            export: crate::SymbolExportFlags::new(true, false),
            async_symbol: false,
            static_member: true,
            visibility: Some(Visibility::Public),
        },
    )?;
    let body =
        named_children(node).find(|child| matches!(child.kind(), "class_body" | "template_body"));
    visit_owned_body(
        builder,
        OwnedBody {
            id,
            kind: SymbolKind::EnumMember,
            name,
            body,
            depth,
        },
    )
}

fn visit_scala_extension(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    for child in named_children(node) {
        builder.context.ensure_active()?;
        if child.kind() != "parameters" && child.kind() != "type_parameters" {
            builder.visit(child, depth.saturating_add(1))?;
        }
    }
    Ok(())
}

fn capture_kotlin_inheritance(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: InheritanceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    let InheritanceCapture {
        node,
        owner,
        owner_kind,
    } = input;
    let mut index = 0_usize;
    for specifier in named_children(node).filter(|child| child.kind() == "delegation_specifier") {
        builder.context.ensure_active()?;
        let construction =
            named_children(specifier).any(|child| child.kind() == "constructor_invocation");
        let kind = if owner_kind == SymbolKind::Interface || (construction && index == 0) {
            ReferenceKind::Extends
        } else {
            ReferenceKind::Implements
        };
        capture_outer_type_reference(
            builder,
            TypeReferenceCapture {
                node: specifier,
                owner,
                kind,
                depth: 0,
            },
        )?;
        index = index.saturating_add(1);
    }
    Ok(())
}

fn capture_scala_inheritance(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: InheritanceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    let InheritanceCapture {
        node,
        owner,
        owner_kind,
    } = input;
    let Some(clause) = node.child_by_field_name("extend") else {
        return Ok(());
    };
    let mut index = 0_usize;
    for target in named_children(clause).filter(|child| child.kind() != "arguments") {
        builder.context.ensure_active()?;
        let kind = if owner_kind == SymbolKind::Trait || index == 0 {
            ReferenceKind::Extends
        } else {
            ReferenceKind::Implements
        };
        capture_outer_type_reference(
            builder,
            TypeReferenceCapture {
                node: target,
                owner,
                kind,
                depth: 0,
            },
        )?;
        index = index.saturating_add(1);
    }
    Ok(())
}

fn capture_groovy_implements(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    for error in named_children(node).filter(|child| child.kind() == "ERROR") {
        builder.context.ensure_active()?;
        let raw = builder.context.text(error).trim();
        let Some(targets) = raw.strip_prefix("implements").map(str::trim) else {
            continue;
        };
        let targets = builder.context.copy_text(targets)?;
        for target in targets.split(',') {
            let Some(name) = normalize_reference(builder, target.trim())? else {
                continue;
            };
            push_named_reference(
                builder,
                PendingReference {
                    owner: Some(owner.clone()),
                    name,
                    kind: ReferenceKind::Implements,
                    node: error,
                },
            )?;
        }
    }
    Ok(())
}

fn capture_outer_type_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeReferenceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    let TypeReferenceCapture {
        node, owner, kind, ..
    } = input;
    let target = descendants_including_root(node).find(|candidate| {
        matches!(
            candidate.kind(),
            "user_type"
                | "generic_type"
                | "stable_type_identifier"
                | "type_identifier"
                | "dotted_identifier"
                | "identifier"
        )
    });
    let Some(target) = target else {
        return Ok(());
    };
    let Some(name) = safe_type_text(builder, target)? else {
        return Ok(());
    };
    push_named_reference(
        builder,
        PendingReference {
            owner: Some(owner.clone()),
            name,
            kind,
            node: target,
        },
    )
}

fn capture_type_references(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeReferenceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    let TypeReferenceCapture {
        node,
        owner,
        kind,
        depth,
    } = input;
    if depth > MAX_TYPE_DEPTH {
        return Err(ExtractError::NestingLimit);
    }
    builder.context.ensure_active()?;
    let language = builder.context.snapshot.language();
    let is_leaf = match language {
        SourceLanguage::Kotlin | SourceLanguage::Scala => node.kind() == "type_identifier",
        SourceLanguage::Groovy => matches!(node.kind(), "identifier" | "builtintype"),
        _ => false,
    };
    if is_leaf {
        if let Some(name) = safe_type_text(builder, node)?
            && !is_jvm_builtin(&name)
        {
            push_named_reference(
                builder,
                PendingReference {
                    owner: Some(owner.clone()),
                    name,
                    kind,
                    node,
                },
            )?;
        }
        return Ok(());
    }
    for child in named_children(node) {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                node: child,
                owner,
                kind,
                depth: depth.saturating_add(1),
            },
        )?;
    }
    Ok(())
}

fn capture_kotlin_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match node.kind() {
        "call_expression" => {
            let Some(target) = named_children(node).find(|child| child.kind() != "call_suffix")
            else {
                return Ok(());
            };
            let Some(name) = safe_reference_text(builder, target)? else {
                return Ok(());
            };
            let kind = if terminal_name(&name)
                .chars()
                .next()
                .is_some_and(char::is_uppercase)
            {
                ReferenceKind::Instantiates
            } else {
                ReferenceKind::Calls
            };
            push_named_reference(
                builder,
                PendingReference {
                    owner: builder.owners.last().cloned(),
                    name,
                    kind,
                    node: target,
                },
            )
        }
        "navigation_expression" if !is_kotlin_call_target(node) => {
            capture_terminal_member(builder, node)
        }
        _ => Ok(()),
    }
}

fn capture_scala_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match node.kind() {
        "call_expression" => {
            let Some(target) = node.child_by_field_name("function") else {
                return Ok(());
            };
            let Some(name) = safe_reference_text(builder, target)? else {
                return Ok(());
            };
            push_named_reference(
                builder,
                PendingReference {
                    owner: builder.owners.last().cloned(),
                    name,
                    kind: ReferenceKind::Calls,
                    node: target,
                },
            )
        }
        "instance_expression" => {
            let Some(target) = named_children(node).find(|child| child.kind() != "arguments")
            else {
                return Ok(());
            };
            let Some(name) = safe_type_text(builder, target)? else {
                return Ok(());
            };
            push_named_reference(
                builder,
                PendingReference {
                    owner: builder.owners.last().cloned(),
                    name,
                    kind: ReferenceKind::Instantiates,
                    node: target,
                },
            )
        }
        "field_expression" if !is_scala_call_target(node) => capture_terminal_member(builder, node),
        _ => Ok(()),
    }
}

fn capture_groovy_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match node.kind() {
        "function_call" | GROOVY_COMMAND_CALL
            if !is_groovy_construction_target(node) && is_groovy_call(builder, node)? =>
        {
            let Some(target) = node.child_by_field_name("function") else {
                return Ok(());
            };
            let Some(name) = safe_reference_text(builder, target)? else {
                return Ok(());
            };
            push_named_reference(
                builder,
                PendingReference {
                    owner: builder.owners.last().cloned(),
                    name,
                    kind: ReferenceKind::Calls,
                    node: target,
                },
            )
        }
        "unary_op" if builder.context.text(node).trim_start().starts_with("new ") => {
            let target = named_children(node)
                .find(|child| child.kind() == "function_call")
                .and_then(|call| call.child_by_field_name("function"));
            let Some(target) = target else {
                return Ok(());
            };
            let Some(name) = safe_reference_text(builder, target)? else {
                return Ok(());
            };
            push_named_reference(
                builder,
                PendingReference {
                    owner: builder.owners.last().cloned(),
                    name,
                    kind: ReferenceKind::Instantiates,
                    node: target,
                },
            )
        }
        "dotted_identifier"
            if !is_groovy_call_target(builder, node)?
                && !is_recovered_literal_word(builder, node)? =>
        {
            capture_terminal_member(builder, node)
        }
        _ => Ok(()),
    }
}

fn capture_terminal_member(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(target) = descendants_including_root(node)
        .filter(|candidate| {
            matches!(
                candidate.kind(),
                "simple_identifier" | "identifier" | "operator_identifier"
            )
        })
        .last()
    else {
        return Ok(());
    };
    let Some(name) = safe_reference_text(builder, target)? else {
        return Ok(());
    };
    push_named_reference(
        builder,
        PendingReference {
            owner: builder.owners.last().cloned(),
            name,
            kind: ReferenceKind::FieldAccess,
            node: target,
        },
    )
}

fn is_kotlin_call_target(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "call_expression"
            && named_children(parent)
                .find(|child| child.kind() != "call_suffix")
                .is_some_and(|target| same_node(target, node))
    })
}

fn is_scala_call_target(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "call_expression"
            && parent
                .child_by_field_name("function")
                .is_some_and(|target| same_node(target, node))
    })
}

fn is_groovy_call_target(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let Some(parent) = node.parent().filter(|parent| {
        matches!(parent.kind(), "function_call" | GROOVY_COMMAND_CALL)
            && parent
                .child_by_field_name("function")
                .is_some_and(|target| same_node(target, node))
    }) else {
        return Ok(false);
    };
    is_groovy_call(builder, parent)
}

/// Whether a Groovy call node is a call: it is not a word of an error-recovered
/// literal, and a juxtaposed command call (`receiver.method args`) calls
/// through a member path.
///
/// Error recovery turns the words of a string-named method into calls
/// (`def "greets with a tone"()` becomes `with a` and `tone "..."`,
/// `def "uses a.b c"()` becomes `a.b c` and `def "uses a.b() c"()` becomes
/// `a.b()`), so a call recovered inside a literal, or a bare juxtaposition, is
/// not trusted.
fn is_groovy_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    Ok(!is_recovered_literal_word(builder, node)?
        && (node.kind() != GROOVY_COMMAND_CALL
            || node
                .child_by_field_name("function")
                .is_some_and(|target| target.kind() == "dotted_identifier")))
}

/// Whether `node` is a word that error recovery lifted out of a literal: a
/// parse error ends earlier on its line and the line's text before it is
/// inside a literal or comment. Nodes in error-free surroundings are trusted
/// as the grammar parsed them, so Groovy slashy, dollar-slashy, triple-quoted
/// and multi-line literals the line re-lex does not model are never consulted.
fn is_recovered_literal_word(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    Ok(match follows_parse_error_on_line(builder, node)? {
        Some(false) => false,
        Some(true) => starts_inside_line_literal(builder.context.source(), node.start_byte()),
        None => true,
    })
}

/// Whether a parse error (an `ERROR` or missing node) ends on the row where
/// `node` starts, before it. Only ancestors starting on that row and parents
/// whose subtree holds an error are searched, so error-free trees cost one
/// `has_error` check per ancestor on the row.
fn follows_parse_error_on_line(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<bool>, ExtractError> {
    let start = node.start_position();
    let line = NodeLine {
        start_byte: node.start_byte().saturating_sub(start.column),
        row: start.row,
    };
    let mut current = node;
    let mut visits = 0;
    while let Some(parent) = current.parent() {
        builder.context.ensure_active()?;
        if parent.has_error() {
            if !charge_error_lookback(builder, &mut visits)? {
                return Ok(None);
            }
            match error_precedes_on_line(
                builder,
                ErrorLookback {
                    parent,
                    current,
                    line,
                },
                &mut visits,
            )? {
                Some(false) => {}
                result => return Ok(result),
            }
        }
        if parent.start_position().row != start.row {
            return Ok(Some(false));
        }
        current = parent;
    }
    Ok(Some(false))
}

/// Charge every examined node and poll cancellation before reading it.
fn charge_error_lookback(
    builder: &mut ExtractionBuilder<'_, '_>,
    visits: &mut usize,
) -> Result<bool, ExtractError> {
    builder.context.ensure_active()?;
    *visits = visits.saturating_add(1);
    Ok(*visits <= MAX_PARSE_ERROR_LOOKBACK_NODES)
}

/// The source line a node starts on: its first byte and its row.
#[derive(Clone, Copy)]
struct NodeLine {
    start_byte: usize,
    row: usize,
}

#[derive(Clone, Copy)]
struct ErrorLookback<'tree> {
    parent: Node<'tree>,
    current: Node<'tree>,
    line: NodeLine,
}

/// Whether a child of `parent` before `current` holds a parse error and ends
/// on `line`, at or after its start.
fn error_precedes_on_line(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ErrorLookback<'_>,
    visits: &mut usize,
) -> Result<Option<bool>, ExtractError> {
    let ErrorLookback {
        parent,
        current,
        line,
    } = input;
    let mut cursor = parent.walk();
    if !cursor.goto_first_child() {
        return Ok(Some(false));
    }
    loop {
        if !charge_error_lookback(builder, visits)? {
            return Ok(None);
        }
        let child = cursor.node();
        if child.start_byte() >= current.start_byte() || same_node(child, current) {
            return Ok(Some(false));
        }
        if child.has_error()
            && child.end_byte() >= line.start_byte
            && child.end_position().row == line.row
        {
            return Ok(Some(true));
        }
        if !cursor.goto_next_sibling() {
            return Ok(Some(false));
        }
    }
}

/// Whether `offset` lies inside a literal or comment opened earlier on its line
/// (Groovy `GString` `${...}` interpolations are code). When the line starts beyond the
/// scan bound the position is unknown and the caller abstains, as for a literal.
fn starts_inside_line_literal(source: &str, offset: usize) -> bool {
    let window_start = offset.saturating_sub(MAX_LINE_PREFIX_SCAN_BYTES);
    let Some(window) = source.as_bytes().get(window_start..offset) else {
        return true;
    };
    let line = match window.iter().rposition(|byte| *byte == b'\n') {
        Some(newline) => &window[newline + 1..],
        None if window_start == 0 => window,
        None => return true,
    };
    let mut scan = CodeScan::new(line).with_interpolation();
    scan.by_ref().for_each(drop);
    scan.in_literal()
}

fn is_groovy_construction_target(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "unary_op" && parent.child(0).is_some_and(|token| token.kind() == "new")
    })
}

fn same_node(left: Node<'_>, right: Node<'_>) -> bool {
    left.start_byte() == right.start_byte() && left.end_byte() == right.end_byte()
}

fn push_named_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    pending: PendingReference<'_>,
) -> Result<(), ExtractError> {
    references::push_reference(builder, pending)
}

fn safe_reference_text(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    for descendant in descendants_including_root(node) {
        builder.context.ensure_active()?;
        if is_literal_kind(descendant.kind()) {
            return Ok(None);
        }
    }
    normalize_reference(builder, builder.context.text(node).trim())
}

fn safe_type_text(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    normalize_reference(
        builder,
        builder.context.text(node).trim().trim_end_matches('?'),
    )
}

fn normalize_reference(
    builder: &ExtractionBuilder<'_, '_>,
    raw: &str,
) -> Result<Option<String>, ExtractError> {
    if raw.is_empty()
        || raw.len() > MAX_REFERENCE_TARGET_BYTES
        || super::specifier_safety::specifier_may_carry_credential(raw)
    {
        return Ok(None);
    }
    let mut normalized = String::new();
    normalized
        .try_reserve(raw.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    let mut state = ReferenceNormalization {
        text: normalized,
        generic_depth: 0,
    };
    let mut characters = raw.chars().peekable();
    while let Some(character) = characters.next() {
        if !state.accept(character, characters.peek().copied()) {
            return Ok(None);
        }
    }
    if !state.is_valid() {
        return Ok(None);
    }
    builder
        .context
        .budget
        .ensure_string_length(state.text.len())?;
    Ok(Some(state.text))
}

struct ReferenceNormalization {
    text: String,
    generic_depth: usize,
}

impl ReferenceNormalization {
    fn accept(&mut self, character: char, next: Option<char>) -> bool {
        match character {
            '<' | '[' => self.generic_depth = self.generic_depth.saturating_add(1),
            '>' | ']' if self.generic_depth > 0 => {
                self.generic_depth = self.generic_depth.saturating_sub(1);
            }
            _ if self.generic_depth > 0 => {}
            '?' if next == Some('.') => {}
            character if character.is_whitespace() => {}
            character
                if character.is_ascii_alphanumeric()
                    || matches!(character, '_' | '.' | ':' | '$') =>
            {
                self.text.push(character);
            }
            _ => return false,
        }
        true
    }

    fn is_valid(&self) -> bool {
        self.generic_depth == 0
            && !self.text.is_empty()
            && self.text.len() <= MAX_REFERENCE_TARGET_BYTES
            && !self.text.starts_with('.')
            && !self.text.ends_with('.')
    }
}

fn is_literal_kind(kind: &str) -> bool {
    kind.contains("string")
        || kind.contains("character")
        || kind.contains("number_literal")
        || kind.contains("integer_literal")
        || kind.contains("floating_point_literal")
        || matches!(
            kind,
            "boolean_literal" | "null" | "null_literal" | "real_literal"
        )
}

fn kotlin_parameters_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    parameters: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let values = named_children(parameters)
        .filter(|child| matches!(child.kind(), "parameter" | "class_parameter"));
    parameter_signature(
        builder,
        ParameterSignatureInput {
            parameters: values,
            style: ParameterStyle::NameColonType,
            return_type: None,
        },
    )
}

fn kotlin_callable_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    parameters: Node<'_>,
    return_type: Option<Node<'_>>,
) -> Result<Option<String>, ExtractError> {
    let values = named_children(parameters).filter(|child| child.kind() == "parameter");
    parameter_signature(
        builder,
        ParameterSignatureInput {
            parameters: values,
            style: ParameterStyle::NameColonType,
            return_type,
        },
    )
}

fn scala_parameters_signature<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    parameters: impl IntoIterator<Item = Node<'tree>>,
) -> Result<Option<String>, ExtractError> {
    let mut signature = String::new();
    for parameter_list in parameters {
        let Some(group) = parameter_signature(
            builder,
            ParameterSignatureInput {
                parameters: named_children(parameter_list)
                    .filter(|child| matches!(child.kind(), "parameter" | "class_parameter")),
                style: ParameterStyle::NameColonType,
                return_type: None,
            },
        )?
        else {
            return Ok(None);
        };
        if signature.len().saturating_add(group.len()) > MAX_SIGNATURE_BYTES {
            return Ok(None);
        }
        signature.push_str(&group);
    }
    safe_signature(builder, signature)
}

fn scala_callable_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    parameters: &[Node<'_>],
    return_type: Option<Node<'_>>,
) -> Result<Option<String>, ExtractError> {
    let mut signature = if parameters.is_empty() {
        String::new()
    } else {
        let Some(signature) = scala_parameters_signature(builder, parameters.iter().copied())?
        else {
            return Ok(None);
        };
        signature
    };
    append_return_type(builder, &mut signature, return_type)?;
    safe_signature(builder, signature)
}

fn groovy_callable_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    parameters: Node<'_>,
    return_type: Option<Node<'_>>,
) -> Result<Option<String>, ExtractError> {
    let values = named_children(parameters).filter(|child| child.kind() == "parameter");
    let Some(parameters) = parameter_signature(
        builder,
        ParameterSignatureInput {
            parameters: values,
            style: ParameterStyle::TypeSpaceName,
            return_type: None,
        },
    )?
    else {
        return Ok(None);
    };
    let Some(return_type) = return_type else {
        return Ok(Some(parameters));
    };
    let return_text = builder.context.text(return_type).trim();
    let length = return_text
        .len()
        .checked_add(parameters.len())
        .and_then(|length| length.checked_add(1))
        .ok_or(ExtractError::OutputLimit)?;
    if length > MAX_SIGNATURE_BYTES {
        return Ok(None);
    }
    let mut signature = String::new();
    signature
        .try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    signature.push_str(return_text);
    signature.push(' ');
    signature.push_str(&parameters);
    safe_signature(builder, signature)
}

#[derive(Clone, Copy)]
enum ParameterStyle {
    NameColonType,
    TypeSpaceName,
}

fn parameter_signature<'tree, Parameters>(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ParameterSignatureInput<'tree, Parameters>,
) -> Result<Option<String>, ExtractError>
where
    Parameters: IntoIterator<Item = Node<'tree>>,
{
    let ParameterSignatureInput {
        parameters,
        style,
        return_type,
    } = input;
    let mut signature = String::from("(");
    let mut first = true;
    for parameter in parameters {
        builder.context.ensure_active()?;
        let name_node = parameter
            .child_by_field_name("name")
            .or_else(|| kotlin_direct_name(parameter));
        let Some(name_node) = name_node else {
            return Ok(None);
        };
        let name = builder.context.text(name_node).trim();
        let type_node = parameter
            .child_by_field_name("type")
            .or_else(|| kotlin_type_node(parameter));
        let type_text = type_node.map(|node| builder.context.text(node).trim());
        let separator = if first { "" } else { ", " };
        let required = separator
            .len()
            .checked_add(name.len())
            .and_then(|length| {
                length.checked_add(type_text.map_or(0, |value| value.len().saturating_add(2)))
            })
            .ok_or(ExtractError::OutputLimit)?;
        if signature.len().saturating_add(required).saturating_add(1) > MAX_SIGNATURE_BYTES {
            return Ok(None);
        }
        signature.push_str(separator);
        match (style, type_text) {
            (ParameterStyle::NameColonType, Some(type_text)) => {
                signature.push_str(name);
                signature.push_str(": ");
                signature.push_str(type_text);
            }
            (ParameterStyle::TypeSpaceName, Some(type_text)) => {
                signature.push_str(type_text);
                signature.push(' ');
                signature.push_str(name);
            }
            (_, None) => signature.push_str(name),
        }
        first = false;
    }
    signature.push(')');
    append_return_type(builder, &mut signature, return_type)?;
    safe_signature(builder, signature)
}

fn append_return_type(
    builder: &ExtractionBuilder<'_, '_>,
    signature: &mut String,
    return_type: Option<Node<'_>>,
) -> Result<(), ExtractError> {
    let Some(return_type) = return_type else {
        return Ok(());
    };
    let return_text = builder.context.text(return_type).trim();
    let length = signature
        .len()
        .checked_add(return_text.len())
        .and_then(|length| length.checked_add(2))
        .ok_or(ExtractError::OutputLimit)?;
    if length > MAX_SIGNATURE_BYTES {
        signature.clear();
        return Ok(());
    }
    signature.push_str(": ");
    signature.push_str(return_text);
    Ok(())
}

fn safe_signature(
    builder: &ExtractionBuilder<'_, '_>,
    signature: String,
) -> Result<Option<String>, ExtractError> {
    if signature.is_empty()
        || signature.len() > MAX_SIGNATURE_BYTES
        || !callable_signature_is_literal_free(&signature)
    {
        return Ok(None);
    }
    builder
        .context
        .budget
        .ensure_string_length(signature.len())?;
    Ok(Some(signature))
}

fn kotlin_property_signature(
    builder: &ExtractionBuilder<'_, '_>,
    input: TypedSignatureInput<'_, '_>,
) -> Result<Option<String>, ExtractError> {
    keyword_typed_signature(builder, input)
}

fn keyword_typed_signature(
    builder: &ExtractionBuilder<'_, '_>,
    input: TypedSignatureInput<'_, '_>,
) -> Result<Option<String>, ExtractError> {
    let TypedSignatureInput {
        keyword,
        name,
        type_node,
    } = input;
    let type_text = type_node.map(|node| builder.context.text(node).trim());
    let length = keyword
        .len()
        .checked_add(name.len())
        .and_then(|length| length.checked_add(1))
        .and_then(|length| {
            length.checked_add(type_text.map_or(0, |value| value.len().saturating_add(2)))
        })
        .ok_or(ExtractError::OutputLimit)?;
    if length > MAX_SIGNATURE_BYTES {
        return Ok(None);
    }
    let mut signature = String::new();
    signature
        .try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    signature.push_str(keyword);
    signature.push(' ');
    signature.push_str(name);
    if let Some(type_text) = type_text {
        signature.push_str(": ");
        signature.push_str(type_text);
    }
    safe_signature(builder, signature)
}

fn groovy_typed_signature(
    builder: &ExtractionBuilder<'_, '_>,
    type_node: Option<Node<'_>>,
    name: &str,
) -> Result<Option<String>, ExtractError> {
    let Some(type_node) = type_node else {
        return Ok(None);
    };
    let type_text = builder.context.text(type_node).trim();
    let length = type_text
        .len()
        .checked_add(name.len())
        .and_then(|length| length.checked_add(1))
        .ok_or(ExtractError::OutputLimit)?;
    if length > MAX_SIGNATURE_BYTES {
        return Ok(None);
    }
    let mut signature = String::new();
    signature
        .try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    signature.push_str(type_text);
    signature.push(' ');
    signature.push_str(name);
    safe_signature(builder, signature)
}

fn capture_kotlin_parameter_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    parameters: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    for parameter in named_children(parameters)
        .filter(|child| matches!(child.kind(), "parameter" | "class_parameter"))
    {
        builder.context.ensure_active()?;
        if let Some(type_node) = kotlin_type_node(parameter) {
            capture_type_references(
                builder,
                TypeReferenceCapture {
                    node: type_node,
                    owner,
                    kind: ReferenceKind::TypeOf,
                    depth: 0,
                },
            )?;
        }
    }
    Ok(())
}

fn capture_dynamic_parameter_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    parameters: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    for parameter in named_children(parameters).filter(|child| child.kind() == "parameter") {
        builder.context.ensure_active()?;
        if let Some(type_node) = parameter.child_by_field_name("type") {
            capture_type_references(
                builder,
                TypeReferenceCapture {
                    node: type_node,
                    owner,
                    kind: ReferenceKind::TypeOf,
                    depth: 0,
                },
            )?;
        }
    }
    Ok(())
}

fn kotlin_direct_name(node: Node<'_>) -> Option<Node<'_>> {
    named_children(node).find(|child| child.kind() == "simple_identifier")
}

fn kotlin_type_node(node: Node<'_>) -> Option<Node<'_>> {
    named_children(node).find(|child| {
        matches!(
            child.kind(),
            "user_type"
                | "nullable_type"
                | "function_type"
                | "parenthesized_type"
                | "not_nullable_type"
        )
    })
}

fn kotlin_return_type<'tree>(node: Node<'tree>, parameters: Node<'tree>) -> Option<Node<'tree>> {
    named_children(node).find(|child| {
        child.start_byte() >= parameters.end_byte()
            && matches!(
                child.kind(),
                "user_type"
                    | "nullable_type"
                    | "function_type"
                    | "parenthesized_type"
                    | "not_nullable_type"
            )
    })
}

fn scala_pattern_name(pattern: Node<'_>) -> Option<Node<'_>> {
    if matches!(pattern.kind(), "identifier" | "operator_identifier") {
        Some(pattern)
    } else {
        descendants_including_root(pattern)
            .find(|child| matches!(child.kind(), "identifier" | "operator_identifier"))
    }
}

fn jvm_visibility(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<Visibility>, ExtractError> {
    for (keyword, visibility) in [
        ("private", Visibility::Private),
        ("protected", Visibility::Protected),
        ("internal", Visibility::Internal),
        ("public", Visibility::Public),
    ] {
        if has_modifier(builder, node, keyword)? {
            return Ok(Some(visibility));
        }
    }
    Ok(Some(Visibility::Public))
}

fn has_modifier(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    expected: &str,
) -> Result<bool, ExtractError> {
    for child in children(node) {
        builder.context.ensure_active()?;
        if modifier_token_matches(builder, child, expected) {
            return Ok(true);
        }
        if !matches!(child.kind(), "modifiers" | "modifier" | "access_modifier") {
            continue;
        }
        for token in children(child) {
            builder.context.ensure_active()?;
            if modifier_token_matches(builder, token, expected) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn modifier_token_matches(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    expected: &str,
) -> bool {
    if node.kind() == expected {
        return true;
    }
    matches!(
        node.kind(),
        "modifier"
            | "access_modifier"
            | "visibility_modifier"
            | "function_modifier"
            | "member_modifier"
            | "property_modifier"
            | "class_modifier"
            | "inheritance_modifier"
            | "parameter_modifier"
    ) && node.end_byte().saturating_sub(node.start_byte()) <= 32
        && builder.context.text(node).trim() == expected
}

fn direct_keyword(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    expected: &str,
) -> Result<bool, ExtractError> {
    for child in children(node) {
        builder.context.ensure_active()?;
        if child.kind() == expected
            || (child.end_byte().saturating_sub(child.start_byte()) <= 32
                && builder.context.text(child).trim() == expected)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

const JVM_BUILTIN_TYPES: &[&str] = &[
    "Any", "Boolean", "Byte", "Char", "Double", "Float", "Int", "Long", "Nothing", "Short", "Unit",
    "Void", "boolean", "byte", "char", "def", "double", "float", "int", "long", "short", "void",
];

fn is_jvm_builtin(name: &str) -> bool {
    JVM_BUILTIN_TYPES.contains(&name)
}

fn terminal_name(name: &str) -> &str {
    name.rsplit(['.', ':'])
        .find(|part| !part.is_empty())
        .unwrap_or(name)
}
