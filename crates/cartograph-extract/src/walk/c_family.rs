use cartograph_domain::{
    ReferenceKind, SourceLanguage, SymbolId, SymbolKind, Visibility,
    callable_signature_is_literal_free,
};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind};

use super::{
    ChildReferenceKind, ExtractionBuilder, PendingReference, PendingSymbol,
    capture_child_reference, current_owner_kind_in, references, safe_assignment_signature,
    syntax::{
        children, descendants_including_root, is_call_or_construction_target, named_children,
        span_for,
    },
};

const MAX_DECLARATOR_DEPTH: usize = 64;
const MAX_SIGNATURE_BYTES: usize = 512;
const MAX_REFERENCE_TARGET_BYTES: usize = 512;
const C_TYPE_OWNER_KINDS: &[SymbolKind] = &[
    SymbolKind::Class,
    SymbolKind::Struct,
    SymbolKind::Union,
    SymbolKind::Interface,
    SymbolKind::Enum,
    SymbolKind::TypeAlias,
];

#[derive(Clone, Copy)]
struct DeclaratorName<'tree> {
    node: Node<'tree>,
    scope: Option<Node<'tree>>,
}

#[derive(Clone, Copy)]
struct ContainerVisit<'tree> {
    node: Node<'tree>,
    depth: usize,
    kind: SymbolKind,
}

#[derive(Clone, Copy)]
struct NamedContainerVisit<'tree> {
    node: Node<'tree>,
    depth: usize,
    kind: SymbolKind,
    name_node: Node<'tree>,
}

#[derive(Clone, Copy)]
struct DeclaratorVisit<'tree> {
    node: Node<'tree>,
    depth: usize,
    field_declaration: bool,
}

#[derive(Clone, Copy)]
struct FunctionPrototypeVisit<'tree> {
    node: Node<'tree>,
    declarator: Node<'tree>,
    field_declaration: bool,
}

#[derive(Clone, Copy)]
struct FunctionTypeCapture<'tree, 'owner> {
    node: Node<'tree>,
    declarator: Node<'tree>,
    owner: &'owner SymbolId,
}

#[derive(Clone, Copy)]
struct TypeReferenceCapture<'tree, 'owner> {
    root: Node<'tree>,
    owner: &'owner SymbolId,
    kind: ReferenceKind,
}

#[derive(Clone, Copy)]
struct MacroFunctionInput<'tree> {
    node: Node<'tree>,
    declarator: Node<'tree>,
    parsed_name: DeclaratorName<'tree>,
}

#[derive(Clone, Copy)]
struct ScopeSymbolInput<'scope> {
    name: &'scope str,
    qualified_name: &'scope str,
    id: &'scope SymbolId,
    kind: SymbolKind,
}

struct CFunctionDeclaration<'tree> {
    node: Node<'tree>,
    declarator: Node<'tree>,
    name: DeclaratorName<'tree>,
    local_name: String,
    capture_types: bool,
}

struct EmittedCFunction<'tree> {
    id: SymbolId,
    kind: SymbolKind,
    body: Option<Node<'tree>>,
    local_name: String,
}

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    if let Some(kind) = c_container_kind(node.kind()) {
        visit_container(builder, ContainerVisit { node, depth, kind })?;
        return Ok(true);
    }
    if visit_c_directive_or_module(builder, node, depth)? {
        return Ok(true);
    }
    if visit_c_scope_or_callable(builder, node, depth)? {
        return Ok(true);
    }
    if visit_c_type_declaration(builder, node, depth)? {
        return Ok(true);
    }
    visit_c_declarator(builder, node, depth)
}

fn visit_c_directive_or_module(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    _depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "preproc_include" => visit_include(builder, node)?,
        "preproc_def" => visit_macro_constant(builder, node)?,
        "import_statement" => visit_slang_import(builder, node)?,
        "module_declaration" => visit_slang_module(builder, node)?,
        _ => return Ok(false),
    }
    Ok(true)
}

fn visit_c_scope_or_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "function_definition" => visit_function(builder, node, depth)?,
        "namespace_definition" => visit_namespace(builder, node, depth)?,
        "access_specifier" => visit_access_specifier(builder, node),
        _ => return Ok(false),
    }
    Ok(true)
}

fn visit_c_type_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "type_definition" => visit_type_definition(builder, node, depth)?,
        "alias_declaration" => visit_alias(builder, node)?,
        "enumerator" => visit_named_leaf(builder, node, SymbolKind::EnumMember)?,
        _ => return Ok(false),
    }
    Ok(true)
}

fn visit_c_declarator(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "field_declaration" => visit_declarator_symbols(
            builder,
            DeclaratorVisit {
                node,
                depth,
                field_declaration: true,
            },
        )?,
        "declaration" => visit_declarator_symbols(
            builder,
            DeclaratorVisit {
                node,
                depth,
                field_declaration: false,
            },
        )?,
        _ => return Ok(false),
    }
    Ok(true)
}

fn c_container_kind(node_kind: &str) -> Option<SymbolKind> {
    [
        ("class_specifier", SymbolKind::Class),
        ("struct_specifier", SymbolKind::Struct),
        ("union_specifier", SymbolKind::Union),
        ("enum_specifier", SymbolKind::Enum),
        ("interface_specifier", SymbolKind::Interface),
    ]
    .into_iter()
    .find_map(|(candidate, kind)| (candidate == node_kind).then_some(kind))
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match node.kind() {
        "call_expression" => capture_child_reference(builder, node, ChildReferenceKind::CCall),
        "new_expression" => capture_construction(builder, node),
        "field_expression" => capture_field(builder, node),
        _ => Ok(()),
    }
}

