use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::{
    ReferenceKind, SourceLanguage, SymbolId, SymbolKind, declaration_value_is_search_safe,
};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedReference};

use super::{AstVisitBudget, ExtractionBuilder, PendingSymbol, owner_for_node, syntax::span_for};

const MAX_SCHEMA_AST_DEPTH: usize = 256;
const MAX_SCHEMA_CANDIDATES: usize = 1_024;
const MAX_FIELDS_PER_SCHEMA: usize = 512;
const MAX_ENUM_MEMBERS_PER_FIELD: usize = 128;
const MAX_ZOD_NESTING: usize = 8;
const MAX_SCHEMA_NAME_BYTES: usize = 512;
/// Longest literal word with non-ASCII letters admitted as part of a name.
const MAX_UNICODE_LITERAL_WORD_CHARS: usize = 24;
/// Authorization schemes whose following word is a credential (`Bearer
/// <token>`); a multi-word literal containing one never becomes a name.
const AUTHORIZATION_SCHEME_WORDS: &[&str] = &["bearer"];
/// Provider token prefixes that never become part of a literal-derived name.
const CREDENTIAL_PREFIXES: &[&str] = &[
    "sk_live_",
    "sk_test_",
    "ghp_",
    "github_pat_",
    "xoxb_",
    "xoxp_",
    "akia",
    "asia",
];

#[derive(Default)]
struct ScanBudget {
    visits: AstVisitBudget<MAX_SCHEMA_AST_DEPTH>,
    candidates: usize,
}

impl ScanBudget {
    fn admit_candidate(&mut self) -> Result<(), ExtractError> {
        self.candidates = self
            .candidates
            .checked_add(1)
            .ok_or(ExtractError::OutputLimit)?;
        if self.candidates > MAX_SCHEMA_CANDIDATES {
            return Err(ExtractError::OutputLimit);
        }
        Ok(())
    }
}

#[derive(Default)]
struct ZodSchemas {
    fields: BTreeMap<String, Option<BTreeSet<String>>>,
}

struct ZodSchemaInput<'tree, 'scan> {
    span_node: Node<'tree>,
    structural_node: Node<'tree>,
    object: Node<'tree>,
    name: &'scan str,
    nested_depth: usize,
    budget: &'scan mut ScanBudget,
    consumed_objects: &'scan mut BTreeSet<(usize, usize)>,
}

#[derive(Clone, Copy)]
struct SchemaWalk<'tree> {
    node: Node<'tree>,
    depth: usize,
}

struct ZodDeclarationScan<'scan> {
    budget: &'scan mut ScanBudget,
    consumed_objects: &'scan mut BTreeSet<(usize, usize)>,
    schemas: &'scan mut ZodSchemas,
}

struct ZodInlineScan<'scan> {
    budget: &'scan mut ScanBudget,
    consumed_objects: &'scan mut BTreeSet<(usize, usize)>,
}

struct ZodConsumerScan<'scan> {
    budget: &'scan mut ScanBudget,
    schemas: &'scan ZodSchemas,
}

struct ZodFieldsInput<'tree, 'scan> {
    object: Node<'tree>,
    nested_depth: usize,
    budget: &'scan mut ScanBudget,
    consumed_objects: &'scan mut BTreeSet<(usize, usize)>,
}

#[derive(Clone, Copy)]
struct SchemaFieldInput<'tree, 'field> {
    node: Node<'tree>,
    owner: &'field SymbolId,
    name: &'field str,
}

#[derive(Clone, Copy)]
struct SchemaSymbolInput<'tree, 'text> {
    kind: SymbolKind,
    name: &'text str,
    span_node: Node<'tree>,
    structural_node: Node<'tree>,
    signature: Option<&'text str>,
}

struct SchemaReferenceInput<'tree, 'text> {
    node: Node<'tree>,
    name: &'text str,
    resolution_name: Option<String>,
    kind: ReferenceKind,
}

#[derive(Clone, Copy)]
struct SchemaScope<'scope> {
    owner: &'scope SymbolId,
    kind: SymbolKind,
    qualifier: Option<&'scope str>,
}

impl ZodSchemas {
    fn insert(&mut self, name: String, fields: BTreeSet<String>) {
        match self.fields.entry(name) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Some(fields));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.insert(None);
            }
        }
    }

    fn unique_fields(&self, name: &str) -> Option<&BTreeSet<String>> {
        self.fields.get(name)?.as_ref()
    }
}

pub(super) fn enrich(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    match builder.context.snapshot.language() {
        SourceLanguage::TypeScript
        | SourceLanguage::Tsx
        | SourceLanguage::JavaScript
        | SourceLanguage::Jsx
            if imports_package(builder, "zod") =>
        {
            enrich_zod(builder, root)?;
        }
        SourceLanguage::Python if imports_package(builder, "pydantic") => {
            enrich_pydantic(builder, root)?;
        }
        _ => {}
    }
    Ok(())
}