fn capture_construction(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if !matches!(
        builder.context.snapshot.language(),
        SourceLanguage::Cpp | SourceLanguage::Cuda
    ) {
        return Ok(());
    }
    let Some(owner) = builder.owners.last().cloned() else {
        return Ok(());
    };
    let Some(type_node) = node.child_by_field_name("type") else {
        return Ok(());
    };
    for descendant in descendants_including_root(type_node) {
        builder.context.ensure_active()?;
        let kind = descendant.kind();
        if kind.contains("string")
            || kind.contains("character")
            || kind.contains("number_literal")
            || kind.contains("raw_literal")
        {
            return Ok(());
        }
    }
    let raw = builder.context.text(type_node).trim();
    let bare = raw
        .find(['<', '['])
        .and_then(|index| raw.get(..index))
        .unwrap_or(raw)
        .trim()
        .trim_start_matches("::");
    if bare.is_empty()
        || bare.len() > MAX_REFERENCE_TARGET_BYTES
        || is_builtin_c_type(bare)
        || !bare
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':'))
    {
        return Ok(());
    }
    let name = builder.context.copy_text(bare)?;
    references::push_reference(
        builder,
        PendingReference {
            owner: Some(owner),
            name,
            kind: ReferenceKind::Instantiates,
            node: type_node,
        },
    )
}

const C_BUILTIN_TYPES: &[&str] = &[
    "bool", "char", "char8_t", "char16_t", "char32_t", "double", "float", "int", "long", "short",
    "signed", "unsigned", "void", "wchar_t",
];

fn is_builtin_c_type(name: &str) -> bool {
    C_BUILTIN_TYPES.contains(&name)
}

fn visit_include(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(path_node) = node
        .child_by_field_name("path")
        .or_else(|| named_children(node).find(|child| is_include_path(child.kind())))
    else {
        return Ok(());
    };
    let raw_path = builder.context.text(path_node).trim();
    let module_name = raw_path
        .strip_circumfix('<', '>')
        .or_else(|| raw_path.strip_circumfix('"', '"'))
        .unwrap_or(raw_path);
    if module_name.is_empty()
        || super::specifier_safety::specifier_may_carry_credential(module_name)
    {
        return Ok(());
    }
    let name = builder.context.copy_text(module_name)?;
    let binding_kind = if raw_path.starts_with('"') {
        ImportBindingKind::IncludeQuoted
    } else {
        ImportBindingKind::IncludeSystem
    };
    let signature = builder
        .context
        .copy_text(builder.context.text(node).trim())?;
    let symbol = PendingSymbol {
        kind: SymbolKind::Import,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: Some(signature),
        export: crate::SymbolExportFlags::new(false, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    };
    builder.emit_symbol(symbol)?;
    builder.emit_import_binding(ExtractedImportBinding {
        kind: binding_kind,
        module_specifier: name.clone(),
        imported_name: "*".to_owned(),
        local_name: "*".to_owned(),
        span: span_for(path_node)?,
    })?;
    references::push_reference(
        builder,
        PendingReference {
            owner: None,
            name,
            kind: ReferenceKind::Imports,
            node: path_node,
        },
    )
}

fn is_include_path(kind: &str) -> bool {
    matches!(kind, "system_lib_string" | "string_literal")
}

fn visit_macro_constant(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let signature = if let Some(value) = node.child_by_field_name("value") {
        safe_assignment_signature(builder, value)?
    } else {
        None
    };
    let symbol = PendingSymbol {
        kind: SymbolKind::Constant,
        name: builder.context.owned_text(name_node)?,
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
    };
    builder.emit_symbol(symbol).map(|_| ())
}

fn macro_decorated_function_name<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    input: MacroFunctionInput<'tree>,
) -> Option<DeclaratorName<'tree>> {
    input.node.child_by_field_name("body")?;
    if input
        .node
        .prev_named_sibling()
        .is_some_and(|previous| previous.kind() == "declaration")
        && input.declarator.kind() == "parenthesized_declarator"
        && let Some(name) = input.node.child_by_field_name("type")
        && matches!(name.kind(), "identifier" | "type_identifier")
    {
        return Some(DeclaratorName {
            node: name,
            scope: None,
        });
    }
    let type_node = input.node.child_by_field_name("type")?;
    let type_name = builder.context.text(type_node).trim();
    if !is_uppercase_macro_name(type_name) {
        return None;
    }
    let prefix = builder
        .context
        .source()
        .get(input.node.start_byte()..input.parsed_name.node.start_byte())?;
    let mut tokens = prefix
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .filter(|token| !token.is_empty());
    (tokens.next() == Some(type_name) && tokens.next().is_some()).then_some(input.parsed_name)
}

fn macro_obscured_container<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'tree>,
) -> Result<Option<(SymbolKind, Node<'tree>)>, ExtractError> {
    if builder.context.snapshot.language() != SourceLanguage::Cpp
        || !node.has_error()
        || node.child_by_field_name("body").is_none()
    {
        return Ok(None);
    }
    let Some(macro_node) = node.child_by_field_name("type") else {
        return Ok(None);
    };
    let macro_name = builder.context.text(macro_node).trim();
    if macro_node.kind() != "type_identifier" || !is_uppercase_macro_name(macro_name) {
        return Ok(None);
    }
    ast_container_declaration(builder, node)
}

fn is_uppercase_macro_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        && name.bytes().any(|byte| byte.is_ascii_uppercase())
}

fn ast_container_declaration<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'tree>,
) -> Result<Option<(SymbolKind, Node<'tree>)>, ExtractError> {
    for child in named_children(node) {
        builder.context.ensure_active()?;
        if child.kind() == "compound_statement" {
            break;
        }
        if let Some((kind, name)) = container_declaration_in_subtree(builder, child)? {
            return Ok(Some((kind, name)));
        }
        if let Some(kind) = standalone_container_keyword(builder, child)?
            && let Some(next) = child.next_named_sibling()
            && next.kind() != "compound_statement"
            && let Some(name) = declarator_name(next)?
        {
            return Ok(Some((kind, name.node)));
        }
    }
    Ok(None)
}

fn container_declaration_in_subtree<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'tree>,
) -> Result<Option<(SymbolKind, Node<'tree>)>, ExtractError> {
    for candidate in descendants_including_root(root) {
        builder.context.ensure_active()?;
        if candidate.kind().contains("comment") {
            continue;
        }
        if candidate.kind() == "qualified_identifier"
            && let Some(scope) = candidate.child_by_field_name("scope")
            && let Some(kind) = container_kind(builder.context.text(scope).trim())
            && let Some(raw_name) = candidate.child_by_field_name("name")
        {
            let name = declarator_name(raw_name)?.map_or(raw_name, |name| name.node);
            return Ok(Some((kind, name)));
        }
        let Some(kind) = container_kind(builder.context.text(candidate).trim()) else {
            continue;
        };
        if let Some(next) = candidate.next_named_sibling()
            && let Some(name) = declarator_name(next)?
        {
            return Ok(Some((kind, name.node)));
        }
    }
    Ok(None)
}

fn standalone_container_keyword(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<Option<SymbolKind>, ExtractError> {
    for candidate in descendants_including_root(root) {
        builder.context.ensure_active()?;
        if candidate.kind().contains("comment") {
            continue;
        }
        if let Some(kind) = container_kind(builder.context.text(candidate).trim()) {
            return Ok(Some(kind));
        }
    }
    Ok(None)
}

fn container_kind(keyword: &str) -> Option<SymbolKind> {
    match keyword {
        "class" => Some(SymbolKind::Class),
        "struct" => Some(SymbolKind::Struct),
        "union" => Some(SymbolKind::Union),
        _ => None,
    }
}

fn visit_function(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    if let Some((kind, name_node)) = macro_obscured_container(builder, node)? {
        return visit_named_container(
            builder,
            NamedContainerVisit {
                node,
                depth,
                kind,
                name_node,
            },
        );
    }
    let Some(declaration) = parse_c_function_declaration(builder, node)? else {
        return builder.visit_named_children(node, depth);
    };
    let scope_name = declaration
        .name
        .scope
        .map(|scope| owned_declarator_scope(builder, scope, declaration.name.node))
        .transpose()?;
    let restore = enter_c_function_scope(builder, scope_name.as_deref())?;
    let emitted = emit_c_function(builder, declaration)?;
    let result = visit_c_function_body(builder, depth, emitted);
    restore_c_function_scope(builder, restore);
    result
}

fn parse_c_function_declaration<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'tree>,
) -> Result<Option<CFunctionDeclaration<'tree>>, ExtractError> {
    let Some(declarator) = node.child_by_field_name("declarator") else {
        return Ok(None);
    };
    let Some(parsed_name) = declarator_name(declarator)? else {
        return Ok(None);
    };
    let recovered_name = macro_decorated_function_name(
        builder,
        MacroFunctionInput {
            node,
            declarator,
            parsed_name,
        },
    );
    let name = recovered_name.unwrap_or(parsed_name);
    let local_name = builder.context.owned_text(name.node)?;
    if is_control_keyword(&local_name) {
        return Ok(None);
    }
    Ok(Some(CFunctionDeclaration {
        node,
        declarator,
        name,
        local_name,
        capture_types: recovered_name.is_none(),
    }))
}

fn emit_c_function<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: CFunctionDeclaration<'tree>,
) -> Result<EmittedCFunction<'tree>, ExtractError> {
    let kind = if current_owner_kind_in(builder, C_TYPE_OWNER_KINDS) {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    };
    let body = declaration.node.child_by_field_name("body");
    let mut signature = if declaration.capture_types {
        c_callable_signature(builder, declaration.node, declaration.declarator)?
    } else {
        None
    };
    let shader_stage = slang_shader_stage(builder, declaration.node)?;
    if let Some(stage) = shader_stage.as_deref() {
        let mut staged = format!("shader:{stage}");
        if let Some(existing) = signature.take() {
            staged.push(' ');
            staged.push_str(&existing);
        }
        signature = Some(staged);
    }
    let visibility = shader_stage
        .as_ref()
        .map(|_| Visibility::Public)
        .or_else(|| cxx_visibility(builder));
    let pending = PendingSymbol {
        kind,
        name: declaration.local_name.clone(),
        span_node: declaration.node,
        structural_node: declaration.node,
        doc_anchor: declaration.node,
        body_node: body,
        declaration_only: body.is_none(),
        signature,
        export: crate::SymbolExportFlags::named(
            shader_stage.is_some() || has_external_linkage(builder, declaration.node),
        ),
        async_symbol: false,
        static_member: current_owner_kind_in(builder, C_TYPE_OWNER_KINDS)
            && declaration_has_storage(builder, declaration.node, "static"),
        visibility,
    };
    let id = builder.emit_symbol(pending)?;
    if declaration.capture_types {
        capture_function_types(
            builder,
            FunctionTypeCapture {
                node: declaration.node,
                declarator: declaration.declarator,
                owner: &id,
            },
        )?;
    }
    Ok(EmittedCFunction {
        id,
        kind,
        body,
        local_name: declaration.local_name,
    })
}

fn visit_c_function_body(
    builder: &mut ExtractionBuilder<'_, '_>,
    depth: usize,
    function: EmittedCFunction<'_>,
) -> Result<(), ExtractError> {
    if let Some(body) = function.body {
        builder.owners.push(function.id);
        builder.native_owner_kinds.push(function.kind);
        builder.qualifiers.push(function.local_name);
        let result = builder.visit(body, depth.saturating_add(1));
        builder.qualifiers.pop();
        builder.native_owner_kinds.pop();
        builder.owners.pop();
        result?;
    }
    Ok(())
}

fn visit_slang_module(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if builder.context.snapshot.language() != SourceLanguage::Slang {
        return Ok(());
    }
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    if name.is_empty() {
        return Ok(());
    }
    builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Module,
        name,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: true,
        signature: None,
        export: crate::SymbolExportFlags::new(true, false),
        async_symbol: false,
        static_member: false,
        visibility: Some(Visibility::Public),
    })?;
    Ok(())
}