fn imports_package(builder: &ExtractionBuilder<'_, '_>, package: &str) -> bool {
    if builder.facts.import_bindings.iter().any(|binding| {
        binding.module_specifier == package
            || binding
                .module_specifier
                .strip_prefix(package)
                .is_some_and(|suffix| suffix.starts_with(['/', '.']))
    }) {
        return true;
    }
    if package != "pydantic" {
        return false;
    }
    builder.context.source().lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("import pydantic") || line.starts_with("from pydantic import")
    })
}

fn enrich_zod(builder: &mut ExtractionBuilder<'_, '_>, root: Node<'_>) -> Result<(), ExtractError> {
    let mut budget = ScanBudget::default();
    let mut consumed_objects = BTreeSet::new();
    let mut schemas = ZodSchemas::default();
    scan_zod_declarations(
        builder,
        SchemaWalk {
            node: root,
            depth: 0,
        },
        &mut ZodDeclarationScan {
            budget: &mut budget,
            consumed_objects: &mut consumed_objects,
            schemas: &mut schemas,
        },
    )?;
    scan_zod_inline(
        builder,
        SchemaWalk {
            node: root,
            depth: 0,
        },
        &mut ZodInlineScan {
            budget: &mut budget,
            consumed_objects: &mut consumed_objects,
        },
    )?;
    if !schemas.fields.is_empty() {
        scan_zod_consumers(
            builder,
            SchemaWalk {
                node: root,
                depth: 0,
            },
            &mut ZodConsumerScan {
                budget: &mut budget,
                schemas: &schemas,
            },
        )?;
    }
    Ok(())
}

fn scan_zod_declarations(
    builder: &mut ExtractionBuilder<'_, '_>,
    walk: SchemaWalk<'_>,
    scan: &mut ZodDeclarationScan<'_>,
) -> Result<(), ExtractError> {
    scan.budget.visits.observe(builder, walk.depth)?;
    if walk.node.kind() == "variable_declarator" {
        let name_node = walk.node.child_by_field_name("name");
        let value_node = walk.node.child_by_field_name("value");
        if let (Some(name_node), Some(value_node)) = (name_node, value_node)
            && name_node.kind() == "identifier"
            && let Some(call) = find_zod_call(value_node, "object", builder.context.source())
            && let Some(object) = first_named_argument(call)
            && object.kind() == "object"
        {
            scan.budget.admit_candidate()?;
            let name = builder.context.owned_text(name_node)?;
            if safe_identifier_name(&name) {
                scan.consumed_objects
                    .insert((object.start_byte(), object.end_byte()));
                let (struct_id, fields) = emit_zod_schema(
                    builder,
                    ZodSchemaInput {
                        span_node: walk.node,
                        structural_node: value_node,
                        object,
                        name: &name,
                        nested_depth: 0,
                        budget: scan.budget,
                        consumed_objects: scan.consumed_objects,
                    },
                )?;
                let _ = struct_id;
                scan.schemas.insert(name, fields);
            }
        }
    }
    for child in super::syntax::named_children(walk.node) {
        scan_zod_declarations(
            builder,
            SchemaWalk {
                node: child,
                depth: walk.depth.saturating_add(1),
            },
            scan,
        )?;
    }
    Ok(())
}

fn emit_zod_schema(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ZodSchemaInput<'_, '_>,
) -> Result<(SymbolId, BTreeSet<String>), ExtractError> {
    let ZodSchemaInput {
        span_node,
        structural_node,
        object,
        name,
        nested_depth,
        budget,
        consumed_objects,
    } = input;
    let struct_id = emit_schema_symbol(
        builder,
        SchemaSymbolInput {
            kind: SymbolKind::Struct,
            name,
            span_node,
            structural_node,
            signature: Some("z.object"),
        },
    )?;
    let fields = with_scope(
        builder,
        SchemaScope {
            owner: &struct_id,
            kind: SymbolKind::Struct,
            qualifier: Some(name),
        },
        |builder| {
            emit_zod_fields(
                builder,
                ZodFieldsInput {
                    object,
                    nested_depth,
                    budget,
                    consumed_objects,
                },
            )
        },
    )?;
    Ok((struct_id, fields))
}