fn visit_slang_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if builder.context.snapshot.language() != SourceLanguage::Slang {
        return Ok(());
    }
    let raw = builder.context.text(node).trim();
    let Some(module) = raw
        .strip_prefix("import")
        .map(str::trim)
        .map(|value| value.trim_end_matches(';').trim())
        .filter(|value| !value.is_empty() && value.len() <= MAX_REFERENCE_TARGET_BYTES)
    else {
        return Ok(());
    };
    if !module
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
    {
        return Ok(());
    }
    let module = module
        .split('.')
        .map(|component| component.replace('_', "-"))
        .collect::<Vec<_>>()
        .join("/");
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
        export: crate::SymbolExportFlags::new(false, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })?;
    let span = span_for(node)?;
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: module.clone(),
        imported_name: "*".to_owned(),
        local_name: "*".to_owned(),
        span,
    })?;
    builder.emit_reference(crate::ExtractedReference {
        owner: None,
        name: module,
        resolution_name: None,
        kind: ReferenceKind::Imports,
        span,
    })
}

fn slang_shader_stage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    if builder.context.snapshot.language() != SourceLanguage::Slang {
        return Ok(None);
    }
    if let Some(stage) = slang_shader_stage_from_text(builder.context.text(node)) {
        return Ok(Some(builder.context.copy_text(stage)?));
    }
    let mut sibling = node.prev_named_sibling();
    for _ in 0..8 {
        let Some(attribute) = sibling else {
            break;
        };
        if !matches!(
            attribute.kind(),
            "ERROR" | "hlsl_attribute" | "attribute_declaration" | "attribute_specifier"
        ) {
            break;
        }
        if let Some(stage) = slang_shader_stage_from_text(builder.context.text(attribute)) {
            return Ok(Some(builder.context.copy_text(stage)?));
        }
        sibling = attribute.prev_named_sibling();
    }
    Ok(None)
}

fn slang_shader_stage_from_text(raw: &str) -> Option<&str> {
    const STAGES: &[&str] = &[
        "amplification",
        "anyhit",
        "callable",
        "closesthit",
        "compute",
        "domain",
        "fragment",
        "geometry",
        "hull",
        "intersection",
        "mesh",
        "miss",
        "raygeneration",
        "vertex",
    ];
    let attribute_start = raw.find("shader")?;
    let tail = &raw[attribute_start + "shader".len()..];
    let open = tail.find('(')?;
    let close = tail[open + 1..].find(')')?;
    let stage = tail[open + 1..open + 1 + close].trim().trim_matches('"');
    if !STAGES.contains(&stage) {
        return None;
    }
    Some(stage)
}

#[derive(Clone, Copy)]
struct CFunctionScopeDepths {
    owners: usize,
    kinds: usize,
    qualifiers: usize,
}

fn enter_c_function_scope(
    builder: &mut ExtractionBuilder<'_, '_>,
    scope: Option<&str>,
) -> Result<CFunctionScopeDepths, ExtractError> {
    let depths = CFunctionScopeDepths {
        owners: builder.owners.len(),
        kinds: builder.native_owner_kinds.len(),
        qualifiers: builder.qualifiers.len(),
    };
    let Some(scope) = scope.filter(|_| !current_owner_kind_in(builder, C_TYPE_OWNER_KINDS)) else {
        return Ok(depths);
    };
    let scoped_owner = find_scope_symbol(builder, scope)?;
    let scope_kind = scoped_owner
        .as_ref()
        .map_or_else(|| inferred_scope_kind(scope), |(_, kind)| *kind);
    if let Some((owner, _)) = &scoped_owner
        && builder.owners.last() != Some(owner)
    {
        builder.owners.push(owner.clone());
    }
    builder.native_owner_kinds.push(scope_kind);
    let relative_scope = relative_scope_qualifier(builder, scope)?;
    if !relative_scope.is_empty() {
        builder.qualifiers.push(relative_scope);
    }
    Ok(depths)
}

fn restore_c_function_scope(builder: &mut ExtractionBuilder<'_, '_>, depths: CFunctionScopeDepths) {
    builder.owners.truncate(depths.owners);
    builder.native_owner_kinds.truncate(depths.kinds);
    builder.qualifiers.truncate(depths.qualifiers);
}

fn visit_container(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ContainerVisit<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = input.node.child_by_field_name("name") else {
        return builder.visit_named_children(input.node, input.depth);
    };
    visit_named_container(
        builder,
        NamedContainerVisit {
            node: input.node,
            depth: input.depth,
            kind: input.kind,
            name_node,
        },
    )
}

fn visit_namespace(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(body) = node.child_by_field_name("body") else {
        return Ok(());
    };
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit(body, depth.saturating_add(1));
    };
    let name = builder.context.owned_text(name_node)?;
    let pending = PendingSymbol {
        kind: SymbolKind::Namespace,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: crate::SymbolExportFlags::new(true, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    };
    let qualified_name = builder.qualified_name(&name)?;
    let id = builder.emit_symbol(pending)?;
    register_scope_symbol(
        builder,
        ScopeSymbolInput {
            name: &name,
            qualified_name: &qualified_name,
            id: &id,
            kind: SymbolKind::Namespace,
        },
    )?;
    builder.owners.push(id);
    builder.native_owner_kinds.push(SymbolKind::Namespace);
    builder.qualifiers.push(name);
    let result = builder.visit(body, depth.saturating_add(1));
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn visit_alias(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    let pending = PendingSymbol {
        kind: SymbolKind::TypeAlias,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: crate::SymbolExportFlags::named(has_external_linkage(builder, node)),
        async_symbol: false,
        static_member: false,
        visibility: cxx_visibility(builder),
    };
    let qualified_name = builder.qualified_name(&name)?;
    let id = builder.emit_symbol(pending)?;
    register_scope_symbol(
        builder,
        ScopeSymbolInput {
            name: &name,
            qualified_name: &qualified_name,
            id: &id,
            kind: SymbolKind::TypeAlias,
        },
    )?;
    if let Some(target) = node.child_by_field_name("type") {
        capture_alias_target_references(builder, target, &id)?;
    }
    Ok(())
}

fn visit_named_container(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: NamedContainerVisit<'_>,
) -> Result<(), ExtractError> {
    let name = builder.context.owned_text(input.name_node)?;
    let body = input.node.child_by_field_name("body");
    let pending = PendingSymbol {
        kind: input.kind,
        name: name.clone(),
        span_node: input.node,
        structural_node: input.node,
        doc_anchor: input.node,
        body_node: None,
        declaration_only: body.is_none(),
        signature: None,
        export: crate::SymbolExportFlags::named(has_external_linkage(builder, input.node)),
        async_symbol: false,
        static_member: false,
        visibility: cxx_visibility(builder),
    };
    let qualified_name = builder.qualified_name(&name)?;
    let id = builder.emit_symbol(pending)?;
    register_scope_symbol(
        builder,
        ScopeSymbolInput {
            name: &name,
            qualified_name: &qualified_name,
            id: &id,
            kind: input.kind,
        },
    )?;
    capture_base_classes(builder, input.node, &id)?;
    let Some(body) = body else {
        return Ok(());
    };
    builder.owners.push(id);
    builder.native_owner_kinds.push(input.kind);
    let visibility = default_cxx_visibility(builder, input.kind);
    builder.native_visibilities.push(visibility);
    builder.qualifiers.push(name);
    let result = builder.visit(body, input.depth.saturating_add(1));
    builder.qualifiers.pop();
    builder.native_visibilities.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn visit_type_definition(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(declarator) = node.child_by_field_name("declarator") else {
        return builder.visit_named_children(node, depth);
    };
    let Some(name_node) = declarator_name(declarator)?.map(|name| name.node) else {
        return builder.visit_named_children(node, depth);
    };
    let type_node = node.child_by_field_name("type");
    let kind = match type_node {
        Some(type_node)
            if type_node.kind() == "struct_specifier"
                && type_node.child_by_field_name("body").is_some() =>
        {
            SymbolKind::Struct
        }
        Some(type_node)
            if type_node.kind() == "enum_specifier"
                && type_node.child_by_field_name("body").is_some() =>
        {
            SymbolKind::Enum
        }
        Some(type_node)
            if type_node.kind() == "union_specifier"
                && type_node.child_by_field_name("body").is_some() =>
        {
            SymbolKind::Union
        }
        _ => SymbolKind::TypeAlias,
    };
    let name = builder.context.owned_text(name_node)?;
    let pending = PendingSymbol {
        kind,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: crate::SymbolExportFlags::named(has_external_linkage(builder, node)),
        async_symbol: false,
        static_member: false,
        visibility: None,
    };
    let qualified_name = builder.qualified_name(&name)?;
    let id = builder.emit_symbol(pending)?;
    register_scope_symbol(
        builder,
        ScopeSymbolInput {
            name: &name,
            qualified_name: &qualified_name,
            id: &id,
            kind,
        },
    )?;
    let Some(body) = type_node.and_then(|node| node.child_by_field_name("body")) else {
        if let Some(target) = type_node {
            capture_alias_target_references(builder, target, &id)?;
        }
        return Ok(());
    };
    builder.owners.push(id);
    builder.native_owner_kinds.push(kind);
    let visibility = default_cxx_visibility(builder, kind);
    builder.native_visibilities.push(visibility);
    builder.qualifiers.push(name);
    let result = builder.visit(body, depth.saturating_add(1));
    builder.qualifiers.pop();
    builder.native_visibilities.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn visit_named_leaf(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    kind: SymbolKind,
) -> Result<(), ExtractError> {
    let Some(name_node) = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("declarator"))
    else {
        return Ok(());
    };
    let name_node = declarator_name(name_node)?.map_or(name_node, |name| name.node);
    let pending = PendingSymbol {
        kind,
        name: builder.context.owned_text(name_node)?,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: crate::SymbolExportFlags::named(has_external_linkage(builder, node)),
        async_symbol: false,
        static_member: false,
        visibility: cxx_visibility(builder),
    };
    builder.emit_symbol(pending).map(|_| ())
}

fn visit_declarator_symbols(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: DeclaratorVisit<'_>,
) -> Result<(), ExtractError> {
    let inside_callable = matches!(
        builder.native_owner_kinds.last(),
        Some(SymbolKind::Function | SymbolKind::Method)
    );
    if inside_callable {
        return builder.visit_named_children(input.node, input.depth);
    }
    let kind = if input.field_declaration || current_owner_kind_in(builder, C_TYPE_OWNER_KINDS) {
        SymbolKind::Field
    } else if declaration_is_const(builder, input.node) {
        SymbolKind::Constant
    } else {
        SymbolKind::Variable
    };
    let mut cursor = input.node.walk();
    for declarator in input.node.children_by_field_name("declarator", &mut cursor) {
        builder.context.ensure_active()?;
        let function_declarator = descendants_including_root(declarator)
            .find(|candidate| candidate.kind() == "function_declarator");
        if let Some(function_declarator) = function_declarator
            && !is_function_pointer_declarator(function_declarator)
        {
            visit_function_prototype(
                builder,
                FunctionPrototypeVisit {
                    node: input.node,
                    declarator: function_declarator,
                    field_declaration: input.field_declaration,
                },
            )?;
            continue;
        }
        let Some(name) = declarator_name(declarator)? else {
            continue;
        };
        let pending = PendingSymbol {
            kind,
            name: builder.context.owned_text(name.node)?,
            span_node: declarator,
            structural_node: declarator,
            doc_anchor: input.node,
            body_node: None,
            declaration_only: false,
            signature: None,
            export: crate::SymbolExportFlags::named(has_external_linkage(builder, input.node)),
            async_symbol: false,
            static_member: current_owner_kind_in(builder, C_TYPE_OWNER_KINDS)
                && declaration_has_storage(builder, input.node, "static"),
            visibility: cxx_visibility(builder),
        };
        let id = builder.emit_symbol(pending)?;
        if let Some(type_node) = input.node.child_by_field_name("type") {
            capture_type_references(
                builder,
                TypeReferenceCapture {
                    root: type_node,
                    owner: &id,
                    kind: ReferenceKind::TypeOf,
                },
            )?;
        }
        capture_type_references(
            builder,
            TypeReferenceCapture {
                root: declarator,
                owner: &id,
                kind: ReferenceKind::TypeOf,
            },
        )?;
    }
    builder.visit_named_children(input.node, input.depth)
}

fn is_function_pointer_declarator(function_declarator: Node<'_>) -> bool {
    function_declarator
        .child_by_field_name("declarator")
        .is_some_and(|declarator| {
            descendants_including_root(declarator)
                .any(|candidate| candidate.kind() == "pointer_declarator")
        })
}

fn visit_function_prototype(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: FunctionPrototypeVisit<'_>,
) -> Result<(), ExtractError> {
    let Some(name) = declarator_name(input.declarator)? else {
        return Ok(());
    };
    let local_name = builder.context.owned_text(name.node)?;
    let scope_name = name
        .scope
        .map(|scope| owned_declarator_scope(builder, scope, name.node))
        .transpose()?;
    let scoped_owner = scope_name
        .as_deref()
        .map(|scope| find_scope_symbol(builder, scope))
        .transpose()?
        .flatten();
    let existing_owner_depth = builder.owners.len();
    let existing_kind_depth = builder.native_owner_kinds.len();
    let existing_qualifier_depth = builder.qualifiers.len();
    if let Some(scope) = &scope_name
        && !current_owner_kind_in(builder, C_TYPE_OWNER_KINDS)
    {
        let scope_kind = scoped_owner
            .as_ref()
            .map_or_else(|| inferred_scope_kind(scope), |(_, kind)| *kind);
        if let Some((owner, _)) = &scoped_owner
            && builder.owners.last() != Some(owner)
        {
            builder.owners.push(owner.clone());
        }
        builder.native_owner_kinds.push(scope_kind);
        let relative_scope = relative_scope_qualifier(builder, scope)?;
        if !relative_scope.is_empty() {
            builder.qualifiers.push(relative_scope);
        }
    }
    let kind = if input.field_declaration || current_owner_kind_in(builder, C_TYPE_OWNER_KINDS) {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    };
    let pending = PendingSymbol {
        kind,
        name: local_name,
        span_node: input.node,
        structural_node: input.node,
        doc_anchor: input.node,
        body_node: None,
        declaration_only: true,
        signature: c_callable_signature(builder, input.node, input.declarator)?,
        export: crate::SymbolExportFlags::named(has_external_linkage(builder, input.node)),
        async_symbol: false,
        static_member: current_owner_kind_in(builder, C_TYPE_OWNER_KINDS)
            && declaration_has_storage(builder, input.node, "static"),
        visibility: cxx_visibility(builder),
    };
    let id = builder.emit_symbol(pending)?;
    capture_function_types(
        builder,
        FunctionTypeCapture {
            node: input.node,
            declarator: input.declarator,
            owner: &id,
        },
    )?;
    builder.owners.truncate(existing_owner_depth);
    builder.native_owner_kinds.truncate(existing_kind_depth);
    builder.qualifiers.truncate(existing_qualifier_depth);
    Ok(())
}

fn declaration_is_const(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> bool {
    named_children(node).any(|child| {
        child.kind() == "type_qualifier" && builder.context.text(child).trim() == "const"
    })
}

fn c_callable_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    declarator: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let parameters = descendants_including_root(declarator)
        .find(|candidate| candidate.kind() == "parameter_list");
    let Some(parameters) = parameters else {
        return Ok(None);
    };
    let parameter_text = builder.context.text(parameters).trim();
    let return_text = node
        .child_by_field_name("type")
        .map(|return_type| builder.context.text(return_type).trim())
        .filter(|value| !value.is_empty());
    let length = parameter_text
        .len()
        .checked_add(return_text.map_or(0, |value| value.len().saturating_add(1)))
        .ok_or(ExtractError::OutputLimit)?;
    if length > MAX_SIGNATURE_BYTES {
        return Ok(None);
    }
    builder.context.budget.ensure_string_length(length)?;
    let mut signature = String::new();
    signature
        .try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    if let Some(return_text) = return_text {
        signature.push_str(return_text);
        signature.push(' ');
    }
    signature.push_str(parameter_text);
    if !callable_signature_is_literal_free(&signature) {
        return Ok(None);
    }
    Ok(Some(signature))
}

fn capture_function_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: FunctionTypeCapture<'_, '_>,
) -> Result<(), ExtractError> {
    if let Some(return_type) = input.node.child_by_field_name("type") {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                root: return_type,
                owner: input.owner,
                kind: ReferenceKind::Returns,
            },
        )?;
    }
    if let Some(parameters) = descendants_including_root(input.declarator)
        .find(|candidate| candidate.kind() == "parameter_list")
    {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                root: parameters,
                owner: input.owner,
                kind: ReferenceKind::TypeOf,
            },
        )?;
    }
    Ok(())
}