fn emit_zod_fields(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ZodFieldsInput<'_, '_>,
) -> Result<BTreeSet<String>, ExtractError> {
    let ZodFieldsInput {
        object,
        nested_depth,
        budget,
        consumed_objects,
    } = input;
    let mut fields = BTreeSet::new();
    for pair in super::syntax::named_children(object) {
        emit_zod_field(
            builder,
            ZodFieldInput {
                pair,
                nested_depth,
                budget,
                consumed_objects,
                fields: &mut fields,
            },
        )?;
    }
    Ok(fields)
}

struct ZodFieldInput<'tree, 'input> {
    pair: Node<'tree>,
    nested_depth: usize,
    budget: &'input mut ScanBudget,
    consumed_objects: &'input mut BTreeSet<(usize, usize)>,
    fields: &'input mut BTreeSet<String>,
}

fn emit_zod_field(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ZodFieldInput<'_, '_>,
) -> Result<(), ExtractError> {
    let ZodFieldInput {
        pair,
        nested_depth,
        budget,
        consumed_objects,
        fields,
    } = input;
    if pair.kind() != "pair" {
        return Ok(());
    }
    if fields.len() >= MAX_FIELDS_PER_SCHEMA {
        return Err(ExtractError::OutputLimit);
    }
    let Some(key) = pair.child_by_field_name("key") else {
        return Ok(());
    };
    let Some(value) = pair.child_by_field_name("value") else {
        return Ok(());
    };
    let field_name = builder.context.owned_unquoted_text(key)?;
    if !safe_object_key_name(key, &field_name) || !fields.insert(field_name.clone()) {
        return Ok(());
    }
    let leaf = zod_leaf_type(value, builder.context.source());
    let signature = leaf.as_deref().map(|leaf| format!("z.{leaf}"));
    let field_id = emit_schema_symbol(
        builder,
        SchemaSymbolInput {
            kind: SymbolKind::Field,
            name: &field_name,
            span_node: pair,
            structural_node: pair,
            signature: signature.as_deref(),
        },
    )?;
    match leaf.as_deref() {
        Some("enum") => emit_zod_enum_members(
            builder,
            SchemaFieldInput {
                node: value,
                owner: &field_id,
                name: &field_name,
            },
        ),
        Some("object") if nested_depth < MAX_ZOD_NESTING => emit_nested_zod_object(
            builder,
            NestedZodInput {
                pair,
                value,
                field_id: &field_id,
                field_name: &field_name,
                nested_depth,
                budget,
                consumed_objects,
            },
        ),
        _ => Ok(()),
    }
}

struct NestedZodInput<'tree, 'input> {
    pair: Node<'tree>,
    value: Node<'tree>,
    field_id: &'input SymbolId,
    field_name: &'input str,
    nested_depth: usize,
    budget: &'input mut ScanBudget,
    consumed_objects: &'input mut BTreeSet<(usize, usize)>,
}

fn emit_nested_zod_object(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: NestedZodInput<'_, '_>,
) -> Result<(), ExtractError> {
    let NestedZodInput {
        pair,
        value,
        field_id,
        field_name,
        nested_depth,
        budget,
        consumed_objects,
    } = input;
    let Some(nested) = find_zod_call(value, "object", builder.context.source())
        .and_then(first_named_argument)
        .filter(|candidate| candidate.kind() == "object")
    else {
        return Ok(());
    };
    let key = (nested.start_byte(), nested.end_byte());
    if !consumed_objects.insert(key) {
        return Ok(());
    }
    budget.admit_candidate()?;
    with_scope(
        builder,
        SchemaScope {
            owner: field_id,
            kind: SymbolKind::Field,
            qualifier: None,
        },
        |builder| {
            emit_zod_schema(
                builder,
                ZodSchemaInput {
                    span_node: pair,
                    structural_node: value,
                    object: nested,
                    name: field_name,
                    nested_depth: nested_depth.saturating_add(1),
                    budget,
                    consumed_objects,
                },
            )?;
            Ok(())
        },
    )
}

fn emit_zod_enum_members(
    builder: &mut ExtractionBuilder<'_, '_>,
    field: SchemaFieldInput<'_, '_>,
) -> Result<(), ExtractError> {
    let Some(array) = find_zod_call(field.node, "enum", builder.context.source())
        .and_then(first_named_argument)
        .filter(|argument| argument.kind() == "array")
    else {
        return Ok(());
    };
    let mut seen = BTreeSet::new();
    with_scope(
        builder,
        SchemaScope {
            owner: field.owner,
            kind: SymbolKind::Field,
            qualifier: Some(field.name),
        },
        |builder| {
            for element in super::syntax::named_children(array) {
                if !matches!(element.kind(), "string" | "string_fragment") {
                    continue;
                }
                if seen.len() >= MAX_ENUM_MEMBERS_PER_FIELD {
                    return Err(ExtractError::OutputLimit);
                }
                let member = builder.context.owned_unquoted_text(element)?;
                if !safe_literal_name(&member) || !seen.insert(member.clone()) {
                    continue;
                }
                emit_schema_symbol(
                    builder,
                    SchemaSymbolInput {
                        kind: SymbolKind::EnumMember,
                        name: &member,
                        span_node: element,
                        structural_node: element,
                        signature: None,
                    },
                )?;
            }
            Ok(())
        },
    )
}