fn capture_base_classes(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    for clause in named_children(node).filter(|child| child.kind() == "base_class_clause") {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                root: clause,
                owner,
                kind: ReferenceKind::Extends,
            },
        )?;
    }
    Ok(())
}

fn capture_type_references(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeReferenceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    for target in descendants_including_root(input.root) {
        builder.context.ensure_active()?;
        if target.kind() != "type_identifier" {
            continue;
        }
        let name = builder.context.owned_text(target)?;
        references::push_reference(
            builder,
            PendingReference {
                owner: Some(input.owner.clone()),
                name,
                kind: input.kind,
                node: target,
            },
        )?;
    }
    Ok(())
}

fn capture_alias_target_references(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    let mut qualified_found = false;
    for target in descendants_including_root(root) {
        builder.context.ensure_active()?;
        if target.kind() != "qualified_identifier"
            || target
                .parent()
                .is_some_and(|parent| parent.kind() == "qualified_identifier")
        {
            continue;
        }
        let raw = builder.context.text(target).trim();
        if raw.is_empty()
            || raw.len() > MAX_REFERENCE_TARGET_BYTES
            || !raw
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':'))
        {
            continue;
        }
        let name = builder.context.copy_text(raw.trim_start_matches("::"))?;
        references::push_reference(
            builder,
            PendingReference {
                owner: Some(owner.clone()),
                name,
                kind: ReferenceKind::TypeOf,
                node: target,
            },
        )?;
        qualified_found = true;
    }
    if qualified_found {
        Ok(())
    } else {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                root,
                owner,
                kind: ReferenceKind::TypeOf,
            },
        )
    }
}