fn scan_zod_inline(
    builder: &mut ExtractionBuilder<'_, '_>,
    walk: SchemaWalk<'_>,
    scan: &mut ZodInlineScan<'_>,
) -> Result<(), ExtractError> {
    scan.budget.visits.observe(builder, walk.depth)?;
    if walk.node.kind() == "call_expression"
        && direct_zod_method(walk.node, "object", builder.context.source())
        && let Some(object) =
            first_named_argument(walk.node).filter(|argument| argument.kind() == "object")
    {
        let object_key = (object.start_byte(), object.end_byte());
        if !scan.consumed_objects.contains(&object_key)
            && let Some((name_node, name)) = inline_zod_schema_name(builder, walk.node)?
        {
            scan.budget.admit_candidate()?;
            scan.consumed_objects.insert(object_key);
            let _ = emit_zod_schema(
                builder,
                ZodSchemaInput {
                    span_node: name_node,
                    structural_node: walk.node,
                    object,
                    name: &name,
                    nested_depth: 0,
                    budget: scan.budget,
                    consumed_objects: scan.consumed_objects,
                },
            )?;
        }
    }
    for child in super::syntax::named_children(walk.node) {
        scan_zod_inline(
            builder,
            SchemaWalk {
                node: child,
                depth: walk.depth.saturating_add(1),
            },
            scan,
        )?;
    }
    Ok(())
}

fn inline_zod_schema_name<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    call: Node<'tree>,
) -> Result<Option<(Node<'tree>, String)>, ExtractError> {
    let mut current = call.parent();
    let mut depth = 0_usize;
    while let Some(node) = current {
        if depth > 32 {
            return Ok(None);
        }
        if node.kind() == "pair" {
            let Some(key) = node.child_by_field_name("key") else {
                return Ok(None);
            };
            let name = builder.context.owned_unquoted_text(key)?;
            return Ok(safe_object_key_name(key, &name).then_some((node, name)));
        }
        if node.kind() == "variable_declarator" {
            return Ok(None);
        }
        if !matches!(
            node.kind(),
            "arguments"
                | "call_expression"
                | "member_expression"
                | "object"
                | "parenthesized_expression"
        ) {
            return Ok(None);
        }
        current = node.parent();
        depth = depth.saturating_add(1);
    }
    Ok(None)
}

fn scan_zod_consumers(
    builder: &mut ExtractionBuilder<'_, '_>,
    walk: SchemaWalk<'_>,
    scan: &mut ZodConsumerScan<'_>,
) -> Result<(), ExtractError> {
    scan.budget.visits.observe(builder, walk.depth)?;
    if matches!(walk.node.kind(), "generic_type" | "type_arguments")
        && builder.context.text(walk.node).contains("z.infer")
        && let Some(schema_node) = typeof_identifier(walk.node, builder.context.source())
    {
        let schema = builder.context.owned_text(schema_node)?;
        if scan.schemas.unique_fields(&schema).is_some() {
            emit_schema_reference(
                builder,
                SchemaReferenceInput {
                    node: schema_node,
                    name: &schema,
                    resolution_name: None,
                    kind: ReferenceKind::TypeOf,
                },
            )?;
        }
    }
    if walk.node.kind() == "member_expression"
        && let Some((schema_node, field_node)) =
            zod_shape_reference(walk.node, builder.context.source())
    {
        let schema = builder.context.owned_text(schema_node)?;
        let field = builder.context.owned_text(field_node)?;
        if scan
            .schemas
            .unique_fields(&schema)
            .is_some_and(|fields| fields.contains(&field))
        {
            let resolution_name = format!("{schema}::{field}");
            emit_schema_reference(
                builder,
                SchemaReferenceInput {
                    node: field_node,
                    name: &field,
                    resolution_name: Some(resolution_name),
                    kind: ReferenceKind::References,
                },
            )?;
        }
    }
    if walk.node.kind() == "call_expression" {
        emit_zod_selection_references(builder, walk.node, scan.schemas)?;
    }
    for child in super::syntax::named_children(walk.node) {
        scan_zod_consumers(
            builder,
            SchemaWalk {
                node: child,
                depth: walk.depth.saturating_add(1),
            },
            scan,
        )?;
    }
    Ok(())
}

fn emit_zod_selection_references(
    builder: &mut ExtractionBuilder<'_, '_>,
    call: Node<'_>,
    schemas: &ZodSchemas,
) -> Result<(), ExtractError> {
    let Some(function) = call.child_by_field_name("function") else {
        return Ok(());
    };
    if function.kind() != "member_expression" {
        return Ok(());
    }
    let Some(schema_node) = function.child_by_field_name("object") else {
        return Ok(());
    };
    let Some(method_node) = function.child_by_field_name("property") else {
        return Ok(());
    };
    if schema_node.kind() != "identifier"
        || !matches!(builder.context.text(method_node), "pick" | "omit")
    {
        return Ok(());
    }
    let schema = builder.context.owned_text(schema_node)?;
    let Some(fields) = schemas.unique_fields(&schema) else {
        return Ok(());
    };
    let Some(selection) = first_named_argument(call).filter(|argument| argument.kind() == "object")
    else {
        return Ok(());
    };
    for pair in super::syntax::named_children(selection) {
        let Some(key) = pair.child_by_field_name("key") else {
            continue;
        };
        let field = builder.context.owned_unquoted_text(key)?;
        if !fields.contains(&field) {
            continue;
        }
        let resolution_name = format!("{schema}::{field}");
        emit_schema_reference(
            builder,
            SchemaReferenceInput {
                node: key,
                name: &field,
                resolution_name: Some(resolution_name),
                kind: ReferenceKind::References,
            },
        )?;
    }
    Ok(())
}

fn emit_schema_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: SchemaReferenceInput<'_, '_>,
) -> Result<(), ExtractError> {
    builder.emit_reference(ExtractedReference {
        owner: owner_for_node(builder, input.node),
        name: input.name.to_owned(),
        resolution_name: input.resolution_name,
        kind: input.kind,
        span: span_for(input.node)?,
    })
}

fn enrich_pydantic(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    let mut budget = ScanBudget::default();
    scan_pydantic_models(
        builder,
        SchemaWalk {
            node: root,
            depth: 0,
        },
        &mut budget,
    )
}

fn scan_pydantic_models(
    builder: &mut ExtractionBuilder<'_, '_>,
    walk: SchemaWalk<'_>,
    budget: &mut ScanBudget,
) -> Result<(), ExtractError> {
    budget.visits.observe(builder, walk.depth)?;
    if walk.node.kind() == "class_definition"
        && is_pydantic_model(walk.node, builder.context.source())
    {
        budget.admit_candidate()?;
        emit_pydantic_model(builder, walk.node)?;
    }
    for child in super::syntax::named_children(walk.node) {
        scan_pydantic_models(
            builder,
            SchemaWalk {
                node: child,
                depth: walk.depth.saturating_add(1),
            },
            budget,
        )?;
    }
    Ok(())
}

fn is_pydantic_model(class_definition: Node<'_>, source: &str) -> bool {
    let Some(superclasses) = class_definition.child_by_field_name("superclasses") else {
        return false;
    };
    descendants(superclasses, 0).any(|node| {
        matches!(node.kind(), "identifier" | "attribute")
            && matches!(
                source
                    .get(node.start_byte()..node.end_byte())
                    .unwrap_or_default()
                    .rsplit('.')
                    .next(),
                Some("BaseModel" | "BaseSettings")
            )
    })
}

fn emit_pydantic_model(
    builder: &mut ExtractionBuilder<'_, '_>,
    class_definition: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = class_definition.child_by_field_name("name") else {
        return Ok(());
    };
    let Some(body) = class_definition.child_by_field_name("body") else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    if !safe_identifier_name(&name) {
        return Ok(());
    }
    let struct_id = emit_schema_symbol(
        builder,
        SchemaSymbolInput {
            kind: SymbolKind::Struct,
            name: &name,
            span_node: class_definition,
            structural_node: class_definition,
            signature: Some("pydantic.BaseModel"),
        },
    )?;
    with_scope(
        builder,
        SchemaScope {
            owner: &struct_id,
            kind: SymbolKind::Struct,
            qualifier: Some(&name),
        },
        |builder| {
            let mut fields = BTreeSet::new();
            for statement in super::syntax::named_children(body) {
                if fields.len() >= MAX_FIELDS_PER_SCHEMA {
                    return Err(ExtractError::OutputLimit);
                }
                let Some(assignment) = python_assignment(statement) else {
                    continue;
                };
                let Some(left) = assignment.child_by_field_name("left") else {
                    continue;
                };
                let Some(annotation) = assignment.child_by_field_name("type") else {
                    continue;
                };
                if left.kind() != "identifier" || is_class_var(annotation, builder.context.source())
                {
                    continue;
                }
                let field_name = builder.context.owned_text(left)?;
                if !safe_identifier_name(&field_name) || !fields.insert(field_name.clone()) {
                    continue;
                }
                let signature = safe_type_signature(builder, annotation)?;
                let field_id = emit_schema_symbol(
                    builder,
                    SchemaSymbolInput {
                        kind: SymbolKind::Field,
                        name: &field_name,
                        span_node: statement,
                        structural_node: assignment,
                        signature: signature.as_deref(),
                    },
                )?;
                emit_pydantic_literals(
                    builder,
                    SchemaFieldInput {
                        node: annotation,
                        owner: &field_id,
                        name: &field_name,
                    },
                )?;
            }
            Ok(())
        },
    )
}