fn capture_field(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if is_call_or_construction_target(node) {
        return Ok(());
    }
    let Some(field) = node.child_by_field_name("field") else {
        return Ok(());
    };
    let name = builder.context.owned_text(field)?;
    references::push_reference(
        builder,
        PendingReference {
            owner: builder.owners.last().cloned(),
            name,
            kind: ReferenceKind::FieldAccess,
            node: field,
        },
    )
}

pub(super) fn safe_call_target(
    builder: &mut ExtractionBuilder<'_, '_>,
    target: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    if !matches!(
        target.kind(),
        "identifier"
            | "field_identifier"
            | "qualified_identifier"
            | "field_expression"
            | "template_function"
            | "operator_name"
            | "destructor_name"
    ) {
        return Ok(None);
    }
    for node in descendants_including_root(target) {
        builder.context.ensure_active()?;
        let kind = node.kind();
        if kind.contains("string")
            || kind.contains("character")
            || kind.contains("number_literal")
            || kind.contains("raw_literal")
        {
            return Ok(None);
        }
    }
    let raw = builder.context.text(target).trim();
    if raw.is_empty()
        || raw.len() > MAX_REFERENCE_TARGET_BYTES
        || raw.bytes().any(|byte| matches!(byte, b'\'' | b'"' | b'`'))
    {
        return Ok(None);
    }
    normalize_call_target(builder, raw).map(Some)
}

fn normalize_call_target(
    builder: &ExtractionBuilder<'_, '_>,
    raw: &str,
) -> Result<String, ExtractError> {
    let without_self = raw
        .strip_prefix("this->")
        .or_else(|| raw.strip_prefix("this."))
        .unwrap_or(raw);
    if !without_self.contains("->") {
        return builder.context.copy_text(without_self);
    }
    builder
        .context
        .budget
        .ensure_string_length(without_self.len())?;
    let mut normalized = String::new();
    normalized
        .try_reserve(without_self.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    let mut characters = without_self.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '-' && characters.peek() == Some(&'>') {
            characters.next();
            normalized.push('.');
        } else {
            normalized.push(character);
        }
    }
    Ok(normalized)
}

fn declarator_name(mut node: Node<'_>) -> Result<Option<DeclaratorName<'_>>, ExtractError> {
    for _ in 0..=MAX_DECLARATOR_DEPTH {
        if !node_has_durable_span(node) {
            return Ok(None);
        }
        match node.kind() {
            "identifier" | "field_identifier" | "type_identifier" | "operator_name"
            | "destructor_name" => {
                return Ok(Some(DeclaratorName { node, scope: None }));
            }
            "qualified_identifier" => {
                let Some(raw_name) = node.child_by_field_name("name") else {
                    return Ok(None);
                };
                let name = if declarator_shape(raw_name.kind()) {
                    let Some(name) = declarator_name(raw_name)? else {
                        return Ok(None);
                    };
                    name.node
                } else {
                    descendants_including_root(raw_name)
                        .find(|candidate| {
                            node_has_durable_span(*candidate)
                                && matches!(
                                    candidate.kind(),
                                    "identifier" | "field_identifier" | "type_identifier"
                                )
                        })
                        .unwrap_or(raw_name)
                };
                if !node_has_durable_span(name) {
                    return Ok(None);
                }
                return Ok(Some(DeclaratorName {
                    node: name,
                    scope: Some(node),
                }));
            }
            _ => {}
        }
        let Some(next) = node.child_by_field_name("declarator").or_else(|| {
            children(node).find(|child| {
                !matches!(
                    child.kind(),
                    "parameter_list" | "argument_list" | "initializer_list"
                ) && declarator_shape(child.kind())
            })
        }) else {
            return Ok(None);
        };
        node = next;
    }
    Err(ExtractError::NestingLimit)
}

fn node_has_durable_span(node: Node<'_>) -> bool {
    !node.is_missing() && node.start_byte() < node.end_byte()
}

fn owned_declarator_scope(
    builder: &ExtractionBuilder<'_, '_>,
    qualified: Node<'_>,
    name: Node<'_>,
) -> Result<String, ExtractError> {
    let Some(raw) = builder
        .context
        .source()
        .get(qualified.start_byte()..name.start_byte())
    else {
        return Err(ExtractError::InvalidSpan);
    };
    let scope = raw.trim().trim_end_matches(':').trim();
    builder.context.copy_text(scope)
}

fn declarator_shape(kind: &str) -> bool {
    kind.contains("declarator")
        || matches!(
            kind,
            "identifier"
                | "field_identifier"
                | "type_identifier"
                | "qualified_identifier"
                | "operator_name"
                | "destructor_name"
        )
}

fn find_scope_symbol(
    builder: &ExtractionBuilder<'_, '_>,
    scope: &str,
) -> Result<Option<(SymbolId, SymbolKind)>, ExtractError> {
    if let Some(candidate) = builder.native_scope_symbols.get(scope) {
        return Ok(candidate.clone());
    }
    if !scope.contains("::") && !builder.qualifiers.is_empty() {
        let prefix_length = builder
            .qualifiers
            .iter()
            .try_fold(scope.len(), |length, qualifier| {
                length
                    .checked_add(qualifier.len())
                    .and_then(|length| length.checked_add(2))
            })
            .ok_or(ExtractError::OutputLimit)?;
        builder.context.budget.ensure_string_length(prefix_length)?;
        let mut qualified = String::new();
        qualified
            .try_reserve(prefix_length)
            .map_err(|_| ExtractError::OutputLimit)?;
        for qualifier in &builder.qualifiers {
            qualified.push_str(qualifier);
            qualified.push_str("::");
        }
        qualified.push_str(scope);
        if let Some(candidate) = builder.native_scope_symbols.get(&qualified) {
            return Ok(candidate.clone());
        }
    }
    Ok(builder
        .native_scope_symbols
        .get(terminal_scope_name(scope))
        .cloned()
        .flatten())
}

fn register_scope_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ScopeSymbolInput<'_>,
) -> Result<(), ExtractError> {
    if !builder
        .native_scope_symbols
        .contains_key(input.qualified_name)
    {
        builder
            .native_scope_symbols
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        let key = builder.context.copy_text(input.qualified_name)?;
        builder
            .native_scope_symbols
            .insert(key, Some((input.id.clone(), input.kind)));
    }
    if input.qualified_name != input.name {
        if let Some(slot) = builder.native_scope_symbols.get_mut(input.name) {
            if slot
                .as_ref()
                .is_some_and(|(candidate, _)| candidate != input.id)
            {
                *slot = None;
            }
        } else {
            builder
                .native_scope_symbols
                .try_reserve(1)
                .map_err(|_| ExtractError::OutputLimit)?;
            let key = builder.context.copy_text(input.name)?;
            builder
                .native_scope_symbols
                .insert(key, Some((input.id.clone(), input.kind)));
        }
    }
    Ok(())
}