fn python_assignment(statement: Node<'_>) -> Option<Node<'_>> {
    if statement.kind() == "assignment" {
        return Some(statement);
    }
    if statement.kind() != "expression_statement" {
        return None;
    }
    super::syntax::named_children(statement).find(|child| child.kind() == "assignment")
}

fn is_class_var(annotation: Node<'_>, source: &str) -> bool {
    let raw = source
        .get(annotation.start_byte()..annotation.end_byte())
        .unwrap_or_default()
        .trim();
    raw.starts_with("ClassVar[") || raw.starts_with("typing.ClassVar[")
}

fn safe_type_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    annotation: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let raw = builder.context.owned_text(annotation)?;
    if declaration_value_is_search_safe(&raw)
        && !raw.bytes().any(|byte| matches!(byte, b'\'' | b'"'))
    {
        return Ok(Some(raw));
    }
    let head = raw
        .split(['[', '<', '(', ' ', '\t', '\r', '\n'])
        .next()
        .unwrap_or_default();
    if !safe_identifier_name(head.rsplit('.').next().unwrap_or_default()) {
        return Ok(None);
    }
    Ok(Some(format!("{head}[...]")))
}

fn emit_pydantic_literals(
    builder: &mut ExtractionBuilder<'_, '_>,
    field: SchemaFieldInput<'_, '_>,
) -> Result<(), ExtractError> {
    let Some(literal) = descendants(field.node, 0).find(|node| {
        if node.kind() != "generic_type" {
            return false;
        }
        super::syntax::named_children(*node)
            .next()
            .is_some_and(|head| {
                builder
                    .context
                    .text(head)
                    .rsplit('.')
                    .next()
                    .is_some_and(|name| name == "Literal")
            })
    }) else {
        return Ok(());
    };
    let mut members = BTreeSet::new();
    with_scope(
        builder,
        SchemaScope {
            owner: field.owner,
            kind: SymbolKind::Field,
            qualifier: Some(field.name),
        },
        |builder| emit_pydantic_literal_members(builder, literal, &mut members),
    )
}

fn emit_pydantic_literal_members(
    builder: &mut ExtractionBuilder<'_, '_>,
    literal: Node<'_>,
    members: &mut BTreeSet<String>,
) -> Result<(), ExtractError> {
    for node in descendants(literal, 0) {
        if !matches!(node.kind(), "string" | "string_content") {
            continue;
        }
        if node.kind() == "string" && super::syntax::named_children(node).next().is_some() {
            continue;
        }
        if members.len() >= MAX_ENUM_MEMBERS_PER_FIELD {
            return Err(ExtractError::OutputLimit);
        }
        let member = builder.context.owned_unquoted_text(node)?;
        if !safe_literal_name(&member) || !members.insert(member.clone()) {
            continue;
        }
        emit_schema_symbol(
            builder,
            SchemaSymbolInput {
                kind: SymbolKind::EnumMember,
                name: &member,
                span_node: node,
                structural_node: node,
                signature: None,
            },
        )?;
    }
    Ok(())
}

fn emit_schema_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: SchemaSymbolInput<'_, '_>,
) -> Result<SymbolId, ExtractError> {
    builder.emit_symbol(PendingSymbol {
        kind: input.kind,
        name: input.name.to_owned(),
        span_node: input.span_node,
        structural_node: input.structural_node,
        doc_anchor: input.span_node,
        body_node: None,
        declaration_only: false,
        signature: input.signature.map(str::to_owned),
        export: crate::SymbolExportFlags::new(false, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })
}

fn with_scope<T>(
    builder: &mut ExtractionBuilder<'_, '_>,
    scope: SchemaScope<'_>,
    operation: impl FnOnce(&mut ExtractionBuilder<'_, '_>) -> Result<T, ExtractError>,
) -> Result<T, ExtractError> {
    builder.owners.push(scope.owner.clone());
    builder.native_owner_kinds.push(scope.kind);
    if let Some(qualifier) = scope.qualifier {
        builder.qualifiers.push(qualifier.to_owned());
    }
    let result = operation(builder);
    if scope.qualifier.is_some() {
        builder.qualifiers.pop();
    }
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn find_zod_call<'tree>(value: Node<'tree>, method: &str, source: &str) -> Option<Node<'tree>> {
    let mut current = Some(value);
    for _ in 0..64 {
        let node = current?;
        match node.kind() {
            "call_expression" => {
                let function = node.child_by_field_name("function")?;
                if function.kind() == "member_expression" {
                    if direct_zod_member(function, method, source) {
                        return Some(node);
                    }
                    current = function.child_by_field_name("object");
                } else {
                    current = Some(function);
                }
            }
            "member_expression" => current = node.child_by_field_name("object"),
            _ => return None,
        }
    }
    None
}

fn direct_zod_method(call: Node<'_>, method: &str, source: &str) -> bool {
    call.child_by_field_name("function")
        .is_some_and(|function| direct_zod_member(function, method, source))
}

fn direct_zod_member(member: Node<'_>, method: &str, source: &str) -> bool {
    if member.kind() != "member_expression" {
        return false;
    }
    let Some(object) = member.child_by_field_name("object") else {
        return false;
    };
    let Some(property) = member.child_by_field_name("property") else {
        return false;
    };
    object.kind() == "identifier"
        && source
            .get(object.start_byte()..object.end_byte())
            .is_some_and(|name| name == "z")
        && source
            .get(property.start_byte()..property.end_byte())
            .is_some_and(|name| name == method)
}

fn first_named_argument(call: Node<'_>) -> Option<Node<'_>> {
    let arguments = call.child_by_field_name("arguments")?;
    super::syntax::named_children(arguments).next()
}

fn zod_leaf_type(value: Node<'_>, source: &str) -> Option<String> {
    let mut current = Some(value);
    for _ in 0..64 {
        let node = current?;
        if node.kind() == "call_expression" {
            let function = node.child_by_field_name("function")?;
            if function.kind() != "member_expression" {
                current = Some(function);
                continue;
            }
            let object = function.child_by_field_name("object")?;
            let property = function.child_by_field_name("property")?;
            if object.kind() != "identifier"
                || source
                    .get(object.start_byte()..object.end_byte())
                    .is_none_or(|name| name != "z")
            {
                current = Some(object);
                continue;
            }
            let method = source.get(property.start_byte()..property.end_byte())?;
            return method
                .bytes()
                .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
                .then(|| method.to_owned());
        }
        if node.kind() == "member_expression" {
            current = node.child_by_field_name("object");
            continue;
        }
        return None;
    }
    None
}

fn typeof_identifier<'tree>(node: Node<'tree>, source: &str) -> Option<Node<'tree>> {
    for candidate in descendants(node, 0) {
        if candidate.kind() != "type_query" {
            continue;
        }
        if let Some(identifier) =
            descendants(candidate, 0).find(|child| child.kind() == "identifier")
            && source
                .get(identifier.start_byte()..identifier.end_byte())
                .is_some_and(safe_identifier_name)
        {
            return Some(identifier);
        }
    }
    None
}

fn zod_shape_reference<'tree>(
    member: Node<'tree>,
    source: &str,
) -> Option<(Node<'tree>, Node<'tree>)> {
    let field = member.child_by_field_name("property")?;
    let shape = member.child_by_field_name("object")?;
    if shape.kind() != "member_expression" {
        return None;
    }
    let shape_property = shape.child_by_field_name("property")?;
    let schema = shape.child_by_field_name("object")?;
    if schema.kind() != "identifier"
        || source
            .get(shape_property.start_byte()..shape_property.end_byte())
            .is_none_or(|name| name != "shape")
    {
        return None;
    }
    Some((schema, field))
}

/// Whether a name copied from identifier syntax — a model, field, or schema
/// variable name, or an unquoted object key — may become a symbol name:
/// bounded, non-empty, and made of (Unicode) alphanumerics and the
/// separators `_ - . $`, so identifiers such as `naïve` survive.
fn safe_identifier_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SCHEMA_NAME_BYTES
        && value.chars().all(is_identifier_name_character)
}

/// Whether a name copied from a source string literal — a quoted key, an
/// enum value, a `Literal[...]` member, a quoted class field, a contract
/// property — may become a symbol name.
///
/// Besides the identifier characters it admits single inner spaces
/// (`"content type"`, `"c d"`) but no other punctuation, so a connection URI,
/// user info, or an e-mail address (`:`, `/`, `@`) never forms a name and no
/// name contains the `::` qualifier separator. Every word must be
/// search-safe — no credential word, provider token prefix, high-entropy or
/// numeric token — and a multi-word literal that names an authorization
/// scheme (`Bearer abc123`) is a credential.
pub(super) fn safe_literal_name(value: &str) -> bool {
    value.len() <= MAX_SCHEMA_NAME_BYTES
        && !super::specifier_safety::specifier_may_carry_credential(value)
        && value.trim() == value
        && !value.contains("  ")
        && value
            .chars()
            .any(|character| character.is_alphanumeric() || matches!(character, '_' | '$'))
        && value
            .chars()
            .all(|character| character == ' ' || is_identifier_name_character(character))
        && literal_words_are_search_safe(value)
}