fn relative_scope_qualifier(
    builder: &ExtractionBuilder<'_, '_>,
    scope: &str,
) -> Result<String, ExtractError> {
    if builder.qualifiers.is_empty() {
        return builder.context.copy_text(scope);
    }
    let prefix_length = builder
        .qualifiers
        .iter()
        .try_fold(0_usize, |length, qualifier| {
            length
                .checked_add(qualifier.len())
                .and_then(|length| length.checked_add(2))
        });
    let prefix_length = prefix_length.ok_or(ExtractError::OutputLimit)?;
    builder.context.budget.ensure_string_length(prefix_length)?;
    let mut prefix = String::new();
    prefix
        .try_reserve(prefix_length)
        .map_err(|_| ExtractError::OutputLimit)?;
    for qualifier in &builder.qualifiers {
        prefix.push_str(qualifier);
        prefix.push_str("::");
    }
    let relative = scope.strip_prefix(&prefix).unwrap_or(scope);
    builder.context.copy_text(relative)
}

fn inferred_scope_kind(scope: &str) -> SymbolKind {
    terminal_scope_name(scope)
        .chars()
        .next()
        .filter(char::is_ascii_uppercase)
        .map_or(SymbolKind::Namespace, |_| SymbolKind::Class)
}

fn terminal_scope_name(scope: &str) -> &str {
    scope.rsplit("::").next().unwrap_or(scope)
}

fn is_control_keyword(name: &str) -> bool {
    matches!(
        name,
        "switch" | "if" | "for" | "while" | "do" | "case" | "return"
    ) || name.starts_with("namespace")
}

fn visit_access_specifier(builder: &mut ExtractionBuilder<'_, '_>, node: Node<'_>) {
    if builder.context.snapshot.language() != SourceLanguage::Cpp {
        return;
    }
    let visibility = match builder.context.text(node).trim().trim_end_matches(':') {
        "public" => Some(Visibility::Public),
        "private" => Some(Visibility::Private),
        "protected" => Some(Visibility::Protected),
        _ => None,
    };
    if let Some(slot) = builder.native_visibilities.last_mut()
        && visibility.is_some()
    {
        *slot = visibility;
    }
}

fn cxx_visibility(builder: &ExtractionBuilder<'_, '_>) -> Option<Visibility> {
    (builder.context.snapshot.language() == SourceLanguage::Cpp)
        .then(|| builder.native_visibilities.last().copied().flatten())
        .flatten()
}

fn default_cxx_visibility(
    builder: &ExtractionBuilder<'_, '_>,
    kind: SymbolKind,
) -> Option<Visibility> {
    if builder.context.snapshot.language() != SourceLanguage::Cpp {
        return None;
    }
    match kind {
        SymbolKind::Class => Some(Visibility::Private),
        SymbolKind::Struct | SymbolKind::Union => Some(Visibility::Public),
        _ => None,
    }
}

fn has_external_linkage(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> bool {
    if inside_anonymous_namespace(node) {
        return false;
    }
    current_owner_kind_in(builder, C_TYPE_OWNER_KINDS)
        || !declaration_has_storage(builder, node, "static")
}

fn declaration_has_storage(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    expected: &str,
) -> bool {
    named_children(node).any(|child| {
        child.kind() == "storage_class_specifier" && builder.context.text(child).trim() == expected
    })
}

fn inside_anonymous_namespace(mut node: Node<'_>) -> bool {
    for _ in 0..=MAX_DECLARATOR_DEPTH {
        let Some(parent) = node.parent() else {
            return false;
        };
        if parent.kind() == "namespace_definition" && parent.child_by_field_name("name").is_none() {
            return true;
        }
        node = parent;
    }
    true
}