/// Whether the name of an object key may become a symbol name.
///
/// An unquoted key is identifier syntax. A quoted key is a source literal:
/// an ASCII identifier-shaped one (`"content-type"`, `"200"`, `"password"`)
/// names a field exactly like the unquoted key, so it keeps that rule, but no
/// word of it may start with a provider token prefix and it may not pair an
/// authorization scheme with a credential (`"Bearer-abc123"`). Any other
/// quoted key (`"x y"`, `"café"`) must be a safe literal name.
fn safe_object_key_name(key: Node<'_>, name: &str) -> bool {
    if super::specifier_safety::specifier_may_carry_credential(name) {
        return false;
    }
    if key.kind() != "string" {
        return safe_identifier_name(name);
    }
    if name.is_ascii() && safe_identifier_name(name) {
        !literal_words(name).any(has_credential_prefix) && !names_authorization(name)
    } else {
        safe_literal_name(name)
    }
}

/// The words of a name: its runs of alphanumerics and underscores.
fn literal_words(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .filter(|word| !word.is_empty())
}

/// Whether a multi-word name names an authorization scheme, whose following
/// word is a credential (`Bearer abc123`).
fn names_authorization(value: &str) -> bool {
    literal_words(value).nth(1).is_some()
        && literal_words(value).any(|word| {
            AUTHORIZATION_SCHEME_WORDS
                .iter()
                .any(|scheme| word.eq_ignore_ascii_case(scheme))
        })
}

/// A (Unicode) alphanumeric or one of the separators `_ - . $`.
fn is_identifier_name_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '-' | '.' | '$')
}

/// Every word of a literal-derived name is search-safe, and the name does
/// not name an authorization scheme with its credential.
fn literal_words_are_search_safe(value: &str) -> bool {
    literal_words(value).all(literal_word_is_search_safe) && !names_authorization(value)
}

/// One literal word. Its ASCII letters must not start with a provider token
/// prefix and must pass the shared declaration-value classifier (credential
/// words, provider prefixes, high-entropy and numeric tokens), so interleaved
/// Unicode cannot hide a token. A word with non-ASCII letters must also be
/// short and numeral-free.
fn literal_word_is_search_safe(word: &str) -> bool {
    if word.is_ascii() {
        return !has_credential_prefix(word) && declaration_value_is_search_safe(word);
    }
    let ascii = word.chars().filter(char::is_ascii).collect::<String>();
    word.chars().count() <= MAX_UNICODE_LITERAL_WORD_CHARS
        && !word.chars().any(char::is_numeric)
        && !has_credential_prefix(&ascii)
        && (ascii.is_empty() || declaration_value_is_search_safe(&ascii))
}

/// Whether a word starts with a provider token prefix, ignoring ASCII case.
fn has_credential_prefix(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    CREDENTIAL_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

struct Descendants<'tree> {
    cursor: tree_sitter::TreeCursor<'tree>,
    started: bool,
    depth: usize,
    done: bool,
}

impl<'tree> Descendants<'tree> {
    fn new(root: Node<'tree>, depth: usize) -> Self {
        Self {
            cursor: root.walk(),
            started: false,
            depth,
            done: false,
        }
    }
}

impl<'tree> Iterator for Descendants<'tree> {
    type Item = Node<'tree>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if !self.started {
            self.started = true;
            return Some(self.cursor.node());
        }
        loop {
            if !self.advance_depth_first() {
                self.done = true;
                return None;
            }
            if self.cursor.node().is_named() {
                return Some(self.cursor.node());
            }
        }
    }
}

impl Descendants<'_> {
    fn advance_depth_first(&mut self) -> bool {
        if self.depth < MAX_SCHEMA_AST_DEPTH && self.cursor.goto_first_child() {
            self.depth = self.depth.saturating_add(1);
            return true;
        }
        loop {
            if self.cursor.goto_next_sibling() {
                return true;
            }
            if self.depth == 0 || !self.cursor.goto_parent() {
                return false;
            }
            self.depth = self.depth.saturating_sub(1);
        }
    }
}

fn descendants(root: Node<'_>, depth: usize) -> Descendants<'_> {
    Descendants::new(root, depth)
}
